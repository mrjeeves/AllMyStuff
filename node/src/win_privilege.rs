//! Windows privilege posture: whether a support session can actually reach the
//! machine's repair tooling, and the desktop plumbing that lets it.
//!
//! ## The problem this exists to solve
//!
//! A remote-support session that runs as the logged-in user is locked out of
//! exactly the things a repair needs, by two separate Windows mechanisms that
//! look identical from the technician's chair (the click does nothing):
//!
//! 1. **UIPI** (User Interface Privilege Isolation). `SendInput` from a
//!    medium-integrity process is *silently discarded* by any window owned by a
//!    higher-integrity process. Event Viewer, Services, Device Manager,
//!    regedit, an administrator PowerShell — all of them run high-integrity
//!    when elevated. The technician can see the window and cannot type in it.
//!    No error is raised anywhere; the input simply evaporates.
//! 2. **The secure desktop**. UAC's consent dialog does not run on the ordinary
//!    `Default` desktop — it runs on a separate desktop object (`Winlogon`)
//!    whose DACL admits `SYSTEM` only. A process attached to `Default` can
//!    neither capture it (DXGI hands back `DXGI_ERROR_ACCESS_LOST`, which is
//!    why the stream appears to freeze at the exact moment a UAC prompt opens)
//!    nor inject into it.
//!
//! The two need *different* privilege, which is why this module reports them
//! separately rather than as one "am I admin" boolean:
//!
//! | Reach | Needs | Gets you |
//! |---|---|---|
//! | [`Posture::can_drive_elevated_windows`] | High integrity, **or** UIAccess | Event Viewer, Services, regedit, admin shells |
//! | [`Posture::can_follow_secure_desktop`] | `SYSTEM` integrity | the UAC consent dialog itself |
//!
//! Most repairs need the first. Clicking "Yes" on UAC for the customer needs
//! the second, and that is only reachable when the session is hosted by the
//! `LocalSystem` background service.
//!
//! ## What this module is not
//!
//! Nothing here decides *whether* someone may drive this machine. That is the
//! ordinary route/ownership gate the mesh already applies before an event ever
//! reaches the injector; a session that passes it gets the reach the host
//! process has, and this module's job is only to make sure that reach isn't
//! silently thrown away by UIPI or lost on a desktop switch. A posture is a
//! capability of the *process*, never an authorization.
//!
//! ## Cost
//!
//! [`DesktopFollower::follow`] is three Win32 calls even when nothing changed,
//! so it is not free to call per event. The injector rate-limits it; capture
//! calls it only when DXGI reports `ACCESS_LOST`, which *is* the desktop
//! switch. Neither path runs it per frame.

/// Windows' mandatory integrity levels, ordered so comparisons read the way
/// UIPI actually works: a process may synthesize input into a window whose
/// integrity is **at or below** its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Integrity {
    /// Untrusted (`S-1-16-0`) — sandboxed to nothing.
    Untrusted,
    /// Low (`S-1-16-4096`) — a browser renderer, protected-mode content.
    Low,
    /// Medium (`S-1-16-8192`) — an ordinary interactive user process. **This is
    /// where a support agent lands by default, and why it is locked out.**
    #[default]
    Medium,
    /// High (`S-1-16-12288`) — elevated ("Run as administrator").
    High,
    /// System (`S-1-16-16384`) — `LocalSystem`, e.g. a service.
    System,
}

impl Integrity {
    /// Classify a mandatory-label SID's RID. Unrecognised values round *down*
    /// to the nearest known level rather than up, so an unfamiliar token is
    /// never mistaken for a more privileged one than it is.
    pub fn from_rid(rid: u32) -> Integrity {
        // SECURITY_MANDATORY_*_RID, from winnt.h.
        match rid {
            r if r >= 0x4000 => Integrity::System,
            r if r >= 0x3000 => Integrity::High,
            r if r >= 0x2000 => Integrity::Medium,
            r if r >= 0x1000 => Integrity::Low,
            _ => Integrity::Untrusted,
        }
    }

    /// The label a support log or the customer's UI should show.
    pub fn label(self) -> &'static str {
        match self {
            Integrity::Untrusted => "untrusted",
            Integrity::Low => "low",
            Integrity::Medium => "medium (standard user)",
            Integrity::High => "high (administrator)",
            Integrity::System => "system",
        }
    }
}

/// What the agent process **is** right now — not what it is allowed to do.
///
/// Deliberately a plain value with no handles in it: it is sampled once, passed
/// around freely, reported on the wire, and unit-tested on every platform.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Posture {
    /// The process token's mandatory integrity level.
    pub integrity: Integrity,
    /// The token's elevation flag (`TokenElevation`). Distinct from integrity:
    /// with UAC turned off entirely, a process can be "not elevated" by this
    /// flag and still run at high integrity, which is why input reach keys on
    /// integrity and this is carried only for diagnostics.
    pub elevated: bool,
    /// The token's `UIAccess` flag. Set by a manifest `uiAccess="true"`, which
    /// Windows honours **only** for an Authenticode-signed binary installed
    /// under a trusted path (`%ProgramFiles%`). It exempts the process from
    /// UIPI without making it an administrator — the accessibility-tool route
    /// to driving elevated windows.
    pub ui_access: bool,
}

impl Posture {
    /// A plain interactive user process: the default a support agent gets, and
    /// the one that cannot do the job.
    pub fn standard_user() -> Posture {
        Posture {
            integrity: Integrity::Medium,
            elevated: false,
            ui_access: false,
        }
    }

