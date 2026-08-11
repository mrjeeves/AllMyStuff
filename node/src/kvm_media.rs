//! KVM virtual-media staging.
//!
//! A KVM cannot consume a desktop-native mapped drive: BIOS/UEFI runs before
//! that desktop agent exists. Instead the source machine streams an ISO/image
//! (or the exact raw bytes of a removable Windows/macOS disk) through the KVM's mesh
//! site tunnel. The KVM stages it on `/data`, binds it to its Linux USB mass-
//! storage gadget, and re-enumerates USB at the attached computer.

use futures_util::{SinkExt, StreamExt};
use reqwest::multipart::{Form, Part};
#[cfg(target_os = "windows")]
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
#[cfg(target_os = "macos")]
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_tungstenite::tungstenite::Message;

const PRO_CHUNK: usize = 8 * 1024 * 1024;
const REMOTE_MEDIA_MAX_READ: usize = 1024 * 1024;

type RemoteMediaProvider = (String, tokio::sync::watch::Sender<bool>);
type RemoteMediaProviders = Mutex<HashMap<u16, RemoteMediaProvider>>;

static REMOTE_MEDIA_PROVIDERS: LazyLock<RemoteMediaProviders> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cancel_remote_provider(local_port: u16) {
    if let Some((_, stop)) = REMOTE_MEDIA_PROVIDERS
        .lock()
        .expect("remote-media provider registry poisoned")
        .remove(&local_port)
    {
        let _ = stop.send(true);
    }
}

struct Source {
    file: Option<tokio::fs::File>,
    len: u64,
    name: String,
    cdrom: bool,
    /// macOS may need an authorized `dd` writer feeding a private FIFO. ISO,
    /// IMG, Windows raw disks, and root-run macOS nodes leave these empty.
    worker: Option<tokio::task::JoinHandle<Result<(), String>>>,
    cleanup: Option<PathBuf>,
    #[cfg(target_os = "macos")]
    range: Option<MacRangeReader>,
}

fn safe_name(input: &str, fallback: &str) -> String {
    let out = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches(['.', '_'])
        .to_string();
    if out.is_empty() {
        fallback.into()
    } else {
        out
    }
}

fn image_name(path: &Path, label: &str, raw: bool) -> String {
    let fallback = if raw {
        "usb-drive.img"
    } else {
        "virtual-media.iso"
    };
    let mut name = if raw {
        format!("{}.img", safe_name(label, "usb-drive"))
    } else {
        safe_name(
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(fallback),
            fallback,
        )
    };
    let lower = name.to_ascii_lowercase();
    if !lower.ends_with(".iso") && !lower.ends_with(".img") {
        name.push_str(if raw { ".img" } else { ".iso" });
    }
    name
}

async fn open_source(path: &str, label: &str, lazy: bool) -> Result<Source, String> {
    let path = PathBuf::from(path)
        .canonicalize()
        .map_err(|error| format!("couldn't open that virtual-media source: {error}"))?;
    if path.is_file() {
        let name = image_name(&path, label, false);
        let lower = name.to_ascii_lowercase();
        if !lower.ends_with(".iso") && !lower.ends_with(".img") {
            return Err("choose an .iso or .img disk image".into());
        }
        let file = tokio::fs::File::open(&path)
            .await
            .map_err(|error| format!("couldn't read {}: {error}", path.display()))?;
        let len = file
            .metadata()
            .await
            .map_err(|error| format!("couldn't inspect {}: {error}", path.display()))?
            .len();
        return Ok(Source {
            file: Some(file),
            len,
            cdrom: lower.ends_with(".iso"),
            name,
            worker: None,
            cleanup: None,
            #[cfg(target_os = "macos")]
            range: None,
        });
    }
    if path.is_dir() {
        return open_removable_disk(&path, label, lazy).await;
    }
    Err("choose an .iso/.img file or the root of a removable USB drive".into())
}

