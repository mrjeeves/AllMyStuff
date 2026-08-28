//! Windows' built-in Cloud Files shell registration for the canonical Fleetfiles root.
//!
//! Fleetfiles is already materialized locally and watched by the replication engine.
//! Registering that directory as a sync root gives Explorer a stable, branded,
//! top-level location without sending ordinary file I/O through the WebDAV redirector.

use std::path::{Path, PathBuf};

use windows::core::{GUID, HSTRING};
use windows::Storage::{
    Provider::{
        StorageProviderHardlinkPolicy, StorageProviderHydrationPolicy,
        StorageProviderHydrationPolicyModifier, StorageProviderInSyncPolicy,
        StorageProviderPopulationPolicy, StorageProviderSyncRootInfo,
        StorageProviderSyncRootManager,
    },
    StorageFolder,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

const PROVIDER_NAME: &str = "AllMyStuff";
const ACCOUNT_NAME: &str = "Fleetfiles";
const DISPLAY_NAME: &str = "Fleetfiles";
const PROVIDER_ID: GUID = GUID::from_u128(0x8b7337b1_0c39_4e28_a88d_28c6698b95f1);
const ICON_BYTES: &[u8] = include_bytes!("../../gui/src-tauri/icons/icon.ico");

struct ComApartment;

impl ComApartment {
    fn enter() -> Result<Self, String> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|error| format!("couldn't initialize Windows Cloud Files: {error}"))?;
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

pub fn register(root: PathBuf) -> Result<String, String> {
    if !root.is_absolute() {
        return Err("the Fleetfiles sync root must be an absolute path".into());
    }
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("couldn't create the Fleetfiles sync root: {error}"))?;
    let icon = write_icon(&root)?;
    crate::win_privilege::interactive_user_call("ams-fleetfiles-sync-root", move || {
        let sid = crate::win_privilege::effective_user_sid()?;
        register_for_current_user(&root, &icon, &sid)
    })
}

fn register_for_current_user(root: &Path, icon: &Path, sid: &str) -> Result<String, String> {
    let _apartment = ComApartment::enter()?;
    let supported = StorageProviderSyncRootManager::IsSupported()
        .map_err(|error| format!("couldn't query Windows Cloud Files support: {error}"))?;
    if !supported {
        return Err("Windows Cloud Files sync roots are unavailable on this system".into());
    }

    let id = HSTRING::from(sync_root_id(sid));
    let root_text = root.to_string_lossy().into_owned();
    if let Ok(existing) = StorageProviderSyncRootManager::GetSyncRootInformationForId(&id) {
        let existing_path = existing
            .Path()
            .and_then(|folder| folder.Path())
            .map_err(|error| {
                format!("couldn't inspect the existing Fleetfiles sync root: {error}")
            })?
            .to_string();
        if same_windows_path(&existing_path, &root_text) {
            return Ok(root_text);
        }
        StorageProviderSyncRootManager::Unregister(&id)
            .map_err(|error| format!("couldn't replace the old Fleetfiles sync root: {error}"))?;
    }

    let root_hstring = HSTRING::from(root_text.as_str());
    let folder = StorageFolder::GetFolderFromPathAsync(&root_hstring)
        .and_then(|operation| operation.get())
        .map_err(|error| format!("couldn't open the Fleetfiles sync root for Explorer: {error}"))?;
    let info = StorageProviderSyncRootInfo::new().map_err(|error| {
        format!("couldn't create the Fleetfiles sync-root description: {error}")
    })?;
    info.SetId(&id)
        .map_err(|error| format!("couldn't set the Fleetfiles sync-root id: {error}"))?;
    info.SetPath(&folder)
        .map_err(|error| format!("couldn't set the Fleetfiles sync-root path: {error}"))?;
    info.SetDisplayNameResource(&HSTRING::from(DISPLAY_NAME))
        .map_err(|error| format!("couldn't set the Fleetfiles display name: {error}"))?;
    info.SetIconResource(&HSTRING::from(format!("{},0", icon.display())))
        .map_err(|error| format!("couldn't set the Fleetfiles icon: {error}"))?;
    info.SetHydrationPolicy(StorageProviderHydrationPolicy::Full)
        .map_err(|error| format!("couldn't set the Fleetfiles hydration policy: {error}"))?;
    info.SetHydrationPolicyModifier(StorageProviderHydrationPolicyModifier::None)
        .map_err(|error| format!("couldn't set the Fleetfiles hydration modifiers: {error}"))?;
    info.SetPopulationPolicy(StorageProviderPopulationPolicy::AlwaysFull)
        .map_err(|error| format!("couldn't set the Fleetfiles population policy: {error}"))?;
    info.SetInSyncPolicy(
        StorageProviderInSyncPolicy::FileCreationTime
            | StorageProviderInSyncPolicy::DirectoryCreationTime,
    )
    .map_err(|error| format!("couldn't set the Fleetfiles in-sync policy: {error}"))?;
    info.SetVersion(&HSTRING::from(env!("CARGO_PKG_VERSION")))
        .map_err(|error| format!("couldn't set the Fleetfiles provider version: {error}"))?;
    info.SetShowSiblingsAsGroup(false)
        .map_err(|error| format!("couldn't set the Fleetfiles navigation grouping: {error}"))?;
    info.SetHardlinkPolicy(StorageProviderHardlinkPolicy::None)
        .map_err(|error| format!("couldn't set the Fleetfiles hardlink policy: {error}"))?;
    info.SetAllowPinning(true)
        .map_err(|error| format!("couldn't enable Fleetfiles pinning: {error}"))?;
    info.SetProviderId(PROVIDER_ID)
        .map_err(|error| format!("couldn't set the Fleetfiles provider id: {error}"))?;
    StorageProviderSyncRootManager::Register(&info)
        .map_err(|error| format!("Windows couldn't register Fleetfiles with Explorer: {error}"))?;
    Ok(root_text)
}

fn write_icon(_root: &Path) -> Result<PathBuf, String> {
    let state = allmystuff_protocol::myownmesh_state_dir()
        .ok_or_else(|| "the Fleetfiles state directory is unavailable".to_string())?;
    std::fs::create_dir_all(&state)
        .map_err(|error| format!("couldn't create the Fleetfiles state directory: {error}"))?;
    let icon = state.join(".allmystuff-fleetfiles.ico");
    let current = std::fs::read(&icon).ok();
    if current.as_deref() != Some(ICON_BYTES) {
        std::fs::write(&icon, ICON_BYTES)
            .map_err(|error| format!("couldn't write the Fleetfiles Explorer icon: {error}"))?;
    }
    Ok(icon)
}

fn sync_root_id(sid: &str) -> String {
    format!("{PROVIDER_NAME}!{sid}!{ACCOUNT_NAME}")
}

fn same_windows_path(left: &str, right: &str) -> bool {
    left.trim_end_matches(['\\', '/'])
        .eq_ignore_ascii_case(right.trim_end_matches(['\\', '/']))
}

#[cfg(test)]
mod tests {
    use super::{same_windows_path, sync_root_id};

    #[test]
    fn sync_root_identity_is_stable_per_windows_user() {
        assert_eq!(
            sync_root_id("S-1-5-21-1-2-3-1001"),
            "AllMyStuff!S-1-5-21-1-2-3-1001!Fleetfiles"
        );
    }

    #[test]
    fn existing_root_match_is_case_and_separator_insensitive() {
        assert!(same_windows_path(
            r"C:\Users\Chris\Fleetfiles\Desktop\",
            r"c:\users\chris\fleetfiles\desktop"
        ));
    }
}
