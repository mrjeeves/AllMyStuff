//! KVM virtual-media staging.
//!
//! A KVM cannot consume a desktop-native mapped drive: BIOS/UEFI runs before
//! that desktop agent exists. Instead the source machine streams an ISO/image
//! (or the exact raw bytes of a removable Windows/macOS disk) through the KVM's mesh
//! site tunnel. The KVM stages it on `/data`, binds it to its Linux USB mass-
//! storage gadget, and re-enumerates USB at the attached computer.

use reqwest::multipart::{Form, Part};
#[cfg(target_os = "windows")]
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;

const PRO_CHUNK: usize = 8 * 1024 * 1024;

struct Source {
    file: tokio::fs::File,
    len: u64,
    name: String,
    cdrom: bool,
    /// macOS may need an authorized `dd` writer feeding a private FIFO. ISO,
    /// IMG, Windows raw disks, and root-run macOS nodes leave these empty.
    worker: Option<tokio::task::JoinHandle<Result<(), String>>>,
    cleanup: Option<PathBuf>,
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

async fn open_source(path: &str, label: &str) -> Result<Source, String> {
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
            file,
            len,
            cdrom: lower.ends_with(".iso"),
            name,
            worker: None,
            cleanup: None,
        });
    }
    if path.is_dir() {
        return open_removable_disk(&path, label).await;
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
async fn open_removable_disk(path: &Path, label: &str) -> Result<Source, String> {
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
        file: tokio::fs::File::from_std(file),
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
async fn open_removable_disk(path: &Path, label: &str) -> Result<Source, String> {
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
    let (file, worker, cleanup) = match std::fs::File::open(&raw) {
        Ok(file) => (tokio::fs::File::from_std(file), None, None),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            let (file, worker, cleanup) = authorized_raw_reader(&raw).await?;
            (file, Some(worker), Some(cleanup))
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
    })
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
async fn open_removable_disk(_path: &Path, _label: &str) -> Result<Source, String> {
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
        mut file,
        len,
        name,
        worker,
        cleanup,
        ..
    } = source;
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
    let source = open_source(path, label).await?;
    let cdrom = source.cdrom;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(60 * 60 * 6))
        .build()
        .map_err(|error| format!("couldn't start KVM transfer: {error}"))?;
    let base = format!("http://127.0.0.1:{local_port}");
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