    /// Whether this process's synthesized input will actually land in an
    /// elevated window (Event Viewer, Services, regedit, an admin shell)
    /// instead of being dropped by UIPI.
    ///
    /// High integrity clears it because UIPI compares levels; `UIAccess` clears
    /// it because that is precisely the exemption the flag exists to grant.
    pub fn can_drive_elevated_windows(self) -> bool {
        self.integrity >= Integrity::High || self.ui_access
    }

    /// Whether this process can attach to the **secure desktop** — the separate
    /// desktop UAC's consent dialog runs on — and so keep the screen stream
    /// alive and clickable across a UAC prompt.
    ///
    /// `SYSTEM` only: the `Winlogon` desktop's DACL does not admit even an
    /// elevated administrator. This is the reach that requires the session be
    /// hosted by the `LocalSystem` service rather than launched from the
    /// customer's own desktop session.
    pub fn can_follow_secure_desktop(self) -> bool {
        self.integrity >= Integrity::System
    }
}

/// Build a Windows command line with the executable and every argument quoted.
///
/// Hoisted out of the Win32 module and kept pure so it is tested on every
/// platform, not just on the one CI runner that compiles the `cfg(windows)`
/// half. The bug it exists to prevent is not hypothetical: this is launched
/// from `C:\Program Files\…`, and an unquoted path there silently becomes two
/// arguments, so the agent fails to start on exactly the installs that matter
/// and works fine in every dev checkout.
///
/// Off Windows only the tests call it — which is the entire reason it lives out
/// here rather than inside the `cfg(windows)` module.
#[cfg_attr(not(windows), allow(dead_code))]
fn quote_command(exe: &std::path::Path, args: &[&str]) -> String {
    let mut s = format!("\"{}\"", exe.display());
    for a in args {
        s.push(' ');
        s.push('"');
        s.push_str(&a.replace('"', "\\\""));
        s.push('"');
    }
    s
}

