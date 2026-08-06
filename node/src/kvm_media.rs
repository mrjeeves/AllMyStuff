//! KVM virtual-media staging.
//!
//! A KVM cannot consume a desktop-native mapped drive: BIOS/UEFI runs before
//! that desktop agent exists. Instead the source machine streams an ISO/image
//! (or the exact raw bytes of a removable Windows disk) through the KVM's mesh
//! site tunnel. The KVM stages it on `/data`, binds it to its Linux USB mass-
//! storage gadget, and re-enumerates USB at the attached computer.

use reqwest::multipart::{Form, Part};
#[cfg(target_os = "windows")]
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const PRO_CHUNK: usize = 8 * 1024 * 1024;

struct Source {
    file: tokio::fs::File,
    len: u64,
    name: String,
    cdrom: bool,
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
        });
    }
    if path.is_dir() {
        return open_removable_disk(&path, label).await;
    }
    Err("choose an .iso/.img file or the root of a removable USB drive".into())
}

#[cfg(target_os = "windows")]
async fn open_removable_disk(path: &Path, label: &str) -> Result<Source, String> {
    let text = path.to_string_lossy();
    let bytes = text.as_bytes();
    if bytes.len() < 2 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return Err("choose the root of a drive (for example E:\\)".into());
    }
    let letter = (bytes[0] as char).to_ascii_uppercase();
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
    })
}

#[cfg(not(target_os = "windows"))]
async fn open_removable_disk(_path: &Path, _label: &str) -> Result<Source, String> {
    Err("whole removable-drive imaging is currently available on Windows; choose an .iso or .img on this machine".into())
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
    let target = format!("/data/{}", source.name);
    let part = Part::stream_with_length(source.file, source.len)
        .file_name(source.name.clone())
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

async fn upload_pro(
    client: &reqwest::Client,
    base: &str,
    mut source: Source,
) -> Result<String, String> {
    let target = format!("/data/{}", source.name);
    let chunks = source.len.div_ceil(PRO_CHUNK as u64).max(1);
    let mut buffer = vec![0u8; PRO_CHUNK];
    for index in 0..chunks {
        let offset = index * PRO_CHUNK as u64;
        source
            .file
            .seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| e.to_string())?;
        let want =
            usize::try_from((source.len - offset).min(PRO_CHUNK as u64)).unwrap_or(PRO_CHUNK);
        source
            .file
            .read_exact(&mut buffer[..want])
            .await
            .map_err(|e| format!("couldn't read virtual media: {e}"))?;
        let part = Part::bytes(buffer[..want].to_vec())
            .file_name(source.name.clone())
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
