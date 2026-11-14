//! Native SMB drive mappings. Only AMS-owned exact endpoints can be removed.
use allmystuff_session::SmbEndpoint;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    net::SocketAddr,
    process::Stdio,
    time::{Duration, Instant},
};

static LEASE_LOCK: Mutex<()> = Mutex::new(());
#[derive(Clone, Serialize, Deserialize)]
struct Lease {
    mount: String,
    remote: String,
    port: u16,
    owner: String,
    #[serde(default)]
    generation: String,
}

impl Lease {
    fn matches(&self, other: &Self) -> bool {
        self.mount.eq_ignore_ascii_case(&other.mount)
            && self.remote.eq_ignore_ascii_case(&other.remote)
            && self.port == other.port
            && self.owner == other.owner
            && self.generation == other.generation
    }
}

const PREFLIGHT: &str = r#"
$ErrorActionPreference='Stop'; $ProgressPreference='SilentlyContinue'
try {
 $null=[Console]::In.ReadToEnd()
 $command=Get-Command New-SmbGlobalMapping -ErrorAction SilentlyContinue
 if (!$command -or !$command.Parameters.ContainsKey('TcpPort')) { throw 'SMB mapped drives require Windows 11 24H2 / Server 2025 or later.' }
 $principal=[Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
 if (!$principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'SMB mapped drives require the installed AMS service or an elevated node.' }
 exit 0
} catch { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }
"#;

const CONNECT: &str = r#"
$ErrorActionPreference='Stop'; $ProgressPreference='SilentlyContinue'
try {
 $p=[Console]::In.ReadToEnd() | ConvertFrom-Json
 if (!(Get-Command New-SmbGlobalMapping).Parameters.ContainsKey('TcpPort')) { throw 'SMB mapped drives require Windows 11 24H2 / Server 2025 or later.' }
 $assigned=@(Get-SmbGlobalMapping -ErrorAction Stop)+@(Get-SmbMapping -ErrorAction Stop)
 $remembered="Registry::HKEY_USERS\$($p.owner)\Network\$($p.mount.TrimEnd(':'))"
 if (($assigned | Where-Object LocalPath -eq $p.mount) -or (Test-Path -LiteralPath $remembered -ErrorAction Stop)) { throw 'The drive letter is already assigned, including a disconnected mapping; it was preserved.' }
 $secret=ConvertTo-SecureString $p.password -AsPlainText -Force
 $credential=[pscredential]::new($p.username,$secret)
 $owner=([Security.Principal.SecurityIdentifier]::new($p.owner)).Translate([Security.Principal.NTAccount]).Value
 $system=([Security.Principal.SecurityIdentifier]::new('S-1-5-18')).Translate([Security.Principal.NTAccount]).Value
 New-SmbGlobalMapping -LocalPath $p.mount -RemotePath $p.remote -TcpPort $p.port -Credential $credential -TransportType TCP -RequireIntegrity $true -RequirePrivacy $true -Persistent $false -FullAccess @($owner,$system) -ErrorAction Stop | Out-Null
 exit 0
} catch { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }
"#;
const DISCONNECT: &str = r#"
$ErrorActionPreference='Stop'; $ProgressPreference='SilentlyContinue'
try {
 $p=[Console]::In.ReadToEnd() | ConvertFrom-Json
 $found=Get-SmbGlobalMapping -LocalPath $p.mount -ErrorAction SilentlyContinue
 if ($found -and [String]::Equals($found.RemotePath,$p.remote,[StringComparison]::OrdinalIgnoreCase)) {
   if ($found.PSObject.Properties.Name -contains 'TcpPort' -and $found.TcpPort -and $found.TcpPort -ne $p.port) { throw 'The SMB endpoint changed; its mapping was preserved.' }
   Remove-SmbGlobalMapping -LocalPath $p.mount -Force -Confirm:$false -ErrorAction Stop
 }
 exit 0
} catch { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }
"#;

fn run(script: &str, payload: serde_json::Value) -> Result<(), String> {
    use base64::Engine;
    use std::io::Write;
    let encoded = base64::engine::general_purpose::STANDARD.encode(
        script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    // Credentials travel through stdin, never the command line or a file.
    let mut child = crate::child_process::blocking_command("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &encoded,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start Windows SMB client: {error}"))?;
    let body = serde_json::to_vec(&payload).map_err(|error| error.to_string())?;
    if let Err(error) = child.stdin.take().unwrap().write_all(&body) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error.to_string());
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Windows SMB client operation timed out".into());
            }
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        // PowerShell's error text must not echo the credential payload.
        let error = String::from_utf8_lossy(&output.stderr);
        let error = payload
            .get("password")
            .and_then(|p| p.as_str())
            .map_or_else(
                || error.to_string(),
                |secret| error.replace(secret, "[redacted]"),
            );
        Err(format!("Windows couldn't configure the AMS SMB drive (the installed AMS service or an elevated node is required): {}",error.trim()))
    }
}