// ---------------------------------------------------------------------------
// Windows backend
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_BAD_DEVICE, ERROR_FILE_NOT_FOUND,
        ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA, ERROR_NOT_CONNECTED, ERROR_PATH_NOT_FOUND,
        FALSE, HANDLE, NO_ERROR, TRUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::NetworkManagement::WNet::{
        NETRESOURCEW, RESOURCETYPE_DISK, WNetAddConnection2W, WNetCancelConnection2W,
        WNetGetConnectionW,
    };
    use windows_sys::Win32::Security::{
        DuplicateTokenEx, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
        ImpersonateLoggedOnUser, RevertToSelf, SecurityImpersonation, SetTokenInformation,
        TokenElevation, TokenIntegrityLevel, TokenPrimary, TokenSessionId, TokenUIAccess,
        TOKEN_ALL_ACCESS, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_ELEVATION,
        TOKEN_IMPERSONATE, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows_sys::Win32::Storage::FileSystem::{GetLogicalDrives, QueryDosDeviceW};
    use windows_sys::Win32::System::RemoteDesktop::{
        WTSEnumerateProcessesW, WTSFreeMemory, WTSGetActiveConsoleSessionId,
        WTS_CURRENT_SERVER_HANDLE, WTS_PROCESS_INFOW,
    };
    use windows_sys::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessAsUserW, GetCurrentProcess, OpenProcess, OpenProcessToken, TerminateProcess,
        WaitForSingleObject, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
        PROCESS_QUERY_LIMITED_INFORMATION, STARTUPINFOW,
    };

    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetShellWindow, GetWindowThreadProcessId,
    };
    use super::{Integrity, Posture};

    /// `UOI_NAME` — ask `GetUserObjectInformationW` for an object's name.
    const UOI_NAME: u32 = 2;

    /// The desktop rights the capture and input threads need together: read the
    /// desktop's objects (capture), write to them (`SendInput`), and enumerate.
    /// Requested as one set so a single handle serves both callers.
    const DESKTOP_READOBJECTS: u32 = 0x0001;
    const DESKTOP_CREATEWINDOW: u32 = 0x0002;
    const DESKTOP_CREATEMENU: u32 = 0x0004;
    const DESKTOP_HOOKCONTROL: u32 = 0x0008;
    const DESKTOP_JOURNALRECORD: u32 = 0x0010;
    const DESKTOP_JOURNALPLAYBACK: u32 = 0x0020;
    const DESKTOP_ENUMERATE: u32 = 0x0040;
    const DESKTOP_WRITEOBJECTS: u32 = 0x0080;
    const DESKTOP_SWITCHDESKTOP: u32 = 0x0100;
    const DESKTOP_ALL: u32 = DESKTOP_READOBJECTS
        | DESKTOP_CREATEWINDOW
        | DESKTOP_CREATEMENU
        | DESKTOP_HOOKCONTROL
        | DESKTOP_JOURNALRECORD
        | DESKTOP_JOURNALPLAYBACK
        | DESKTOP_ENUMERATE
        | DESKTOP_WRITEOBJECTS
        | DESKTOP_SWITCHDESKTOP;

    /// Read one fixed-size field out of the current process token.
    ///
    /// # Safety
    /// `T` must be the exact struct Windows documents for `class`; the caller
    /// gets a zeroed value back if the query fails, never uninitialised memory.
    unsafe fn token_field<T: Copy + Default>(class: i32) -> Option<T> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: TOKEN_QUERY on our own process; `token` is written only on success.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return None;
        }
        let mut value = T::default();
        let mut returned = 0u32;
        // SAFETY: `value` is exactly `size_of::<T>()` bytes of writable storage.
        let ok = unsafe {
            GetTokenInformation(
                token,
                class,
                &mut value as *mut T as *mut c_void,
                std::mem::size_of::<T>() as u32,
                &mut returned,
            )
        };
        // SAFETY: `token` came from a successful OpenProcessToken above.
        unsafe { CloseHandle(token) };
        (ok != 0).then_some(value)
    }

    /// The process token's mandatory integrity level.
    ///
    /// Unlike the fixed-size fields above, `TokenIntegrityLevel` returns a
    /// variable-length `TOKEN_MANDATORY_LABEL` (it carries a SID), so this
    /// sizes the buffer from the failed first call.
    fn integrity_level() -> Integrity {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: TOKEN_QUERY on our own process.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Integrity::Medium;
        }
        let mut needed = 0u32;
        // First call fails with ERROR_INSUFFICIENT_BUFFER and sets `needed`.
        // SAFETY: a null buffer with zero length is the documented sizing call.
        unsafe {
            GetTokenInformation(
                token,
                TokenIntegrityLevel,
                std::ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        if needed == 0 {
            // SAFETY: `token` came from a successful OpenProcessToken.
            unsafe { CloseHandle(token) };
            return Integrity::Medium;
        }
        let mut buf = vec![0u8; needed as usize];
        // SAFETY: `buf` is `needed` writable bytes, the size Windows just asked for.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenIntegrityLevel,
                buf.as_mut_ptr() as *mut c_void,
                needed,
                &mut needed,
            )
        };
        // SAFETY: `token` came from a successful OpenProcessToken.
        unsafe { CloseHandle(token) };
        if ok == 0 {
            return Integrity::Medium;
        }
        // SAFETY: on success `buf` holds a TOKEN_MANDATORY_LABEL whose `Label.Sid`
        // points into Windows-owned memory valid for the life of the buffer.
        unsafe {
            let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
            let sid = label.Label.Sid;
            if sid.is_null() {
                return Integrity::Medium;
            }
            let count = GetSidSubAuthorityCount(sid);
            if count.is_null() || *count == 0 {
                return Integrity::Medium;
            }
            // The integrity RID is the SID's last sub-authority.
            let rid = *GetSidSubAuthority(sid, (*count - 1) as u32);
            Integrity::from_rid(rid)
        }
    }

    /// Sample this process's privilege posture.
    pub fn current_posture() -> Posture {
        // SAFETY: TOKEN_ELEVATION is the documented struct for TokenElevation.
        let elevated = unsafe { token_field::<TOKEN_ELEVATION>(TokenElevation) }
            .map(|e| e.TokenIsElevated != 0)
            .unwrap_or(false);
        // SAFETY: TokenUIAccess returns a DWORD.
        let ui_access = unsafe { token_field::<u32>(TokenUIAccess) }
            .map(|v| v != 0)
            .unwrap_or(false);
        Posture {
            integrity: integrity_level(),
            elevated,
            ui_access,
        }
    }

    /// The session id of the physical console — the desktop a human is sitting
    /// at. A service running in session 0 has no desktop of its own and must
    /// host the session here instead.
    pub fn active_console_session() -> u32 {
        // SAFETY: no arguments, no preconditions.
        unsafe { WTSGetActiveConsoleSessionId() }
    }

    /// Drive letters visible to the signed-in user's Explorer token.
    ///
    /// UAC gives elevated and ordinary processes separate DOS-device maps.
    /// The service has the same split because it retains its SYSTEM token even
    /// after moving into the console session. Looking only at our own
    /// `GetLogicalDrives` result can therefore select a letter already used on
    /// the actual desktop. Impersonate Explorer only for this read and return
    /// its bitmask; no process is launched and no mapping is contacted.
    pub fn interactive_user_logical_drive_mask() -> Result<u32, String> {
        // Token impersonation is thread-local. Isolate it on a short-lived OS
        // thread so even a pathological RevertToSelf failure cannot leak the
        // desktop user's token onto a long-lived async executor worker.
        std::thread::Builder::new()
            .name("ams-drive-namespace".into())
            .spawn(read_interactive_user_logical_drive_mask)
            .map_err(|error| format!("couldn't start the drive namespace check: {error}"))?
            .join()
            .map_err(|_| "the drive namespace check stopped unexpectedly".to_string())?
    }

    fn read_interactive_user_logical_drive_mask() -> Result<u32, String> {
        with_interactive_user(|| {
            let mask = unsafe { GetLogicalDrives() };
            (mask != 0)
                .then_some(mask)
                .ok_or_else(|| "couldn't inspect the signed-in user's drive letters".into())
        })
        .map(|mask| mask.unwrap_or(0))
    }

    /// Resolve a drive in the signed-in Explorer user's DOS-device namespace.
    pub fn interactive_user_network_mapping(mount: &str) -> Result<Option<String>, String> {
        let mount = mount.to_string();
        std::thread::Builder::new()
            .name("ams-drive-lookup".into())
            .spawn(move || {
                with_interactive_user(|| read_network_mapping(&mount)).map(Option::flatten)
            })
            .map_err(|error| format!("couldn't start the drive mapping check: {error}"))?
            .join()
            .map_err(|_| "the drive mapping check stopped unexpectedly".to_string())?
    }

    /// Read the DOS-device targets behind a drive in this process's logon
    /// namespace. Unlike opening the drive or asking WebDAV to reconnect, this
    /// is a local object-manager lookup and remains reliable when the old
    /// localhost bridge died in a crash.
    pub fn dos_device_targets(mount: &str) -> Result<Vec<String>, String> {
        read_dos_device_targets(mount)
    }

    /// Read the same object-manager identity in the signed-in Explorer user's
    /// namespace. Elevated/service and desktop mappings can be different.
    pub fn interactive_user_dos_device_targets(mount: &str) -> Result<Vec<String>, String> {
        let mount = mount.to_string();
        std::thread::Builder::new()
            .name("ams-drive-device-lookup".into())
            .spawn(move || {
                with_interactive_user(|| read_dos_device_targets(&mount))
                    .map(|targets| targets.unwrap_or_default())
            })
            .map_err(|error| format!("couldn't start the drive device check: {error}"))?
            .join()
            .map_err(|_| "the drive device check stopped unexpectedly".to_string())?
    }

    fn read_dos_device_targets(mount: &str) -> Result<Vec<String>, String> {
        let local = mount
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut targets = vec![0_u16; 1024];
        loop {
            let length = unsafe {
                QueryDosDeviceW(local.as_ptr(), targets.as_mut_ptr(), targets.len() as u32)
            };
            if length != 0 {
                return Ok(targets[..length as usize]
                    .split(|unit| *unit == 0)
                    .filter(|target| !target.is_empty())
                    .map(String::from_utf16_lossy)
                    .collect());
            }
            let error = unsafe { GetLastError() };
            match error {
                ERROR_INSUFFICIENT_BUFFER if targets.len() < 32_768 => {
                    targets.resize((targets.len() * 2).min(32_768), 0);
                }
                ERROR_BAD_DEVICE | ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => {
                    return Ok(Vec::new());
                }
                _ => {
                    return Err(format!(
                        "Windows couldn't inspect the DOS-device target for {mount} (error {error})"
                    ));
                }
            }
        }
    }

    /// Create a temporary drive mapping in the signed-in Explorer user's
    /// logon namespace. A service or scheduled task has a different DOS-device
    /// map; running `net use` there produces a drive the user cannot see and
    /// leaks one private-session mapping on every restart.
    pub fn connect_interactive_user_network_mapping(
        mount: &str,
        remote: &str,
    ) -> Result<(), String> {
        let mount = mount.to_string();
        let remote = remote.to_string();
        std::thread::Builder::new()
            .name("ams-drive-connect".into())
            .spawn(move || {
                with_interactive_user(|| {
                    let mut local = mount
                        .encode_utf16()
                        .chain(std::iter::once(0))
                        .collect::<Vec<_>>();
                    let mut remote_wide = remote
                        .encode_utf16()
                        .chain(std::iter::once(0))
                        .collect::<Vec<_>>();
                    let resource = NETRESOURCEW {
                        dwType: RESOURCETYPE_DISK,
                        lpLocalName: local.as_mut_ptr(),
                        lpRemoteName: remote_wide.as_mut_ptr(),
                        ..Default::default()
                    };
                    let status = unsafe {
                        WNetAddConnection2W(
                            &resource,
                            std::ptr::null(),
                            std::ptr::null(),
                            0,
                        )
                    };
                    (status == NO_ERROR).then_some(()).ok_or_else(|| {
                        format!(
                            "Windows couldn't connect {mount} to {remote} in the signed-in user's drive namespace (error {status})"
                        )
                    })
                })
            })
            .map_err(|error| format!("couldn't start the drive connection: {error}"))?
            .join()
            .map_err(|_| "the drive connection stopped unexpectedly".to_string())?
            .and_then(|connected| {
                connected.ok_or_else(|| {
                    "there is no signed-in Explorer session to receive the drive mapping".into()
                })
            })
    }
    fn read_network_mapping(mount: &str) -> Result<Option<String>, String> {
        let local = mount
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut remote = vec![0_u16; 512];
        loop {
            let mut length = remote.len() as u32;
            let status =
                unsafe { WNetGetConnectionW(local.as_ptr(), remote.as_mut_ptr(), &mut length) };
            match status {
                NO_ERROR => {
                    let end = remote
                        .iter()
                        .position(|unit| *unit == 0)
                        .unwrap_or(remote.len());
                    return Ok(Some(String::from_utf16_lossy(&remote[..end])));
                }
                ERROR_MORE_DATA => {
                    let required = usize::try_from(length)
                        .map_err(|_| "Windows returned an invalid network path length")?;
                    if required == 0 || required > 32_768 {
                        return Err("Windows returned an invalid network path length".into());
                    }
                    remote.resize(required.saturating_add(1), 0);
                }
                ERROR_BAD_DEVICE | ERROR_NOT_CONNECTED => return Ok(None),
                error => {
                    return Err(format!(
                        "Windows couldn't inspect {mount} in the signed-in user's drive namespace (error {error})"
                    ));
                }
            }
        }
    }

    /// Disconnect a mapping only while it still resolves to the exact endpoint
    /// the caller proved belongs to AllMyStuff. Keep the identity check and
    /// cancellation under one Explorer-token impersonation window so a drive
    /// letter reused by another application is never cancelled from a stale
    /// observation made by the service session.
    pub fn disconnect_interactive_user_network_mapping_if_matches(
        mount: &str,
        expected_remote: &str,
    ) -> Result<bool, String> {
        let mount = mount.to_string();
        let expected_remote = expected_remote.to_string();
        std::thread::Builder::new()
            .name("ams-drive-disconnect".into())
            .spawn(move || {
                with_interactive_user(|| {
                    let Some(remote) = read_network_mapping(&mount)? else {
                        return Ok(false);
                    };
                    if !remote
                        .trim_end_matches(['\\', '/'])
                        .eq_ignore_ascii_case(expected_remote.trim_end_matches(['\\', '/']))
                    {
                        return Ok(false);
                    }
                    let local = mount
                        .encode_utf16()
                        .chain(std::iter::once(0))
                        .collect::<Vec<_>>();
                    let status = unsafe { WNetCancelConnection2W(local.as_ptr(), 0, TRUE) };
                    match status {
                        NO_ERROR => Ok(true),
                        ERROR_BAD_DEVICE | ERROR_NOT_CONNECTED => Ok(false),
                        error => Err(format!(
                            "Windows couldn't disconnect {mount} in the signed-in user's drive namespace (error {error})"
                        )),
                    }
                })
                .map(|removed| removed.unwrap_or(false))
            })
            .map_err(|error| format!("couldn't start the drive disconnect: {error}"))?
            .join()
            .map_err(|_| "the drive disconnect stopped unexpectedly".to_string())?
    }

    /// Run one bounded operation as the signed-in Explorer user. Every public
    /// caller uses a disposable OS thread so a failed revert cannot leak the
    /// desktop user's token onto a long-lived async executor worker.
    fn with_interactive_user<T>(
        read: impl FnOnce() -> Result<T, String>,
    ) -> Result<Option<T>, String> {
        let Some(process_id) = interactive_explorer_process_id()? else {
            return Ok(None);
        };
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, process_id) };
        if process.is_null() {
            return Err("couldn't inspect the signed-in user's Explorer process".into());
        }
        let mut token: HANDLE = std::ptr::null_mut();
        let opened = unsafe {
            OpenProcessToken(
                process,
                TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE,
                &mut token,
            )
        };
        unsafe { CloseHandle(process) };
        if opened == 0 {
            return Err("couldn't inspect the signed-in user's Explorer token".into());
        }
        if unsafe { ImpersonateLoggedOnUser(token) } == 0 {
            unsafe { CloseHandle(token) };
            return Err("couldn't enter the signed-in user's drive namespace".into());
        }
        let result = read();
        let reverted = unsafe { RevertToSelf() };
        unsafe { CloseHandle(token) };
        if reverted == 0 {
            return Err("couldn't leave the signed-in user's drive namespace".into());
        }
        result.map(Some)
    }


    /// Find Explorer in the physical console session even when the caller is a
    /// service or scheduled task in a different Windows session. GetShellWindow
    /// only sees the caller's window station and is therefore merely a fallback.
    fn interactive_explorer_process_id() -> Result<Option<u32>, String> {
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if session == NO_SESSION {
            return Ok(None);
        }

        let mut processes: *mut WTS_PROCESS_INFOW = std::ptr::null_mut();
        let mut count = 0_u32;
        let enumerated = unsafe {
            WTSEnumerateProcessesW(
                WTS_CURRENT_SERVER_HANDLE,
                0,
                1,
                &mut processes,
                &mut count,
            )
        };
        if enumerated != 0 {
            let process_id = if processes.is_null() || count == 0 {
                None
            } else {
                let entries =
                    unsafe { std::slice::from_raw_parts(processes, count as usize) };
                entries
                    .iter()
                    .find(|entry| {
                        entry.SessionId == session
                            && wide_process_name_is(entry.pProcessName, "explorer.exe")
                    })
                    .map(|entry| entry.ProcessId)
                    .filter(|process_id| *process_id != 0)
            };
            if !processes.is_null() {
                unsafe { WTSFreeMemory(processes.cast()) };
            }
            return Ok(process_id);
        }

        // Restricted desktop builds can deny WTS enumeration while still
        // allowing a same-session process to inspect its own shell.
        let shell = unsafe { GetShellWindow() };
        if shell.is_null() {
            return Err("couldn't enumerate the signed-in user's Explorer process".into());
        }
        let mut process_id = 0;
        if unsafe { GetWindowThreadProcessId(shell, &mut process_id) } == 0 || process_id == 0 {
            return Err("couldn't identify the signed-in user's Explorer process".into());
        }
        Ok(Some(process_id))
    }

    fn wide_process_name_is(name: *const u16, expected: &str) -> bool {
        if name.is_null() {
            return false;
        }
        let mut length = 0;
        while length < 260 && unsafe { *name.add(length) } != 0 {
            length += 1;
        }
        let name = unsafe { std::slice::from_raw_parts(name, length) };
        String::from_utf16_lossy(name).eq_ignore_ascii_case(expected)
    }
    /// `WTSGetActiveConsoleSessionId` returns this when no session is attached
    /// to the console — between a logoff and the next logon, or on a box whose
    /// session is being transferred.
    const NO_SESSION: u32 = 0xFFFF_FFFF;

    /// Launch `exe args` as **`SYSTEM` in the interactive console session**.
    ///
    /// This is the whole point of the background service. A service runs in
    /// session 0, which since Vista has no desktop at all — nothing there can
    /// capture a screen or synthesize input, however privileged it is. Meanwhile
    /// a process launched from the customer's own desktop has a desktop but only
    /// medium integrity, so Windows discards its input into anything elevated.
    /// Neither half can do the job alone.
    ///
    /// Duplicating the service's own `SYSTEM` token and retargeting it at the
    /// console session produces the process that can: `SYSTEM` integrity *and*
    /// a real desktop. That is what makes elevated windows clickable and the
    /// secure desktop reachable.
    ///
    /// Requires `SeTcbPrivilege` to retarget the session, which `LocalSystem`
    /// holds and an ordinary administrator does not — so this only ever
    /// succeeds from the service, which is exactly the intended constraint.
    ///
    /// The child inherits the service's environment deliberately (null `lpEnvironment`):
    /// its state must live under the service's `*_HOME`, not the console user's,
    /// so a machine with several logins keeps one agent identity rather than
    /// minting a new node per user.
    ///
    /// Returns the child's process handle, which the caller owns and must close
    /// (see [`ConsoleAgent`], which does).
    pub fn launch_in_console_session(exe: &Path, args: &[&str]) -> Result<*mut c_void, String> {
        let session = active_console_session();
        if session == NO_SESSION {
            return Err("no interactive console session is attached".into());
        }
        if session == 0 {
            // Session 0 is the service session — launching there would reproduce
            // the exact desktop-less situation this function exists to escape.
            return Err("the console session is session 0 (no interactive desktop)".into());
        }

        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: our own process; the access mask is what DuplicateTokenEx +
        // CreateProcessAsUser need.
        if unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY,
                &mut token,
            )
        } == 0
        {
            return Err("opening this process's token failed".into());
        }

        let mut primary: HANDLE = std::ptr::null_mut();
        // SAFETY: `token` is live; a primary token is what CreateProcessAsUser
        // requires (an impersonation token is rejected).
        let dup_ok = unsafe {
            DuplicateTokenEx(
                token,
                TOKEN_ALL_ACCESS,
                std::ptr::null(),
                SecurityImpersonation,
                TokenPrimary,
                &mut primary,
            )
        };
        // SAFETY: `token` came from a successful OpenProcessToken and is no
        // longer needed once duplicated.
        unsafe { CloseHandle(token) };
        if dup_ok == 0 {
            return Err("duplicating the service token failed".into());
        }

        // Retarget the duplicate at the console session. This is the step that
        // needs SeTcbPrivilege, and the step that moves the child out of the
        // desktop-less session 0.
        // SAFETY: `primary` is a live primary token; TokenSessionId takes a DWORD.
        let set_ok = unsafe {
            SetTokenInformation(
                primary,
                TokenSessionId,
                &session as *const u32 as *const c_void,
                std::mem::size_of::<u32>() as u32,
            )
        };
        if set_ok == 0 {
            // SAFETY: `primary` is ours and unused from here.
            unsafe { CloseHandle(primary) };
            return Err(format!(
                "retargeting the token at session {session} failed (SeTcbPrivilege is required — \
                 this must run as LocalSystem)"
            ));
        }

        // `CreateProcessAsUserW` writes into the command line buffer, so it must
        // be owned and mutable.
        let mut cmdline: Vec<u16> = super::quote_command(exe, args)
            .encode_utf16()
            .chain([0])
            .collect();
        // `WinSta0\Default` is the interactive window station's ordinary desktop.
        // The agent re-attaches to whatever desktop is taking input once it's
        // running (see `DesktopFollower`); this is only where it starts.
        let mut desktop: Vec<u16> = "WinSta0\\Default".encode_utf16().chain([0]).collect();

        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.lpDesktop = desktop.as_mut_ptr();
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        // SAFETY: `primary` is a session-retargeted primary token; `cmdline` is
        // a NUL-terminated mutable UTF-16 buffer that outlives the call.
        let ok = unsafe {
            CreateProcessAsUserW(
                primary,
                std::ptr::null(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0, // don't inherit handles
                CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
                std::ptr::null(),
                std::ptr::null(),
                &si,
                &mut pi,
            )
        };
        // SAFETY: `primary` is ours; the child holds its own reference now.
        unsafe { CloseHandle(primary) };
        if ok == 0 {
            return Err("CreateProcessAsUser into the console session failed".into());
        }
        // The thread handle is surplus — only the process handle is waited on.
        // SAFETY: both handles came from a successful CreateProcessAsUser.
        unsafe { CloseHandle(pi.hThread) };
        Ok(pi.hProcess)
    }

    /// A child launched by [`launch_in_console_session`], closed on drop.
    pub struct ConsoleAgent {
        handle: *mut c_void,
        /// The console session it was launched into. When the console moves to a
        /// different session (logoff, fast user switching, an RDP takeover) this
        /// child is stranded on a desktop nobody is at and has to be replaced.
        session: u32,
    }

    // SAFETY: a process handle is valid from any thread of the process.
    unsafe impl Send for ConsoleAgent {}

    impl ConsoleAgent {
        /// Launch and wrap. See [`launch_in_console_session`].
        pub fn launch(exe: &Path, args: &[&str]) -> Result<ConsoleAgent, String> {
            let session = active_console_session();
            let handle = launch_in_console_session(exe, args)?;
            Ok(ConsoleAgent { handle, session })
        }

        /// Whether the child is still running.
        pub fn alive(&self) -> bool {
            // SAFETY: `self.handle` is a live process handle this value owns.
            // A zero timeout makes this a poll, not a wait.
            let signalled = unsafe { WaitForSingleObject(self.handle, 0) };
            signalled != WAIT_OBJECT_0
        }

        /// Whether the interactive console has moved to a different session than
        /// the one this agent was launched into — i.e. it is now on a desktop
        /// nobody is sitting at, and the supervisor should replace it.
        pub fn session_moved(&self) -> bool {
            let now = active_console_session();
            now != NO_SESSION && now != self.session
        }

        /// Stop the child. Terminating is correct here rather than harsh: the
        /// agent holds no user data and no in-flight writes worth draining, and
        /// a stranded one must not outlive the session it was launched for.
        pub fn stop(&self) {
            // SAFETY: a live process handle this value owns.
            unsafe { TerminateProcess(self.handle, 0) };
        }
    }

    impl Drop for ConsoleAgent {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                // SAFETY: this value owns `handle` and is being destroyed.
                unsafe { CloseHandle(self.handle) };
            }
        }
    }

    /// A handle to a desktop, closed on drop.
    ///
    /// Ownership matters more than usual here: the handle currently assigned to
    /// a thread must outlive the assignment, so [`DesktopFollower`] holds the
    /// live one and drops the previous only *after* a successful switch.
    struct Desktop(*mut c_void);

    // SAFETY: HDESK is a kernel handle, valid in any thread of the process.
    unsafe impl Send for Desktop {}

    impl Drop for Desktop {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` is a handle from OpenInputDesktop that this
                // value owns and is no longer assigned to any thread.
                unsafe { CloseDesktop(self.0) };
            }
        }
    }

    /// Open the desktop currently receiving input, and read its name.
    fn open_input_desktop() -> Option<(Desktop, String)> {
        // SAFETY: documented call; a null return signals failure.
        let h = unsafe { OpenInputDesktop(0, FALSE, DESKTOP_ALL) };
        if h.is_null() {
            return None;
        }
        let desktop = Desktop(h);
        let mut buf = [0u16; 256];
        let mut needed = 0u32;
        // SAFETY: `buf` is 256 writable u16s and its byte length is passed exactly.
        let ok = unsafe {
            GetUserObjectInformationW(
                h,
                UOI_NAME as i32,
                buf.as_mut_ptr() as *mut c_void,
                std::mem::size_of_val(&buf) as u32,
                &mut needed,
            )
        };
        let name = if ok != 0 {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..end])
        } else {
            String::new()
        };
        Some((desktop, name))
    }

    /// Keeps one thread attached to whichever desktop is currently receiving
    /// input, so capture and injection follow the user across a UAC prompt
    /// instead of going blind and deaf the moment the secure desktop appears.
    ///
    /// One per thread: `SetThreadDesktop` is a *per-thread* association, and it
    /// fails outright on a thread that owns any window or hook — which is why
    /// the injector and capture threads (neither creates a window) are the two
    /// places this is used.
    pub struct DesktopFollower {
        current: Option<Desktop>,
        name: String,
    }

    impl Default for DesktopFollower {
        fn default() -> Self {
            Self::new()
        }
    }

    impl DesktopFollower {
        pub fn new() -> DesktopFollower {
            DesktopFollower {
                current: None,
                name: String::new(),
            }
        }

        /// The name of the desktop this thread is attached to (`"Default"`,
        /// `"Winlogon"`, a screensaver desktop), empty before the first attach.
        pub fn desktop_name(&self) -> &str {
            &self.name
        }

        /// Whether the thread is attached to the secure desktop right now.
        pub fn on_secure_desktop(&self) -> bool {
            self.name.eq_ignore_ascii_case("Winlogon")
        }

        /// Attach this thread to the input desktop if it has changed since the
        /// last call. Returns `true` when a switch actually happened, so a
        /// caller holding desktop-derived state (a DXGI duplication) knows to
        /// rebuild it.
        ///
        /// Not free: even the unchanged case opens the input desktop, reads its
        /// name, and drops the handle — three Win32 calls. Callers on a hot path
        /// (the injector, once per input event) rate-limit it; callers that need
        /// a truthful answer at a specific moment (capture, on `ACCESS_LOST`)
        /// ask directly.
        pub fn follow(&mut self) -> bool {
            let Some((desktop, name)) = open_input_desktop() else {
                // Nothing to attach to — a locked session with no input desktop
                // this process may open. Keep the current attachment; the next
                // call retries.
                return false;
            };
            if self.current.is_some() && name == self.name {
                return false; // unchanged; `desktop` closes on drop
            }
            // SAFETY: `desktop.0` is a live handle; this thread owns no windows
            // or hooks (the injector and capture threads create none).
            if unsafe { SetThreadDesktop(desktop.0) } == 0 {
                // Refused — almost always "this process isn't privileged enough
                // for that desktop", i.e. the secure desktop without SYSTEM.
                // Stay where we are rather than ending up attached to nothing.
                return false;
            }
            // Only now is it safe to release the previous desktop: the thread is
            // no longer standing on it.
            self.current = Some(desktop);
            self.name = name;
            true
        }
    }
}

// ---------------------------------------------------------------------------
// Non-Windows stub — same shape, so callers stay free of `cfg` noise
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
mod imp {
    use super::{Integrity, Posture};

    /// On Unix there is no UAC/UIPI split; the process is whatever the OS says
    /// and the support session already has the reach the user has.
    pub fn current_posture() -> Posture {
        Posture {
            integrity: Integrity::Medium,
            elevated: false,
            ui_access: false,
        }
    }

    pub fn active_console_session() -> u32 {
        0
    }

    pub fn interactive_user_logical_drive_mask() -> Result<u32, String> {
        Ok(0)
    }

    pub fn interactive_user_network_mapping(_mount: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
    pub fn connect_interactive_user_network_mapping(
        _mount: &str,
        _remote: &str,
    ) -> Result<(), String> {
        Err("interactive Windows drive mappings are unavailable".into())
    }

    pub fn disconnect_interactive_user_network_mapping_if_matches(
        _mount: &str,
        _expected_remote: &str,
    ) -> Result<bool, String> {
        Ok(false)
    }

    pub fn dos_device_targets(_mount: &str) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    pub fn interactive_user_dos_device_targets(_mount: &str) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    /// No session-0 split to escape: a Unix daemon that needs a display gets it
    /// from the display server's own rules, not from token surgery.
    pub struct ConsoleAgent;

    impl ConsoleAgent {
        pub fn launch(_exe: &std::path::Path, _args: &[&str]) -> Result<ConsoleAgent, String> {
            Err("launching a console-session agent is a Windows-only mechanism".into())
        }
        pub fn alive(&self) -> bool {
            false
        }
        pub fn session_moved(&self) -> bool {
            false
        }
        pub fn stop(&self) {}
    }

    /// A follower that never has anywhere to follow to.
    #[derive(Default)]
    pub struct DesktopFollower;

    impl DesktopFollower {
        pub fn new() -> DesktopFollower {
            DesktopFollower
        }
        pub fn desktop_name(&self) -> &str {
            ""
        }
        pub fn on_secure_desktop(&self) -> bool {
            false
        }
        pub fn follow(&mut self) -> bool {
            false
        }
    }
}

pub use imp::{
    active_console_session, connect_interactive_user_network_mapping, current_posture,
    disconnect_interactive_user_network_mapping_if_matches, dos_device_targets,
    interactive_user_dos_device_targets, interactive_user_logical_drive_mask,
    interactive_user_network_mapping, ConsoleAgent, DesktopFollower,
};

/// Whether this build targets Windows at all, hoisted here so callers don't
/// sprinkle `cfg!`.
pub const IS_WINDOWS: bool = cfg!(windows);

#[cfg(test)]
mod tests {
    use super::*;

    fn posture(integrity: Integrity, ui_access: bool) -> Posture {
        Posture {
            integrity,
            elevated: integrity >= Integrity::High,
            ui_access,
        }
    }

    #[test]
    fn integrity_rids_map_to_levels() {
        assert_eq!(Integrity::from_rid(0x0000), Integrity::Untrusted);
        assert_eq!(Integrity::from_rid(0x1000), Integrity::Low);
        assert_eq!(Integrity::from_rid(0x2000), Integrity::Medium);
        assert_eq!(Integrity::from_rid(0x3000), Integrity::High);
        assert_eq!(Integrity::from_rid(0x4000), Integrity::System);
        // Protected-process levels above System still classify as System.
        assert_eq!(Integrity::from_rid(0x5000), Integrity::System);
        // An in-between value rounds DOWN, never up.
        assert_eq!(Integrity::from_rid(0x2FFF), Integrity::Medium);
    }

    #[test]
    fn a_standard_user_process_cannot_reach_anything() {
        let p = Posture::standard_user();
        assert!(!p.can_drive_elevated_windows());
        assert!(!p.can_follow_secure_desktop());
    }

    #[test]
    fn high_integrity_drives_elevated_windows_but_not_the_secure_desktop() {
        // The distinction that makes this module worth having: "run as
        // administrator" fixes Event Viewer and does NOT fix the UAC prompt.
        let p = posture(Integrity::High, false);
        assert!(p.can_drive_elevated_windows());
        assert!(!p.can_follow_secure_desktop());
    }

    #[test]
    fn system_reaches_both() {
        let p = posture(Integrity::System, false);
        assert!(p.can_drive_elevated_windows());
        assert!(p.can_follow_secure_desktop());
    }

    #[test]
    fn ui_access_exempts_from_uipi_without_being_admin() {
        let p = posture(Integrity::Medium, true);
        assert!(p.can_drive_elevated_windows());
        // …but it is not SYSTEM, so the secure desktop stays out of reach.
        assert!(!p.can_follow_secure_desktop());
    }

    #[test]
    fn a_follower_starts_unattached() {
        let f = DesktopFollower::new();
        assert!(!f.on_secure_desktop());
    }

    #[test]
    fn a_program_files_path_stays_one_argument() {
        // The install path has a space in it. Unquoted, `CreateProcessAsUser`
        // would try to run `C:\Program` — and only ever on a real install, never
        // in a dev checkout, which is the worst possible place to find out.
        let cmd = quote_command(
            std::path::Path::new(r"C:\Program Files\CEC Support\cec-support.exe"),
            &["run", "--session-agent"],
        );
        assert_eq!(
            cmd,
            r#""C:\Program Files\CEC Support\cec-support.exe" "run" "--session-agent""#
        );
    }

    #[test]
    fn an_argument_containing_a_quote_is_escaped() {
        let cmd = quote_command(std::path::Path::new("agent.exe"), &[r#"a"b"#]);
        assert_eq!(cmd, r#""agent.exe" "a\"b""#);
    }

    #[test]
    fn no_arguments_is_just_the_quoted_exe() {
        let cmd = quote_command(std::path::Path::new("agent.exe"), &[]);
        assert_eq!(cmd, r#""agent.exe""#);
    }

    #[cfg(windows)]
    #[test]
    fn the_interactive_drive_namespace_can_be_inspected_without_mutation() {
        interactive_user_logical_drive_mask().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn the_interactive_network_namespace_can_be_queried_without_mutation() {
        interactive_user_network_mapping("X:").unwrap();
    }

    #[test]
    fn a_console_agent_cannot_be_launched_off_windows() {
        // The stub must fail loudly rather than pretend it launched something:
        // a service that believes it has a session agent when it hasn't would
        // sit there supervising nothing.
        let r = ConsoleAgent::launch(std::path::Path::new("agent"), &[]);
        assert!(r.is_err() || cfg!(windows));
    }
}
