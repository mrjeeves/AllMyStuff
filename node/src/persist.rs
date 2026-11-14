//! Crash-safe persistence for the node's durable stores (ownership,
//! shares, parked networks). Mirrors the daemon-side module of the
//! same name in myownmesh-core:
//!
//! * [`write_atomic`] — temp + fsync + rename, so a power cut or a
//!   killed process mid-write can never leave a truncated file. That
//!   matters most for the ownership record: a 0-byte file used to
//!   load as `Default` — an *unowned* device — silently forgetting
//!   the owner and (with the env flag) re-offering itself for
//!   claiming.
//! * [`load_json`] — the common load shape: missing file is a quiet
//!   default; a file that exists but doesn't parse is quarantined
//!   aside as `{name}.corrupt` (bytes preserved for hand-recovery)
//!   with an error-level log, then the default is returned.

use std::io::Write;
use std::path::Path;

/// Keep the strongest supported flush. Darwin's F_FULLFSYNC is unavailable
/// on network filesystems; an ordinary fsync still requests the SMB flush.
/// Other errors (including a failed fallback) remain real transfer failures.
pub(crate) fn sync_file(file: &std::fs::File) -> std::io::Result<()> {
    let result = file.sync_all();
    #[cfg(target_os = "macos")]
    if result
        .as_ref()
        .is_err_and(|error| error.raw_os_error() == Some(libc::ENOTSUP))
    {
        use std::os::fd::AsRawFd;
        return if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        };
    }
    result
}

/// Atomically replace `path` with `bytes`. The temp file lives in the
/// same directory (rename can't cross filesystems) and is created
/// `0600` on Unix — the ownership record carries the plaintext fleet
/// key (AMS-08), and none of these stores has any business being
/// world-readable.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let Some(name) = path.file_name() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("no file name in {}", path.display()),
        ));
    };
    let mut tmp_name = name.to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        // The rename must not be able to land before the data does.
        sync_file(&f)?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Load a JSON store: missing → default, corrupt → quarantine +
/// default, loudly. Never a hard error — these stores must not stop
/// the node from starting — but never a *silent* wipe either.
pub(crate) fn load_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(_) => return T::default(),
    };
    match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            let mut quarantined = path.file_name().unwrap_or_default().to_os_string();
            quarantined.push(".corrupt");
            let dest = path.with_file_name(quarantined);
            let kept = std::fs::rename(path, &dest).is_ok();
            tracing::error!(
                path = %path.display(),
                quarantined = kept,
                "store file is corrupt ({e}) — starting from defaults; \
                 the previous contents were kept beside it as .corrupt"
            );
            T::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[derive(serde::Deserialize, Default, PartialEq, Debug)]
    struct Doc {
        n: u32,
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ams-persist-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp() {
        let dir = tmpdir("write");
        let path = dir.join("store.json");
        write_atomic(&path, b"{\"n\":1}").unwrap();
        write_atomic(&path, b"{\"n\":2}").unwrap();
        assert_eq!(load_json::<Doc>(&path), Doc { n: 2 });
        assert!(!dir.join("store.json.tmp").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_store_quarantines_and_defaults() {
        let dir = tmpdir("corrupt");
        let path = dir.join("store.json");
        std::fs::write(&path, b"{\"n\":").unwrap();
        assert_eq!(load_json::<Doc>(&path), Doc::default());
        assert!(!path.exists(), "corrupt file moved aside");
        assert_eq!(
            std::fs::read(dir.join("store.json.corrupt")).unwrap(),
            b"{\"n\":",
            "corrupt bytes preserved"
        );
        // Missing file stays a quiet default, no quarantine.
        assert_eq!(load_json::<Doc>(&path), Doc::default());
        let _ = std::fs::remove_dir_all(dir);
    }
}
