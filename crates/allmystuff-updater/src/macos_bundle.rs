use super::{Error, Result};
use std::path::{Component, Path, PathBuf};

/// Only the desktop executable's exact bundle layout is eligible. Sidecars
/// inside a bundle are replaced together with the signed app, never singly.
pub(super) fn bundle_for_executable(executable: &Path) -> Option<PathBuf> {
    if executable.file_name()?.to_str()? != "allmystuff-gui" {
        return None;
    }
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()?.to_str()? == "MacOS"
        && contents.file_name()?.to_str()? == "Contents"
        && bundle.extension()?.to_str()? == "app")
        .then(|| bundle.to_path_buf())
}

pub(super) fn replace_bundle(
    archive: &Path,
    executable: &Path,
    verify: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let target = bundle_for_executable(executable)
        .ok_or_else(|| Error::msg("invalid desktop bundle path"))?;
    let parent = target
        .parent()
        .ok_or_else(|| Error::msg("bundle has no parent"))?;
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let staging = parent.join(format!(".allmystuff-stage-{nonce}"));
    let backup = parent.join(format!(".allmystuff-previous-{nonce}"));
    std::fs::create_dir(&staging)?;
    struct Staging(PathBuf);
    impl Drop for Staging {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _staging = Staging(staging.clone());
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(archive)?));
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if !path.starts_with("AllMyStuff.app")
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
        {
            return Err(Error::msg(
                "bundle archive contains a path outside AllMyStuff.app",
            ));
        }
        if !entry.unpack_in(&staging)? {
            return Err(Error::msg("couldn't safely extract the desktop bundle"));
        }
    }
    let staged = staging.join("AllMyStuff.app");
    if !staged.join("Contents/MacOS/allmystuff-gui").is_file()
        || !staged.join("Contents/Info.plist").is_file()
    {
        return Err(Error::msg(
            "release archive has no complete desktop app bundle",
        ));
    }
    verify(&staged)?;
    std::fs::rename(&target, &backup)?;
    if let Err(error) = std::fs::rename(&staged, &target) {
        if let Err(rollback) = std::fs::rename(&backup, &target) {
            return Err(Error::msg(format!(
                "install desktop bundle: {error}; restore {backup:?}: {rollback}"
            )));
        }
        return Err(error.into());
    }
    // This is the old app directory just moved above, after the verified
    // replacement is installed. Never clean a pre-existing backup or path.
    if let Err(error) = std::fs::remove_dir_all(&backup) {
        tracing::warn!(%error, "couldn't remove previous desktop bundle");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn archive(path: &Path) {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            std::fs::File::create(path).unwrap(),
            flate2::Compression::default(),
        ));
        for (name, data) in [
            ("Contents/MacOS/allmystuff-gui", b"new".as_slice()),
            ("Contents/Info.plist", b"info".as_slice()),
            ("Contents/MacOS/allmystuff-serve", b"node".as_slice()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, format!("AllMyStuff.app/{name}"), data)
                .unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn complete_bundle_replaces_gui_resources_and_sidecars_after_verification() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("Custom.app/Contents/MacOS/allmystuff-gui");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, b"old").unwrap();
        let release = root.path().join("bundle.tar.gz");
        archive(&release);
        assert!(
            replace_bundle(&release, &executable, |_| Err(Error::msg("bad signature"))).is_err()
        );
        assert_eq!(std::fs::read(&executable).unwrap(), b"old");
        replace_bundle(&release, &executable, |bundle| {
            assert!(bundle.join("Contents/Info.plist").is_file());
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read(&executable).unwrap(), b"new");
        assert_eq!(
            std::fs::read(executable.with_file_name("allmystuff-serve")).unwrap(),
            b"node"
        );
        assert!(bundle_for_executable(&executable.with_file_name("allmystuff-serve")).is_none());
        assert!(bundle_for_executable(Path::new("/tmp/Fake.app/allmystuff-gui")).is_none());
    }
}