/// The drive letter a Windows path names, if it names one.
///
/// The path reaching this has been through `canonicalize`, and on Windows that
/// returns an extended-length ("verbatim") path: `E:\` comes back as `\\?\E:\`.
/// The first byte is therefore a backslash, never a letter — so testing
/// `bytes[0].is_ascii_alphabetic()` against the canonical form rejected every
/// real drive root and told the operator to "choose the root of a drive",
/// which is exactly what they had just done.
///
/// Kept as string surgery rather than `std::path::Prefix` so it compiles and
/// unit-tests on any host: prefix parsing is Windows-only behaviour, so a
/// `Prefix`-based version could never be exercised by CI's Linux job.
#[cfg(any(target_os = "windows", test))]
fn windows_drive_letter(path: &str) -> Option<char> {
    // A UNC share — `\\?\UNC\server\share` or `\\server\share` — has no drive
    // letter and is not a local disk, so it keeps the caller's error.
    if path.starts_with(r"\\?\UNC\") || (path.starts_with(r"\\") && !path.starts_with(r"\\?\")) {
        return None;
    }
    let rest = path.strip_prefix(r"\\?\").unwrap_or(path);
    let mut chars = rest.chars();
    let letter = chars.next()?;
    if !letter.is_ascii_alphabetic() || chars.next()? != ':' {
        return None;
    }
    Some(letter.to_ascii_uppercase())
}

#[cfg(target_os = "windows")]
async fn open_removable_disk(path: &Path, label: &str, _lazy: bool) -> Result<Source, String> {
    let text = path.to_string_lossy();
    let Some(letter) = windows_drive_letter(&text) else {
        // Name what we actually got: the operator picked a drive root and was
        // told to pick a drive root, with nothing to act on in between.
        return Err(format!(
            "choose the root of a drive (for example E:\\) — {text} isn't one"
        ));
    };
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct DiskInfo {
        number: u32,
        size: u64,
        bus_type: String,
    }
    let script = format!(
        "$p=Get-Partition -DriveLetter '{letter}' -ErrorAction Stop; $d=$p|Get-Disk; [pscustomobject]@{{Number=$d.Number;Size=[uint64]$d.Size;BusType=[string]$d.BusType}}|ConvertTo-Json -Compress"
    );
    let output = tokio::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .await
        .map_err(|error| format!("couldn't inspect drive {letter}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "couldn't inspect drive {letter}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let disk: DiskInfo = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("couldn't identify drive {letter}: {error}"))?;
    if !disk.bus_type.eq_ignore_ascii_case("usb") {
        return Err(format!(
            "drive {letter}: is {}, not a removable USB disk",
            disk.bus_type
        ));
    }
    let raw = format!(r"\\.\PhysicalDrive{}", disk.number);
    let file = std::fs::File::open(&raw).map_err(|error| {
        format!(
            "couldn't read {letter}: as a bootable disk: {error}. Install/run the Always On service so AllMyStuff can read raw removable media"
        )
    })?;
    Ok(Source {
        file: Some(tokio::fs::File::from_std(file)),
        len: disk.size,
        name: image_name(path, label, true),
        cdrom: false,
        worker: None,
        cleanup: None,
    })
}

#[cfg(target_os = "macos")]
async fn diskutil_info(value: &str) -> Result<plist::Dictionary, String> {
    let output = tokio::process::Command::new("/usr/sbin/diskutil")
        .args(["info", "-plist", value])
        .output()
        .await
        .map_err(|error| format!("couldn't inspect removable disk {value}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "couldn't inspect removable disk {value}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    plist::Value::from_reader_xml(output.stdout.as_slice())
        .map_err(|error| format!("couldn't read disk information for {value}: {error}"))?
        .into_dictionary()
        .ok_or_else(|| format!("diskutil returned invalid disk information for {value}"))
}

#[cfg(target_os = "macos")]
fn plist_string<'a>(disk: &'a plist::Dictionary, key: &str) -> Option<&'a str> {
    disk.get(key).and_then(plist::Value::as_string)
}

#[cfg(target_os = "macos")]
fn plist_bool(disk: &plist::Dictionary, key: &str) -> bool {
    disk.get(key)
        .and_then(plist::Value::as_boolean)
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn plist_size(disk: &plist::Dictionary) -> Option<u64> {
    ["TotalSize", "Size", "IOKitSize"]
        .into_iter()
        .find_map(|key| disk.get(key).and_then(plist::Value::as_unsigned_integer))
        .filter(|size| *size > 0)
}

#[cfg(target_os = "macos")]
fn valid_whole_disk(value: &str) -> bool {
    value
        .strip_prefix("disk")
        .is_some_and(|number| !number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit()))
}

#[cfg(target_os = "macos")]
fn apple_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "macos")]
struct MacRangeReader {
    command: tokio::fs::File,
    data: tokio::fs::File,
    worker: tokio::task::JoinHandle<Result<(), String>>,
    directory: PathBuf,
}

#[cfg(target_os = "macos")]
impl MacRangeReader {
    async fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<(), String> {
        const BLOCK: u64 = 256 * 1024;
        if offset % BLOCK != 0 || output.is_empty() || output.len() > BLOCK as usize {
            return Err("KVM requested an unsupported raw-disk range".into());
        }
        if self.worker.is_finished() {
            return Err("administrator raw-disk helper stopped".into());
        }
        let request = format!("{} {}\n", offset / BLOCK, output.len());
        self.command
            .write_all(request.as_bytes())
            .await
            .map_err(|error| format!("couldn't request removable-disk bytes: {error}"))?;
        self.command
            .flush()
            .await
            .map_err(|error| format!("couldn't flush removable-disk request: {error}"))?;
        self.data
            .read_exact(output)
            .await
            .map_err(|error| format!("couldn't read removable-disk bytes: {error}"))?;
        Ok(())
    }

    async fn finish(self) {
        drop(self.command);
        drop(self.data);
        self.worker.abort();
        let _ = tokio::fs::remove_dir_all(self.directory).await;
    }
}

#[cfg(target_os = "macos")]
async fn authorized_range_reader(raw: &str) -> Result<MacRangeReader, String> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut random = [0u8; 8];
    getrandom::getrandom(&mut random)
        .map_err(|error| format!("couldn't prepare raw-disk helper: {error}"))?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let directory = std::env::temp_dir().join(format!("allmystuff-kvm-ranges-{suffix}"));
    tokio::fs::create_dir(&directory)
        .await
        .map_err(|error| format!("couldn't prepare raw-disk helper: {error}"))?;
    let command_path = directory.join("commands.pipe");
    let data_path = directory.join("data.pipe");
    let ready_path = directory.join("ready");
    for fifo in [&command_path, &data_path] {
        let status = tokio::process::Command::new("/usr/bin/mkfifo")
            .arg(fifo)
            .status()
            .await
            .map_err(|error| format!("couldn't create raw-disk pipe: {error}"))?;
        if !status.success() {
            let _ = tokio::fs::remove_dir_all(&directory).await;
            return Err("couldn't create raw-disk pipe".into());
        }
    }

    let quote = |path: &Path| path.to_string_lossy().replace('\'', "'\\''");
    let shell = format!(
        "exec 3<\'{}\'; exec 4>\'{}\'; /usr/bin/touch \'{}\'; while read block length <&3; do /bin/dd if=\'{}\' bs=262144 skip=\"$block\" count=1 2>/dev/null | /usr/bin/head -c \"$length\" >&4 || exit; done",
        quote(&command_path),
        quote(&data_path),
        quote(&ready_path),
        raw.replace('\'', "'\\''")
    );
    let script = format!(
        "do shell script \"{}\" with administrator privileges",
        apple_string(&shell)
    );
    let mut worker = tokio::spawn(async move {
        let output = tokio::process::Command::new("/usr/bin/osascript")
            .args(["-e", &script])
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| format!("couldn't request removable-disk access: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "macOS didn't allow access to the removable disk: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    });

    let command = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&command_path)
        .map_err(|error| format!("couldn't open raw-disk command pipe: {error}"))?;
    let data = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&data_path)
        .map_err(|error| format!("couldn't open raw-disk data pipe: {error}"))?;

    loop {
        tokio::select! {
            result = &mut worker => {
                let _ = tokio::fs::remove_dir_all(&directory).await;
                return Err(match result {
                    Ok(Ok(())) => "administrator raw-disk helper exited before it became ready".into(),
                    Ok(Err(error)) => error,
                    Err(error) => format!("administrator raw-disk helper stopped: {error}"),
                });
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                if tokio::fs::metadata(&ready_path).await.is_ok() {
                    break;
                }
            }
        }
    }
    let _ = tokio::fs::remove_file(&ready_path).await;
    Ok(MacRangeReader {
        command: tokio::fs::File::from_std(command),
        data: tokio::fs::File::from_std(data),
        worker,
        directory,
    })
}

#[cfg(target_os = "macos")]
async fn authorized_raw_reader(
    raw: &str,
) -> Result<
    (
        tokio::fs::File,
        tokio::task::JoinHandle<Result<(), String>>,
        PathBuf,
    ),
    String,
> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut nonce = [0u8; 8];
    getrandom::getrandom(&mut nonce)
        .map_err(|error| format!("couldn't prepare removable-disk access: {error}"))?;
    let suffix = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let fifo = std::env::temp_dir().join(format!("allmystuff-kvm-media-{suffix}.pipe"));
    let status = tokio::process::Command::new("/usr/bin/mkfifo")
        .arg(&fifo)
        .status()
        .await
        .map_err(|error| format!("couldn't prepare the removable-disk stream: {error}"))?;
    if !status.success() {
        return Err("couldn't prepare the removable-disk stream".into());
    }
    // A blocking FIFO open would strand one of Tokio's filesystem threads if
    // the user cancelled the authorization prompt before `dd` became its
    // writer. Open nonblocking now, then flip it to blocking only after the
    // privileged shell has signalled that authorization succeeded.
    let reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .map_err(|error| format!("couldn't open the removable-disk stream: {error}"))?;
    let ready = fifo.with_extension("ready");

    // Raw disks are root:operator on an ordinary Mac. Ask macOS for the same
    // one-time administrator approval Disk Utility uses, and stream `dd`
    // directly into the upload rather than copying the whole drive locally.
    let command = format!(
        "exec 3<'{}' && exec 4>'{}' && /usr/bin/touch '{}' && /bin/dd if=/dev/fd/3 of=/dev/fd/4 bs=4m",
        raw,
        fifo.to_string_lossy().replace('\'', "'\\''"),
        ready.to_string_lossy().replace('\'', "'\\''")
    );
    let script = format!(
        "do shell script \"{}\" with administrator privileges",
        apple_string(&command)
    );
    let fifo_for_worker = fifo.clone();
    let mut worker = tokio::spawn(async move {
        let output = tokio::process::Command::new("/usr/bin/osascript")
            .args(["-e", &script])
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| format!("couldn't request removable-disk access: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "macOS didn't allow access to the removable disk: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    });
    loop {
        tokio::select! {
            result = &mut worker => {
                let _ = tokio::fs::remove_file(&ready).await;
                let _ = tokio::fs::remove_file(&fifo_for_worker).await;
                return Err(match result {
                    Ok(Ok(())) => "the removable-disk stream ended before it opened".into(),
                    Ok(Err(error)) => error,
                    Err(error) => format!("removable-disk access stopped: {error}"),
                });
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                if tokio::fs::metadata(&ready).await.is_ok() {
                    break;
                }
            }
        }
    }
    let _ = tokio::fs::remove_file(&ready).await;
    let descriptor = reader.as_raw_fd();
    // SAFETY: `descriptor` belongs to the live FIFO `reader`; F_GETFL/F_SETFL
    // neither takes ownership nor outlives it.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0
    {
        worker.abort();
        let _ = tokio::fs::remove_file(&fifo).await;
        return Err(format!(
            "couldn't start the removable-disk stream: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok((tokio::fs::File::from_std(reader), worker, fifo))
}

#[cfg(target_os = "macos")]
async fn open_removable_disk(path: &Path, label: &str, lazy: bool) -> Result<Source, String> {
    let mounted = path
        .to_str()
        .ok_or_else(|| "the removable drive path isn't valid UTF-8".to_string())?;
    let volume = diskutil_info(mounted).await?;
    let whole = if plist_bool(&volume, "WholeDisk") {
        plist_string(&volume, "DeviceIdentifier")
    } else {
        plist_string(&volume, "ParentWholeDisk")
    }
    .ok_or_else(|| format!("couldn't find the whole disk behind {}", path.display()))?;
    if !valid_whole_disk(whole) {
        return Err(format!(
            "diskutil returned an unsafe disk identifier: {whole}"
        ));
    }
    let disk = diskutil_info(whole).await?;
    let protocol = plist_string(&disk, "BusProtocol").unwrap_or("unknown");
    let external = !plist_bool(&disk, "Internal")
        && (plist_bool(&disk, "Ejectable")
            || plist_bool(&disk, "Removable")
            || protocol.eq_ignore_ascii_case("usb"));
    if !external {
        return Err(format!(
            "{} is an internal {protocol} disk, not removable install media",
            path.display()
        ));
    }
    let len = plist_size(&disk)
        .ok_or_else(|| format!("couldn't determine the size of removable disk {whole}"))?;
    let raw = format!("/dev/r{whole}");
    let (file, worker, cleanup, range) = match std::fs::File::open(&raw) {
        Ok(file) => (Some(tokio::fs::File::from_std(file)), None, None, None),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied && lazy => {
            let range = authorized_range_reader(&raw).await?;
            (None, None, None, Some(range))
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            let (file, worker, cleanup) = authorized_raw_reader(&raw).await?;
            (Some(file), Some(worker), Some(cleanup), None)
        }
        Err(error) => {
            return Err(format!(
                "couldn't read {} as a bootable disk: {error}",
                path.display()
            ));
        }
    };
    Ok(Source {
        file,
        len,
        name: image_name(path, label, true),
        cdrom: false,
        worker,
        cleanup,
        range,
    })
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
async fn open_removable_disk(_path: &Path, _label: &str, _lazy: bool) -> Result<Source, String> {
    Err("whole removable-drive imaging is currently available on Windows and macOS; choose an .iso or .img on this machine".into())
}

async fn finish_source(
    worker: Option<tokio::task::JoinHandle<Result<(), String>>>,
    cleanup: Option<PathBuf>,
    result: Result<String, String>,
) -> Result<String, String> {
    let result = match (result, worker) {
        (Ok(target), Some(worker)) => match worker.await {
            Ok(Ok(())) => Ok(target),
            Ok(Err(error)) => Err(error),
            Err(error) => Err(format!("removable-disk stream stopped: {error}")),
        },
        (Err(error), Some(worker)) => {
            worker.abort();
            Err(error)
        }
        (result, None) => result,
    };
    if let Some(path) = cleanup {
        let _ = tokio::fs::remove_file(path).await;
    }
    result
}

fn response_ok(status: reqwest::StatusCode, body: &str, action: &str) -> Result<Value, String> {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!("KVM {action} failed with HTTP {status}"));
    }
    if parsed
        .get("code")
        .and_then(Value::as_i64)
        .is_some_and(|code| code != 0)
    {
        let message = parsed
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("device refused the request");
        return Err(format!("KVM {action} failed: {message}"));
    }
    Ok(parsed)
}

async fn is_pro(client: &reqwest::Client, base: &str) -> bool {
    client
        .get(format!("{base}/api/storage/download/image/enabled"))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

async fn remote_media_enabled(client: &reqwest::Client, base: &str) -> bool {
    let Ok(response) = client
        .get(format!("{base}/api/storage/remote-media/enabled"))
        .send()
        .await
    else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    response
        .json::<Value>()
        .await
        .ok()
        .and_then(|value| {
            value
                .get("data")
                .and_then(|data| data.get("enabled"))
                .and_then(Value::as_bool)
        })
        .unwrap_or(false)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteManifest<'a> {
    session: &'a str,
    name: &'a str,
    size: u64,
    cdrom: bool,
    source: &'a str,
    label: &'a str,
}

#[derive(serde::Deserialize)]
struct RemoteMessage {
    kind: String,
    #[serde(default)]
    id: u64,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    length: usize,
    #[serde(default)]
    message: String,
}

async fn read_file_range(
    file: &mut tokio::fs::File,
    offset: u64,
    output: &mut [u8],
) -> Result<(), String> {
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|error| format!("couldn't seek the install media: {error}"))?;
    file.read_exact(output)
        .await
        .map_err(|error| format!("couldn't read the install media: {error}"))?;
    Ok(())
}

async fn read_source_range(
    file: &mut Option<tokio::fs::File>,
    #[cfg(target_os = "macos")] range: &mut Option<MacRangeReader>,
    offset: u64,
    output: &mut [u8],
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    if let Some(range) = range.as_mut() {
        return range.read_at(offset, output).await;
    }
    read_file_range(
        file.as_mut().expect("file-backed remote media"),
        offset,
        output,
    )
    .await
}

/// Ordinary HTTP fallback for site relays that cannot carry a WebSocket
/// Upgrade. The KVM long-polls for work and receives range replies as raw
/// request bodies, so the media remains lazy and resumable over TURN.
async fn stream_remote_http(
    local_port: u16,
    session: &str,
    manifest: &str,
    len: u64,
    file: &mut Option<tokio::fs::File>,
    #[cfg(target_os = "macos")] range: &mut Option<MacRangeReader>,
    stop_rx: &mut tokio::sync::watch::Receiver<bool>,
    mounted_tx: &mut Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(35))
        .build()
        .map_err(|error| format!("couldn't start the KVM media fallback: {error}"))?;
    let base = format!("http://127.0.0.1:{local_port}/api/storage/remote-media/session");
    let mut opened = false;
    let mut retry = std::time::Duration::from_millis(300);

    'poll: loop {
        if *stop_rx.borrow() {
            let _ = client
                .delete(&base)
                .query(&[("session", session)])
                .send()
                .await;
            return Ok(());
        }
        if !opened {
            let response = tokio::select! {
                _ = stop_rx.changed() => continue,
                response = client
                    .post(&base)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(manifest.to_owned())
                    .send() => response,
            };
            match response {
                Ok(response) if response.status().is_success() => {
                    opened = true;
                    retry = std::time::Duration::from_millis(300);
                }
                Ok(response) => {
                    tracing::warn!(
                        "KVM remote-media fallback open returned {}",
                        response.status()
                    );
                }
                Err(error) => tracing::warn!("KVM remote-media fallback open failed: {error}"),
            }
            if !opened {
                tokio::select! {
                    _ = stop_rx.changed() => continue,
                    _ = tokio::time::sleep(retry) => {}
                }
                retry = (retry * 2).min(std::time::Duration::from_secs(5));
                continue;
            }
        }

        let response = tokio::select! {
            _ = stop_rx.changed() => continue,
            response = client.get(format!("{base}/next"))
                .query(&[("session", session)])
                .send() => response,
        };
        let response = match response {
            Ok(response) if response.status() == reqwest::StatusCode::GONE => return Ok(()),
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                tracing::warn!("KVM remote-media poll returned {}", response.status());
                tokio::time::sleep(retry).await;
                retry = (retry * 2).min(std::time::Duration::from_secs(5));
                continue;
            }
            Err(error) => {
                tracing::warn!("KVM remote-media poll changed: {error}");
                tokio::time::sleep(retry).await;
                retry = (retry * 2).min(std::time::Duration::from_secs(5));
                continue;
            }
        };
        retry = std::time::Duration::from_millis(300);
        let message = response
            .json::<RemoteMessage>()
            .await
            .map_err(|error| format!("KVM sent an invalid remote-media poll: {error}"))?;
        match message.kind.as_str() {
            "idle" => {}
            "mounted" => {
                if let Some(tx) = mounted_tx.take() {
                    let _ = tx.send(Ok(()));
                }
            }
            "unmounted" => return Ok(()),
            "error" => {
                let error = if message.message.is_empty() {
                    "KVM remote media failed".to_string()
                } else {
                    message.message
                };
                if let Some(tx) = mounted_tx.take() {
                    let _ = tx.send(Err(error.clone()));
                }
                return Err(error);
            }
            "read" => {
                let end = message.offset.checked_add(message.length as u64);
                if message.length == 0
                    || message.length > REMOTE_MEDIA_MAX_READ
                    || end.is_none_or(|end| end > len)
                {
                    return Err("KVM requested an invalid remote-media range".into());
                }
                let mut data = vec![0u8; message.length];
                read_source_range(
                    file,
                    #[cfg(target_os = "macos")]
                    range,
                    message.offset,
                    &mut data,
                )
                .await?;
                let reply_id = message.id.to_string();
                let mut reply_retry = std::time::Duration::from_millis(300);
                loop {
                    let response = tokio::select! {
                        _ = stop_rx.changed() => continue 'poll,
                        response = client
                            .post(format!("{base}/reply"))
                            .query(&[("session", session), ("id", reply_id.as_str())])
                            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                            .body(data.clone())
                            .send() => response,
                    };
                    match response {
                        Ok(response) if response.status().is_success() => break,
                        // The KVM timed out this request while the route was
                        // absent. Resume polling; it will ask for the page
                        // again and receives the same bytes under a new id.
                        Ok(response) if response.status() == reqwest::StatusCode::GONE => break,
                        Ok(response) if response.status().is_client_error() => {
                            return Err(format!(
                                "KVM refused a remote-media reply with HTTP {}",
                                response.status()
                            ));
                        }
                        Ok(response) => tracing::warn!(
                            "KVM remote-media reply returned {}; retrying",
                            response.status()
                        ),
                        Err(error) => tracing::warn!(
                            "KVM remote-media reply path changed ({error}); retrying"
                        ),
                    }
                    tokio::select! {
                        _ = stop_rx.changed() => continue 'poll,
                        _ = tokio::time::sleep(reply_retry) => {}
                    }
                    reply_retry = (reply_retry * 2).min(std::time::Duration::from_secs(5));
                }
            }
            _ => {}
        }
    }
}

async fn stream_remote(
    local_port: u16,
    source_node: &str,
    label: &str,
    source: Source,
) -> Result<(), String> {
    if source.worker.is_some() {
        return Err(
            "lazy KVM media needs a seekable source; this macOS removable disk still uses a sequential administrator stream"
                .into(),
        );
    }
    let mut random = [0u8; 16];
    getrandom::getrandom(&mut random)
        .map_err(|error| format!("couldn't create a remote-media session: {error}"))?;
    let session = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let manifest = serde_json::to_string(&RemoteManifest {
        session: &session,
        name: &source.name,
        size: source.len,
        cdrom: source.cdrom,
        source: source_node,
        label,
    })
    .map_err(|error| format!("couldn't describe remote media: {error}"))?;
    let url = format!("ws://127.0.0.1:{local_port}/api/storage/remote-media");
    let (mounted_tx, mounted_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    if let Some((_, old)) = REMOTE_MEDIA_PROVIDERS
        .lock()
        .expect("remote-media provider registry poisoned")
        .insert(local_port, (session.clone(), stop_tx))
    {
        let _ = old.send(true);
    }
    let provider_session = session.clone();
    crate::spawn(async move {
        let Source {
            mut file,
            len,
            cleanup,
            #[cfg(target_os = "macos")]
            mut range,
            ..
        } = source;
        let mut mounted_tx = Some(mounted_tx);
        let mut terminal_error: Option<String> = None;
        let mut retry = std::time::Duration::from_millis(300);
        'provider: loop {
            if *stop_rx.borrow() {
                break;
            }
            let connected = tokio::select! {
                _ = stop_rx.changed() => break,
                connected = tokio_tungstenite::connect_async(&url) => connected,
            };
            let (socket, _) = match connected {
                Ok(socket) => socket,
                Err(error) => {
                    // A working site may still reject Upgrade (notably after
                    // a route rebuild or over some TURN relays). Fall back to
                    // the equivalent long-poll protocol before mounting, then
                    // stay on that resumable transport for this session.
                    if mounted_tx.is_some() {
                        tracing::warn!(
                            "KVM remote-media WebSocket unavailable ({error}); using HTTP fallback"
                        );
                        let result = stream_remote_http(
                            local_port,
                            &provider_session,
                            &manifest,
                            len,
                            &mut file,
                            #[cfg(target_os = "macos")]
                            &mut range,
                            &mut stop_rx,
                            &mut mounted_tx,
                        )
                        .await;
                        if let Err(error) = result {
                            terminal_error = Some(error);
                        }
                        break 'provider;
                    } else {
                        tracing::warn!("KVM remote-media reconnect failed: {error}");
                        tokio::select! {
                            _ = stop_rx.changed() => break,
                            _ = tokio::time::sleep(retry) => {}
                        }
                        retry = (retry * 2).min(std::time::Duration::from_secs(5));
                        continue;
                    }
                }
            };
            retry = std::time::Duration::from_millis(300);
            let (mut writer, mut reader) = socket.split();
            if let Err(error) = writer.send(Message::Text(manifest.clone().into())).await {
                tracing::warn!("couldn't resume KVM remote media: {error}");
                continue;
            }

            let mut reconnect = true;
            loop {
                let next = tokio::select! {
                    _ = stop_rx.changed() => {
                        let _ = writer.send(Message::Close(None)).await;
                        reconnect = false;
                        break;
                    }
                    next = reader.next() => next,
                };
                let message = match next {
                    Some(Ok(message)) => message,
                    Some(Err(error)) => {
                        tracing::warn!("KVM remote-media connection changed: {error}");
                        break;
                    }
                    None => break,
                };
                if let Message::Close(frame) = &message {
                    if frame
                        .as_ref()
                        .is_some_and(|frame| frame.reason.as_str() == "unmounted")
                    {
                        reconnect = false;
                    }
                    break;
                }
                let Message::Text(text) = message else {
                    continue;
                };
                let message: RemoteMessage = match serde_json::from_str(text.as_str()) {
                    Ok(message) => message,
                    Err(error) => {
                        terminal_error =
                            Some(format!("KVM sent an invalid remote-media request: {error}"));
                        reconnect = false;
                        break;
                    }
                };
                match message.kind.as_str() {
                    "mounted" => {
                        if let Some(tx) = mounted_tx.take() {
                            let _ = tx.send(Ok(()));
                        }
                    }
                    "error" => {
                        let error = if message.message.is_empty() {
                            "KVM remote media failed".to_string()
                        } else {
                            message.message
                        };
                        if let Some(tx) = mounted_tx.take() {
                            let _ = tx.send(Err(error.clone()));
                        }
                        terminal_error = Some(error);
                        reconnect = false;
                        break;
                    }
                    "read" => {
                        let end = message.offset.checked_add(message.length as u64);
                        if message.length == 0
                            || message.length > REMOTE_MEDIA_MAX_READ
                            || end.is_none_or(|end| end > len)
                        {
                            terminal_error =
                                Some("KVM requested an invalid remote-media range".into());
                            reconnect = false;
                            break;
                        }
                        let mut frame = vec![0u8; 8 + message.length];
                        frame[..8].copy_from_slice(&message.id.to_be_bytes());
                        let read = read_source_range(
                            &mut file,
                            #[cfg(target_os = "macos")]
                            &mut range,
                            message.offset,
                            &mut frame[8..],
                        )
                        .await;
                        if let Err(error) = read {
                            terminal_error = Some(error);
                            reconnect = false;
                            break;
                        }
                        if let Err(error) = writer.send(Message::Binary(frame.into())).await {
                            tracing::warn!("couldn't answer a KVM media read: {error}");
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if reconnect && mounted_tx.is_some() {
                tracing::warn!(
                    "KVM remote-media WebSocket closed before mounting; using HTTP fallback"
                );
                let result = stream_remote_http(
                    local_port,
                    &provider_session,
                    &manifest,
                    len,
                    &mut file,
                    #[cfg(target_os = "macos")]
                    &mut range,
                    &mut stop_rx,
                    &mut mounted_tx,
                )
                .await;
                if let Err(error) = result {
                    terminal_error = Some(error);
                }
                break 'provider;
            }
            if !reconnect {
                break 'provider;
            }
            tokio::select! {
                _ = stop_rx.changed() => break,
                _ = tokio::time::sleep(retry) => {}
            }
        }
        if let Some(tx) = mounted_tx.take() {
            let _ = tx.send(Err(terminal_error.clone().unwrap_or_else(|| {
                "KVM remote-media connection closed before mounting".into()
            })));
        }
        if let Some(error) = terminal_error {
            tracing::warn!("{error}");
        }
        if let Some(path) = cleanup {
            let _ = tokio::fs::remove_file(path).await;
        }
        #[cfg(target_os = "macos")]
        if let Some(range) = range {
            range.finish().await;
        }
        let mut providers = REMOTE_MEDIA_PROVIDERS
            .lock()
            .expect("remote-media provider registry poisoned");
        if providers
            .get(&local_port)
            .is_some_and(|(session, _)| session == &provider_session)
        {
            providers.remove(&local_port);
        }
    });

    let result = match tokio::time::timeout(std::time::Duration::from_secs(45), mounted_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("KVM remote-media provider stopped before mounting".into()),
        Err(_) => Err("KVM did not make the remote install media ready in time".into()),
    };
    if result.is_err() {
        cancel_remote_provider(local_port);
    }
    result
}

async fn upload_standard(
    client: &reqwest::Client,
    base: &str,
    source: Source,
) -> Result<String, String> {
    let Source {
        file,
        len,
        name,
        worker,
        cleanup,
        ..
    } = source;
    let file = file.ok_or_else(|| {
        "this KVM needs remote-media support to use a macOS removable disk".to_string()
    })?;
    let target = format!("/data/{name}");
    let result = async {
        let part = Part::stream_with_length(file, len)
            .file_name(name)
            .mime_str("application/octet-stream")
            .map_err(|error| error.to_string())?;
        let response = client
            .post(format!("{base}/api/download/file"))
            .multipart(Form::new().part("file", part))
            .send()
            .await
            .map_err(|error| format!("couldn't upload virtual media to the KVM: {error}"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        response_ok(status, &body, "upload")?;
        Ok(target)
    }
    .await;
    finish_source(worker, cleanup, result).await
}

async fn upload_pro(
    client: &reqwest::Client,
    base: &str,
    source: Source,
) -> Result<String, String> {
    let Source {
        file,
        len,
        name,
        worker,
        cleanup,
        ..
    } = source;
    let mut file = file.ok_or_else(|| {
        "this KVM needs remote-media support to use a macOS removable disk".to_string()
    })?;
    let target = format!("/data/{name}");
    let result = async {
        let chunks = len.div_ceil(PRO_CHUNK as u64).max(1);
        let mut buffer = vec![0u8; PRO_CHUNK];
        for index in 0..chunks {
            let offset = index * PRO_CHUNK as u64;
            let want = usize::try_from((len - offset).min(PRO_CHUNK as u64)).unwrap_or(PRO_CHUNK);
            file.read_exact(&mut buffer[..want])
                .await
                .map_err(|e| format!("couldn't read virtual media: {e}"))?;
            let part = Part::bytes(buffer[..want].to_vec())
                .file_name(name.clone())
                .mime_str("application/octet-stream")
                .map_err(|error| error.to_string())?;
            let response = client
                .post(format!("{base}/api/storage/image/upload"))
                .multipart(
                    Form::new()
                        .text("chunkIndex", index.to_string())
                        .text("chunkSize", PRO_CHUNK.to_string())
                        .text("totalChunks", chunks.to_string())
                        .part("file", part),
                )
                .send()
                .await
                .map_err(|error| {
                    format!("couldn't upload virtual-media chunk {}: {error}", index + 1)
                })?;
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            response_ok(status, &body, "upload")?;
        }
        Ok(target)
    }
    .await;
    finish_source(worker, cleanup, result).await
}

/// Stage and mount one local source on the KVM reachable at `local_port`.
pub async fn stage(
    local_port: u16,
    source_node: &str,
    path: &str,
    label: &str,
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(60 * 60 * 6))
        .build()
        .map_err(|error| format!("couldn't start KVM transfer: {error}"))?;
    let base = format!("http://127.0.0.1:{local_port}");
    let lazy = remote_media_enabled(&client, &base).await;
    let source = open_source(path, label, lazy).await?;
    let cdrom = source.cdrom;
    if source.worker.is_none() && lazy {
        return stream_remote(local_port, source_node, label, source).await;
    }
    let target = if is_pro(&client, &base).await {
        upload_pro(&client, &base, source).await?
    } else {
        upload_standard(&client, &base, source).await?
    };
    let response = client
        .post(format!("{base}/api/storage/image/mount"))
        .json(&json!({
            "file": target,
            "cdrom": cdrom,
            "readOnly": true,
            "source": source_node,
            "label": label,
        }))
        .send()
        .await
        .map_err(|error| format!("virtual media uploaded but couldn't be mounted: {error}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    response_ok(status, &body, "mount")?;
    Ok(())
}

pub async fn unmount(local_port: u16) -> Result<(), String> {
    cancel_remote_provider(local_port);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .post(format!(
            "http://127.0.0.1:{local_port}/api/storage/image/mount"
        ))
        .json(&json!({ "file": "", "cdrom": false, "readOnly": false }))
        .send()
        .await
        .map_err(|error| format!("couldn't unmount KVM virtual media: {error}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    response_ok(status, &body, "unmount")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::windows_drive_letter;

    /// The regression. `std::fs::canonicalize("E:\\")` on Windows returns the
    /// extended-length form `\\?\E:\`, and every path reaching
    /// `open_removable_disk` has been canonicalized — so this, not the form a
    /// human types, is what the drive-letter test actually sees. Reading the
    /// letter off byte 0 found a backslash and refused the mapping every time:
    /// installable media could never be mapped from Windows at all.
    #[test]
    fn canonicalized_drive_root_still_yields_its_letter() {
        assert_eq!(windows_drive_letter(r"\\?\E:\"), Some('E'));
        assert_eq!(windows_drive_letter(r"\\?\e:\"), Some('E'));
        assert_eq!(windows_drive_letter(r"\\?\C:\media\win11"), Some('C'));
    }

    /// The plain form keeps working — a caller that skips canonicalize, and
    /// the shape every error message tells the operator to use.
    #[test]
    fn plain_drive_root_still_yields_its_letter() {
        assert_eq!(windows_drive_letter(r"E:\"), Some('E'));
        assert_eq!(windows_drive_letter("E:"), Some('E'));
        assert_eq!(windows_drive_letter(r"d:\images"), Some('D'));
    }

    /// A network share has no local disk behind it to read raw sectors from,
    /// so it must keep failing rather than being parsed into some letter.
    #[test]
    fn unc_shares_are_not_drives() {
        assert_eq!(windows_drive_letter(r"\\?\UNC\server\share"), None);
        assert_eq!(windows_drive_letter(r"\\server\share"), None);
    }

    #[test]
    fn non_drive_paths_are_rejected() {
        assert_eq!(windows_drive_letter(""), None);
        assert_eq!(windows_drive_letter("E"), None);
        assert_eq!(windows_drive_letter("1:"), None);
        assert_eq!(windows_drive_letter("/home/user/media"), None);
        assert_eq!(windows_drive_letter(r"\\?\"), None);
    }
}
