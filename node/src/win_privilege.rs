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
//! ## Consent still governs, always
//!
//! Nothing here decides *whether* to escalate. That is two separate yeses held
//! elsewhere — the machine's one-time `ElevationPolicy` and the technician's
//! `Capability::Elevated` grant, both in `allmystuff_cec_consent` and both
//! re-read on every privileged frame. This module answers the other half:
//! given that it was allowed, can this process actually deliver it, and if not,
//! what should the technician be told. A posture is a capability of the
//! *process*, never an authorization — nothing here may be used to skip a gate.

use allmystuff_cec_protocol::ElevationBlocker;

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

/// Decide what to tell the technician about administrator reach on this
/// session.
///
/// Pure, so the wording a technician sees in the field is decided by something
/// that can be exhaustively tested rather than by whichever branch a Win32 call
/// happened to take. The order encodes the triage a technician would otherwise
/// have to do out loud: *is it even a Windows question* → *does this PC allow
/// admin at all* → *does this session's grant include it* → *can we deliver it*
/// → *is the fix "install the service" or "it's installed and still can't"*.
///
/// Each answer names something exactly one person can act on, which is the
/// whole job: a wrong-but-plausible message here sends a technician chasing a
/// setting on the wrong machine while the customer waits.
pub fn blocker_for(
    is_windows: bool,
    machine_allows: bool,
    granted: bool,
    posture: Posture,
    service_installed: bool,
) -> ElevationBlocker {
    if !is_windows {
        // No UAC/UIPI split to cross: control is already whatever the OS allows.
        return ElevationBlocker::NotApplicable;
    }
    if !machine_allows {
        // The machine-wide switch, checked first: when it's off, nothing about
        // this session or this technician's grant is the reason, and saying
        // otherwise would send someone to reconnect over and over.
        return ElevationBlocker::NotAllowedOnThisMachine;
    }
    if !granted {
        // Checked before posture on purpose. A session that wasn't granted admin
        // must report the grant gap even when the process happens to be running
        // as SYSTEM and could technically do anything — consent is the reason,
        // and naming the posture instead would invite a technician to go chase
        // a non-problem.
        return ElevationBlocker::NotGranted;
    }
    if posture.can_drive_elevated_windows() {
        return ElevationBlocker::None;
    }
    if !service_installed {
        ElevationBlocker::ServiceMissing
    } else {
        ElevationBlocker::AgentNotPrivileged
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

    use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::Security::{
        DuplicateTokenEx, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
        SecurityImpersonation, SetTokenInformation, TokenElevation, TokenIntegrityLevel,
        TokenPrimary, TokenSessionId, TokenUIAccess, TOKEN_ALL_ACCESS, TOKEN_ASSIGN_PRIMARY,
        TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
    use windows_sys::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, TerminateProcess,
        WaitForSingleObject, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
        STARTUPINFOW,
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
        /// Cheap enough for a per-frame call: the common case opens the input
        /// desktop, sees the same name, and drops the handle again.
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

pub use imp::{active_console_session, current_posture, ConsoleAgent, DesktopFollower};

/// Whether this build targets Windows at all — the first question
/// [`blocker_for`] asks, hoisted here so callers don't sprinkle `cfg!`.
pub const IS_WINDOWS: bool = cfg!(windows);

/// Whether screen capture may follow the desktop switch onto the **secure
/// desktop**. Off until the node turns it on from the customer's machine-wide
/// admin-access setting.
///
/// This is a process-wide switch rather than a parameter because the capture
/// pump is spawned deep in the media stack with no view of consent, and the
/// question it answers is a property of the machine, not of one stream.
///
/// It is separate from the input path — and defaults to *off* — because the
/// secure desktop is not just another window. It is where Windows puts the
/// "enter an administrator password" prompt, so streaming it turns "let them
/// see my screen" into "let them watch me type a password". Following it is
/// therefore tied to the customer having deliberately enabled administrator
/// access for support, and a machine that never enabled it keeps the old
/// behaviour exactly: the stream holds the last frame until the prompt closes.
static SECURE_DESKTOP_FOLLOW: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Turn secure-desktop capture on or off. Called by the node whenever the
/// customer's admin-access setting is loaded or changed, so switching it off in
/// Settings stops the UAC prompt being streamed from the next re-acquire.
pub fn set_secure_desktop_follow(allowed: bool) {
    SECURE_DESKTOP_FOLLOW.store(allowed, std::sync::atomic::Ordering::Relaxed);
}

/// Whether capture may follow onto the secure desktop right now.
pub fn secure_desktop_follow_allowed() -> bool {
    SECURE_DESKTOP_FOLLOW.load(std::sync::atomic::Ordering::Relaxed)
}

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
    fn the_machine_switch_is_reported_before_anything_else() {
        // A PC with admin access switched off says so, even when the session
        // was granted it and the process could technically deliver it. Any
        // other message here has the technician reconnecting in a loop against
        // a setting that reconnecting cannot touch.
        let b = blocker_for(true, false, true, posture(Integrity::System, false), true);
        assert_eq!(b, ElevationBlocker::NotAllowedOnThisMachine);
    }

    #[test]
    fn consent_is_reported_before_posture() {
        // Even running as SYSTEM on a PC that allows admin, a session that
        // wasn't granted it reports the grant gap — not a posture problem that
        // doesn't exist.
        let b = blocker_for(true, true, false, posture(Integrity::System, false), true);
        assert_eq!(b, ElevationBlocker::NotGranted);
    }

    #[test]
    fn granted_and_privileged_is_unblocked() {
        let b = blocker_for(true, true, true, posture(Integrity::High, false), true);
        assert_eq!(b, ElevationBlocker::None);
    }

    #[test]
    fn granted_but_stuck_names_the_fix() {
        // No service installed: the customer can fix this, and the message says so.
        assert_eq!(
            blocker_for(true, true, true, Posture::standard_user(), false),
            ElevationBlocker::ServiceMissing
        );
        // Service installed and still medium integrity: a different problem,
        // and it must not tell the customer to install what they already have.
        assert_eq!(
            blocker_for(true, true, true, Posture::standard_user(), true),
            ElevationBlocker::AgentNotPrivileged
        );
    }

    #[test]
    fn non_windows_is_never_blocked_on_elevation() {
        assert_eq!(
            blocker_for(false, true, true, Posture::standard_user(), false),
            ElevationBlocker::NotApplicable
        );
        // …including when nothing was allowed or granted: there is no rung here.
        assert_eq!(
            blocker_for(false, false, false, Posture::standard_user(), false),
            ElevationBlocker::NotApplicable
        );
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

    #[test]
    fn a_console_agent_cannot_be_launched_off_windows() {
        // The stub must fail loudly rather than pretend it launched something:
        // a service that believes it has a session agent when it hasn't would
        // sit there supervising nothing.
        let r = ConsoleAgent::launch(std::path::Path::new("agent"), &[]);
        assert!(r.is_err() || cfg!(windows));
    }
}