/// Check the receiving OS and token before migration can remove a legacy alias.
pub(super) async fn ensure_supported() -> Result<(), String> {
    tokio::task::spawn_blocking(|| run(PREFLIGHT, json!({})))
        .await
        .map_err(|error| error.to_string())?
}

fn lease_file() -> Result<std::path::PathBuf, String> {
    Ok(allmystuff_protocol::myownmesh_state_dir()
        .ok_or("AllMyStuff state is unavailable")?
        .join("allmystuff-smb-drives.json"))
}
fn leases() -> Result<Vec<Lease>, String> {
    match std::fs::read(lease_file()?) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.to_string()),
    }
}
fn save(leases: &[Lease]) -> Result<(), String> {
    crate::persist::write_atomic(
        &lease_file()?,
        &serde_json::to_vec(leases).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

pub(super) fn remove_known(mount: &str) -> Result<bool, String> {
    let mut removed = false;
    for lease in leases()?
        .into_iter()
        .filter(|lease| lease.mount.eq_ignore_ascii_case(mount))
    {
        removed |= remove_exact(&lease)?;
    }
    Ok(removed)
}

fn remove_exact(expected: &Lease) -> Result<bool, String> {
    let _guard = LEASE_LOCK.lock();
    let mut saved = leases()?;
    if !saved.iter().any(|lease| lease.matches(expected)) {
        return Ok(false);
    }
    if !owned_remote(&expected.remote) {
        return Err("the saved SMB endpoint is not an AMS loopback share".into());
    }
    run(
        DISCONNECT,
        serde_json::to_value(expected).map_err(|error| error.to_string())?,
    )?;
    saved.retain(|lease| !lease.matches(expected));
    save(&saved)?;
    Ok(true)
}
fn owned_remote(remote: &str) -> bool {
    let Some((host, share)) = remote
        .strip_prefix(r"\\")
        .and_then(|value| value.split_once('\\'))
    else {
        return false;
    };
    host.parse::<std::net::Ipv4Addr>()
        .is_ok_and(|address| address.is_loopback())
        && share.starts_with("ams")
        && share.len() == 19
        && share[3..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn dos_target_matches_remote(target: &str, remote: &str) -> bool {
    let target = target.trim_end_matches('\\').to_ascii_lowercase();
    let suffix = format!(r"\{}", remote.trim_matches('\\')).to_ascii_lowercase();
    (target.starts_with(r"\device\lanmanredirector\") || target.starts_with(r"\device\mup\"))
        && target.ends_with(&suffix)
}
pub(super) fn cleanup() -> Result<(), String> {
    for lease in leases()? {
        remove_known(&lease.mount)?;
    }
    Ok(())
}

pub(super) struct NativeSmbDrive {
    lease: Lease,
}
impl NativeSmbDrive {
    pub(super) async fn start(
        mount: &str,
        label: &str,
        address: SocketAddr,
        endpoint: SmbEndpoint,
    ) -> Result<Self, String> {
        if !address.ip().is_loopback() {
            return Err("the SMB tunnel must listen on loopback".into());
        }
        if !super::windows_letter_available(mount).await? {
            return Err(format!(
                "{mount} is already assigned; its drive was preserved"
            ));
        }
        let owner = crate::win_privilege::interactive_user_call(
            "ams-smb-owner",
            crate::win_privilege::effective_user_sid,
        )?;
        let mut identity = [0u8; 16];
        getrandom::getrandom(&mut identity).map_err(|error| error.to_string())?;
        let lease = Lease {
            mount: mount.to_string(),
            remote: format!(r"\\{}\{}", address.ip(), endpoint.share),
            port: address.port(),
            owner,
            generation: identity.iter().map(|byte| format!("{byte:02x}")).collect(),
        };
        if !owned_remote(&lease.remote) {
            return Err("the SMB share identity is invalid".into());
        }
        {
            let _guard = LEASE_LOCK.lock();
            let mut saved = leases()?;
            // The OS proved this letter is free. Stale bookkeeping does not
            // reserve it, and selection never disconnects an existing drive.
            saved.retain(|entry| !entry.mount.eq_ignore_ascii_case(mount));
            saved.push(lease.clone());
            save(&saved)?;
        }
        let mut payload = serde_json::to_value(&lease).map_err(|error| error.to_string())?;
        payload["username"] = json!(endpoint.username);
        payload["password"] = json!(endpoint.password);
        let result = tokio::task::spawn_blocking(move || run(CONNECT, payload))
            .await
            .map_err(|error| error.to_string())?;
        if let Err(error) = result {
            if let Err(cleanup) = remove_exact(&lease) {
                return Err(format!("{error}; cleanup: {cleanup}"));
            }
            return Err(error);
        }
        let visibility = crate::win_privilege::interactive_user_logical_drive_mask().and_then(|mask| {
            let letter=mount.as_bytes().first().copied().ok_or("missing SMB drive letter")?.to_ascii_uppercase();
            if !letter.is_ascii_uppercase() || mask & (1u32 << (letter-b'A')) == 0 {
                Err("Windows created an SMB mapping that is not visible in the signed-in user's drive namespace".to_string())
            } else {
                let ours=match crate::win_privilege::interactive_user_network_mapping(mount)? {
                    Some(remote)=>remote.trim_end_matches('\\').eq_ignore_ascii_case(lease.remote.trim_end_matches('\\')),
                    None=>crate::win_privilege::interactive_user_dos_device_targets(mount)?.first()
                        .is_some_and(|target|dos_target_matches_remote(target,&lease.remote)),
                };
                if ours {Ok(())} else {Err("The selected drive letter now belongs to another mapping; it was preserved".into())}
            }
        });
        if let Err(error) = visibility {
            remove_exact(&lease)?;
            return Err(error);
        }
        let key = format!(
            r"{}\Software\Microsoft\Windows\CurrentVersion\Explorer\DriveIcons\{}\DefaultLabel",
            super::drive_registry_root()?,
            mount.trim_end_matches(':')
        );
        let _ = crate::child_process::command("reg.exe")
            .args(["add", &key, "/ve", "/t", "REG_SZ", "/d", label, "/f"])
            .output()
            .await;
        Ok(Self { lease })
    }
}
impl Drop for NativeSmbDrive {
    fn drop(&mut self) {
        if let Err(error) = remove_exact(&self.lease) {
            tracing::warn!(%error,"couldn't remove AMS SMB mapping");
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn visible_drive_identity_must_match_the_exact_smb_endpoint() {
        let remote = r"\\127.64.1.2\ams0123456789abcdef";
        assert!(super::dos_target_matches_remote(
            r"\Device\LanmanRedirector\;Y:1234\127.64.1.2\ams0123456789abcdef",
            remote
        ));
        assert!(super::dos_target_matches_remote(
            r"\Device\Mup\127.64.1.2\ams0123456789abcdef",
            remote
        ));
        for target in [
            r"\Device\LanmanRedirector\;Y:1234\FRANK6\media",
            r"\Device\Mup\127.64.1.3\ams0123456789abcdef",
            r"\??\C:\temp\127.64.1.2\ams0123456789abcdef",
            r"\Device\Mup\127.64.1.2\ams0123456789abcdef\child",
        ] {
            assert!(!super::dos_target_matches_remote(target, remote));
        }
    }
    #[test]
    fn an_old_owner_cannot_remove_a_replacement_on_the_same_letter() {
        let old = super::Lease {
            mount: "Y:".into(),
            remote: r"\\127.64.1.2\ams0123456789abcdef".into(),
            port: 23456,
            owner: "user".into(),
            generation: "original".into(),
        };
        let mut replacement = old.clone();
        replacement.generation = "replacement".into();
        assert!(!old.matches(&replacement));
        replacement = old.clone();
        replacement.remote = r"\\127.64.1.3\amsfedcba9876543210".into();
        assert!(!old.matches(&replacement));
        replacement = old.clone();
        replacement.port += 1;
        assert!(!old.matches(&replacement));
        replacement = old.clone();
        replacement.owner = "another-user".into();
        assert!(!old.matches(&replacement));
        replacement = old.clone();
        replacement.mount = "y:".into();
        assert!(old.matches(&replacement));
    }
    #[test]
    fn ownership_requires_a_loopback_address_and_private_share_identity() {
        assert!(super::owned_remote(r"\\127.64.1.2\ams0123456789abcdef"));
        for value in [
            r"\\FRANK6\media",
            r"\\192.168.1.2\ams0123456789abcdef",
            r"\\127.0.0.1\other",
            r"\\127.0.0.1\ams0123456789abcdef\child",
        ] {
            assert!(!super::owned_remote(value));
        }
    }
}
