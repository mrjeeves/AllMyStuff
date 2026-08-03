//! The input media plane's *sink* side: apply a remote console's keyboard
//! and mouse events to this machine, via `enigo` (SendInput on Windows,
//! CoreGraphics on macOS, X11 on Linux).
//!
//! Mouse coordinates arrive normalized 0..1 over the sender's *view of one
//! of our screens* (the paired display stream) plus which screen that is —
//! the `screen` field of [`InputAction::MouseMove`], absent for the
//! primary. Injection resolves that monitor's rectangle in the global
//! desktop space and lands the cursor inside it, so control follows the
//! screen the console is showing. The two ends still never negotiate
//! resolutions — exactly the piKVM model, one monitor at a time.
//!
//! On Windows the absolute move is raised by hand (`SendInput` with
//! `MOUSEEVENTF_VIRTUALDESK`): enigo's absolute coordinates normalize
//! against the primary monitor only, which physically can't express a
//! point on a second screen. macOS and X11 take global coordinates
//! through enigo directly.
//!
//! Keys arrive as the DOM `KeyboardEvent.key` value (the layout-resolved
//! character or a name like "Enter"), plus — from senders that know it —
//! the physical `KeyboardEvent.code` ("KeyC"). Injection assumes key
//! *combinations* are the norm, not the exception:
//!
//! - Plain typing with no modifier held injects the resolved character
//!   as itself, so the *typist's* layout wins.
//! - With Shift held the resolved character is the *shifted* one ("A",
//!   "!", "?"), which must not be injected as itself: that would shift a
//!   key whose unshifted face is already the symbol, and the held Shift
//!   either doubles up or (on X11's Unicode keycode remap) lands on an
//!   empty shift level and types nothing. Instead the unshifted keycap of
//!   the physical key is injected and the forwarded Shift composes it —
//!   the base letter for "A", the digit "1" for "!", just like hardware.
//! - A chord (Ctrl/Alt/Meta held) resolves through the *physical* key:
//!   Ctrl+C must land on the remote's C key, never on whatever character
//!   the sender's layout composed under the held modifiers.
//! - Each route's pressed keys are remembered, so a keyup releases
//!   exactly the key its keydown pressed (Shift+1 goes down as "!" but
//!   comes up as "1" — without the memory the remote keeps a stuck key),
//!   and a route tearing down mid-chord lifts everything it still held.
//!
//! `Enigo` isn't `Send`, so one dedicated thread owns it and drains a
//! channel of commands; the mesh just calls [`Injector::apply`] after its
//! route/ownership gates pass. The thread starts lazily on the first
//! event and dies with the app.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(not(windows))]
use enigo::Coordinate;
use enigo::{Axis, Button, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use parking_lot::Mutex;

use allmystuff_session::InputAction;

enum Cmd {
    Event {
        route: String,
        action: InputAction,
        /// Whether this event is authorized to reach **elevated** windows.
        /// Decided per event by the mesh's `sender_may_elevate` (machine
        /// setting AND the sender's entitlement), never cached here — so
        /// switching admin access off stops the very next keystroke.
        elevated: bool,
    },
    ReleaseRoute(String),
}

/// AMS-06: the most input events queued for injection before excess is dropped.
/// Legitimate remote control is consumed far faster than it's produced, so the
/// queue normally holds a handful; this only ever bites a flood from an
/// abusive controller, capping memory instead of growing it unbounded.
const INPUT_QUEUE_CAP: usize = 4096;

#[derive(Default)]
pub struct Injector {
    tx: Mutex<Option<mpsc::SyncSender<Cmd>>>,
}

impl Injector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one event for injection. Starts the injector thread on first
    /// use; if the platform refuses (no display server, missing
    /// permissions) the failure is logged once and events are dropped.
    ///
    /// `elevated` says this event is authorized to reach elevated windows. On
    /// Windows that decides whether the injector follows the *input desktop*
    /// before typing — the difference between a click landing in Event Viewer
    /// and being silently discarded by UIPI. Elsewhere it is inert.
    pub fn apply(&self, route: &str, action: InputAction, elevated: bool) {
        self.send(
            Cmd::Event {
                route: route.into(),
                action,
                elevated,
            },
            true,
        );
    }

    /// A control route ended (teardown, peer drop): lift any keys its
    /// stream still holds down, so a console that vanished mid-chord
    /// doesn't leave this machine with a stuck Ctrl. No-op for routes
    /// that never pressed anything.
    pub fn release_route(&self, route: &str) {
        // Never spawns the thread: if it isn't running, nothing is held.
        self.send(Cmd::ReleaseRoute(route.into()), false);
    }

    fn send(&self, cmd: Cmd, spawn: bool) {
        let mut tx = self.tx.lock();
        if tx.is_none() {
            if !spawn {
                return;
            }
            // A *bounded* queue: a flood from an abusive controller drops excess
            // (below) rather than growing memory without limit (AMS-06).
            let (sender, rx) = mpsc::sync_channel::<Cmd>(INPUT_QUEUE_CAP);
            std::thread::spawn(move || run_injector(rx));
            *tx = Some(sender);
        }
        if let Some(t) = tx.as_ref() {
            match t.try_send(cmd) {
                Ok(()) => {}
                // Queue full — the injector can't keep up with the inbound rate.
                // Drop the event; legitimate control never fills a queue this
                // deep, and a teardown's `release_route` lifts any held keys.
                Err(mpsc::TrySendError::Full(_)) => {}
                // The thread ended (platform refused); allow a retry on the next
                // event rather than wedging forever.
                Err(mpsc::TrySendError::Disconnected(_)) => *tx = None,
            }
        }
    }
}

fn run_injector(rx: mpsc::Receiver<Cmd>) {
    // Inbound control is what the user *feels* — keep the injector
    // responsive under exactly the load that made them reach for it.
    crate::os_perf::boost_media_thread();
    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("input injection unavailable on this machine: {e}");
            return;
        }
    };
    let mut screens = ScreenMap::load();
    // Each control route's keyboard state — held modifiers steer how the
    // next key resolves, and whatever is still down gets lifted when the
    // route goes away.
    let mut keys: HashMap<String, KeyTracker> = HashMap::new();
    // Windows only: keeps this thread attached to whichever desktop is taking
    // input. Without it every synthesized event goes to the desktop the thread
    // started on, so the moment Windows switches to the secure desktop (a UAC
    // prompt) the technician is typing into a desktop nobody is looking at.
    // Inert off Windows.
    let mut desktop = crate::win_privilege::DesktopFollower::new();
    while let Ok(cmd) = rx.recv() {
        let (route, action) = match cmd {
            Cmd::Event {
                route,
                action,
                elevated,
            } => {
                // Follow the input desktop only for events authorized to reach
                // elevated windows. An unelevated session deliberately stays on
                // the desktop it started on: it must not be able to type at a
                // UAC prompt, which is the one dialog whose whole purpose is to
                // be answered by the person sitting at the machine.
                if elevated && desktop.follow() {
                    tracing::info!(
                        "input: following desktop switch to {:?}{}",
                        desktop.desktop_name(),
                        if desktop.on_secure_desktop() {
                            " (secure desktop — UAC prompt)"
                        } else {
                            ""
                        }
                    );
                }
                (route, action)
            }
            Cmd::ReleaseRoute(route) => {
                if let Some(mut tracker) = keys.remove(&route) {
                    for k in tracker.release_all() {
                        if let Err(e) = enigo.key(k, Direction::Release) {
                            tracing::debug!("releasing {k:?} after route end failed: {e}");
                        }
                    }
                }
                continue;
            }
        };
        // A viewer clicking or typing at a *dark* console is the "remote
        // login wakes the machine" moment — relight the panel so the
        // stream they're driving blind comes back. No-op while frames
        // flow (rate-limited and gated inside).
        if matches!(
            action,
            InputAction::MouseButton { .. } | InputAction::Key { .. }
        ) {
            crate::wake::force_display_on_if_dark();
        }
        let result = match action {
            InputAction::MouseMove { x, y, screen } => {
                let rect = screens.resolve(screen);
                let (gx, gy) = rect.denorm(x, y);
                move_mouse_global(&mut enigo, gx, gy)
            }
            // Pointer-lock deltas: raw relative motion, straight through —
            // games read this as camera movement, so no screen resolve, no
            // clamping, no coalescing beyond what the sender already did.
            InputAction::MouseMoveRel { dx, dy } => enigo
                .move_mouse(dx.round() as i32, dy.round() as i32, enigo::Coordinate::Rel)
                .map_err(|e| e.to_string()),
            InputAction::MouseButton { button, down } => match dom_button(button) {
                Some(b) => enigo.button(b, direction(down)).map_err(|e| e.to_string()),
                None => Ok(()),
            },
            InputAction::Wheel { dx, dy } => {
                let mut r = Ok(());
                if dy.abs() >= 0.5 {
                    r = enigo
                        .scroll(dy.round() as i32, Axis::Vertical)
                        .map_err(|e| e.to_string());
                }
                if r.is_ok() && dx.abs() >= 0.5 {
                    r = enigo
                        .scroll(dx.round() as i32, Axis::Horizontal)
                        .map_err(|e| e.to_string());
                }
                r
            }
            InputAction::Key {
                ref key,
                ref code,
                down,
            } => {
                let tracker = keys.entry(route).or_default();
                let k = if down {
                    tracker.press(key, code.as_deref())
                } else {
                    tracker.release(key, code.as_deref())
                };
                match k {
                    Some(k) => enigo.key(k, direction(down)).map_err(|e| e.to_string()),
                    None => Ok(()),
                }
            }
            // An input kind a newer viewer introduced that this build can't
            // inject — nothing to do (it decoded as `Unknown` rather than
            // failing the whole frame).
            InputAction::Unknown => Ok(()),
        };
        if let Err(e) = result {
            tracing::debug!("input injection event failed: {e}");
        }
    }
}

/// Land the cursor on a global desktop coordinate (any monitor).
#[cfg(not(windows))]
fn move_mouse_global(enigo: &mut Enigo, gx: i32, gy: i32) -> Result<(), String> {
    // CoreGraphics and X11 take global coordinates as-is.
    enigo
        .move_mouse(gx, gy, Coordinate::Abs)
        .map_err(|e| e.to_string())
}

/// Land the cursor on a global desktop coordinate (any monitor).
///
/// Raised by hand because enigo's `Coordinate::Abs` normalizes against the
/// primary monitor without `MOUSEEVENTF_VIRTUALDESK` — a point on a second
/// screen is simply unreachable through it. With the flag, 0..65535 spans
/// the whole virtual desktop.
#[cfg(windows)]
fn move_mouse_global(_enigo: &mut Enigo, gx: i32, gy: i32) -> Result<(), String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = i64::from(GetSystemMetrics(SM_CXVIRTUALSCREEN)).max(2);
        let vh = i64::from(GetSystemMetrics(SM_CYVIRTUALSCREEN)).max(2);
        // Pixel → the API's 0..65535 span of the virtual desktop, rounding
        // at the pixel center.
        let nx =
            ((i64::from(gx) - i64::from(vx)).clamp(0, vw - 1) * 65535 + (vw - 1) / 2) / (vw - 1);
        let ny =
            ((i64::from(gy) - i64::from(vy)).clamp(0, vh - 1) * 65535 + (vh - 1) / 2) / (vh - 1);
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: nx as i32,
                    dy: ny as i32,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        if SendInput(&[input], std::mem::size_of::<INPUT>() as i32) == 1 {
            Ok(())
        } else {
            Err("SendInput refused the move".into())
        }
    }
}

fn direction(down: bool) -> Direction {
    if down {
        Direction::Press
    } else {
        Direction::Release
    }
}

/// One monitor's rectangle in the global desktop space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScreenRect {
    id: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    primary: bool,
}

impl ScreenRect {
    /// Normalized 0..1 over this screen → a global pixel inside it.
    fn denorm(&self, nx: f64, ny: f64) -> (i32, i32) {
        let px = (nx.clamp(0.0, 1.0) * (self.w.max(1) - 1) as f64).round() as i32;
        let py = (ny.clamp(0.0, 1.0) * (self.h.max(1) - 1) as f64).round() as i32;
        (self.x + px, self.y + py)
    }
}

/// The monitor layout, refreshed when an event names a screen we don't
/// know (hotplug since the last look).
/// How stale the cached monitor layout may grow before `resolve` re-reads
/// it. A resolution/DPI/layout change mid-session must reach the mapping
/// within a beat — the old code refreshed only when a *named* screen id
/// went missing, so the primary-screen path (the common case) denormalized
/// against stale dimensions forever after a mode change, landing the
/// cursor in the wrong place. Two seconds keeps the enumeration cost
/// invisible at 60 events/s.
const SCREENS_TTL: Duration = Duration::from_secs(2);

struct ScreenMap {
    rects: Vec<ScreenRect>,
    refreshed: Instant,
}

impl ScreenMap {
    fn load() -> Self {
        ScreenMap {
            rects: query_screens(),
            refreshed: Instant::now(),
        }
    }

    /// The rectangle for `screen` — the primary when unnamed, a re-query
    /// then the primary when the named one is gone. The cache re-reads on
    /// [`SCREENS_TTL`] regardless, so a mode change reaches the mapping.
    /// The fallback rect (no monitors readable at all) keeps the old
    /// primary-screen behaviour.
    fn resolve(&mut self, screen: Option<u32>) -> ScreenRect {
        if self.refreshed.elapsed() >= SCREENS_TTL {
            self.rects = query_screens();
            self.refreshed = Instant::now();
        }
        if let Some(id) = screen {
            if let Some(r) = self.rects.iter().find(|r| r.id == id) {
                return *r;
            }
            self.rects = query_screens();
            self.refreshed = Instant::now();
            if let Some(r) = self.rects.iter().find(|r| r.id == id) {
                return *r;
            }
            tracing::debug!("input names screen {id} but it isn't attached — using the primary");
        }
        self.primary()
    }

    fn primary(&self) -> ScreenRect {
        self.rects
            .iter()
            .find(|r| r.primary)
            .or_else(|| self.rects.first())
            .copied()
            .unwrap_or(ScreenRect {
                id: 0,
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
                primary: true,
            })
    }
}

/// Every attached monitor's global rect, via the same enumeration the
/// capture side uses — so the ids in `screen:<id>` capabilities resolve
/// back to the same physical screens here.
fn query_screens() -> Vec<ScreenRect> {
    let Ok(monitors) = xcap::Monitor::all() else {
        return Vec::new();
    };
    monitors
        .iter()
        .filter_map(|m| {
            Some(ScreenRect {
                id: m.id().ok()?,
                x: m.x().unwrap_or(0),
                y: m.y().unwrap_or(0),
                w: m.width().unwrap_or(0).max(1),
                h: m.height().unwrap_or(0).max(1),
                primary: m.is_primary().unwrap_or(false),
            })
        })
        .collect()
}

/// DOM `MouseEvent.button` → enigo button. 3/4 (browser back/forward)
/// don't inject — they'd navigate the *remote*'s browser focus oddly.
fn dom_button(button: u8) -> Option<Button> {
    match button {
        0 => Some(Button::Left),
        1 => Some(Button::Middle),
        2 => Some(Button::Right),
        _ => None,
    }
}

/// One control route's keyboard state: which keys its stream holds down
/// (each remembered as the enigo key actually injected), in press order.
///
/// The memory carries combinations across the places a stateless mapping
/// breaks: held Ctrl/Alt/Meta switch resolution to the physical key, a
/// keyup releases exactly what its keydown pressed even when the
/// modifiers (and so the DOM `key` value) changed in between, and a dying
/// route can lift everything it still holds.
#[derive(Default)]
struct KeyTracker {
    /// (identity, injected key) per held key — identity is the physical
    /// `code` when the sender knew it, else the `key` value itself.
    pressed: Vec<(String, Key)>,
}

impl KeyTracker {
    fn held(&self, k: Key) -> bool {
        self.pressed.iter().any(|(_, p)| *p == k)
    }

    fn identity(key: &str, code: Option<&str>) -> String {
        match code {
            Some(c) if !c.is_empty() => c.into(),
            _ => key.into(),
        }
    }

    /// A keydown: resolve what to inject and remember it as held.
    fn press(&mut self, key: &str, code: Option<&str>) -> Option<Key> {
        let k = self.resolve(key, code)?;
        let id = Self::identity(key, code);
        // Auto-repeat arrives as a burst of re-presses (the remote OS won't
        // repeat an injected key on its own); collapse them to a single held
        // entry so the eventual keyup still lifts exactly once.
        self.pressed.retain(|(i, _)| *i != id);
        self.pressed.push((id, k));
        Some(k)
    }

    /// A keyup: release exactly the key the matching keydown pressed. For
    /// a down we never saw (an older sender, a mid-stream subscribe),
    /// fall back to resolving fresh.
    fn release(&mut self, key: &str, code: Option<&str>) -> Option<Key> {
        let id = Self::identity(key, code);
        if let Some(i) = self.pressed.iter().position(|(i, _)| *i == id) {
            return Some(self.pressed.remove(i).1);
        }
        self.resolve(key, code)
    }

    /// Everything still held, in reverse press order (chords unwind the
    /// way they wound up: the letter lifts before its modifier).
    fn release_all(&mut self) -> Vec<Key> {
        self.pressed.drain(..).rev().map(|(_, k)| k).collect()
    }

    /// DOM `KeyboardEvent.key` (+ physical `code`) → enigo key, steered by
    /// the modifiers this route currently holds. `None` = a key we
    /// deliberately don't carry (media keys, IME composition, lock keys we
    /// can't faithfully mirror).
    fn resolve(&self, key: &str, code: Option<&str>) -> Option<Key> {
        let mut chars = key.chars();
        let c = match (chars.next(), chars.next()) {
            (Some(c), None) => c,
            _ => return map_named(key),
        };
        let combo = self.held(Key::Control) || self.held(Key::Alt) || self.held(Key::Meta);
        if combo {
            // A chord wants the *physical* key: Ctrl+C is the C key, not
            // whatever character the sender's layout composed under the
            // held modifiers (macOS Option turns letters into "ç", "å"…).
            if let Some(base) = code.and_then(base_char) {
                return Some(Key::Unicode(base));
            }
        }
        if combo || self.held(Key::Shift) {
            // A modifier is already held on this end, so the character the
            // sender's layout composed under it ("A", but also "!", "?",
            // "@"…) must not be injected as itself: that asks the platform
            // to *shift a key whose unshifted face is already the symbol*.
            // On X11 the Unicode path remaps a spare keycode's unshifted
            // level to that symbol, so with Shift held the server reads the
            // shifted level — NoSymbol — and types nothing. That's why
            // capitals worked (handled below) while "!" and "?" vanished.
            // Inject the key's unshifted keycap and let the held Shift
            // compose it, exactly as a real keyboard does.
            if let Some(base) = code.and_then(base_char) {
                return Some(Key::Unicode(base));
            }
            // No physical code (an older sender): a letter can still be
            // de-shifted by lower-casing it; a symbol has to stand as
            // itself and hope the modifier doesn't swallow it.
            if c.is_uppercase() {
                let mut lower = c.to_lowercase();
                if let (Some(l), None) = (lower.next(), lower.next()) {
                    return Some(Key::Unicode(l));
                }
            }
        }
        Some(Key::Unicode(c))
    }
}

/// The physical key a DOM `KeyboardEvent.code` names, as the character on
/// its keycap (US-reference, the convention `code` itself is defined by) —
/// what a chord resolves through. `None` for keys without a stable cap
/// (named keys travel by `key` instead).
fn base_char(code: &str) -> Option<char> {
    if let Some(letter) = code.strip_prefix("Key") {
        let mut chars = letter.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if c.is_ascii_uppercase() {
                return Some(c.to_ascii_lowercase());
            }
        }
        return None;
    }
    if let Some(digit) = code.strip_prefix("Digit") {
        let mut chars = digit.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if c.is_ascii_digit() {
                return Some(c);
            }
        }
        return None;
    }
    if let Some(pad) = code.strip_prefix("Numpad") {
        return match pad {
            "Add" => Some('+'),
            "Subtract" => Some('-'),
            "Multiply" => Some('*'),
            "Divide" => Some('/'),
            "Decimal" => Some('.'),
            _ => {
                let mut chars = pad.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if c.is_ascii_digit() => Some(c),
                    _ => None,
                }
            }
        };
    }
    match code {
        "Space" => Some(' '),
        "Minus" => Some('-'),
        "Equal" => Some('='),
        "BracketLeft" => Some('['),
        "BracketRight" => Some(']'),
        "Backslash" => Some('\\'),
        "Semicolon" => Some(';'),
        "Quote" => Some('\''),
        "Backquote" => Some('`'),
        "Comma" => Some(','),
        "Period" => Some('.'),
        "Slash" => Some('/'),
        _ => None,
    }
}

/// Named DOM `KeyboardEvent.key` values → enigo keys.
fn map_named(key: &str) -> Option<Key> {
    Some(match key {
        "Enter" => Key::Return,
        "Escape" => Key::Escape,
        "Backspace" => Key::Backspace,
        "Tab" => Key::Tab,
        "Delete" => Key::Delete,
        // Insert is half a chord on its own (Shift+Insert pastes,
        // Ctrl+Insert copies) — macOS has no such key to press.
        #[cfg(not(target_os = "macos"))]
        "Insert" => Key::Insert,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "ArrowUp" => Key::UpArrow,
        "ArrowDown" => Key::DownArrow,
        "ArrowLeft" => Key::LeftArrow,
        "ArrowRight" => Key::RightArrow,
        "Shift" => Key::Shift,
        "Control" => Key::Control,
        "Alt" => Key::Alt,
        "Meta" => Key::Meta,
        "CapsLock" => Key::CapsLock,
        "F1" => Key::F1,
        "F2" => Key::F2,
        "F3" => Key::F3,
        "F4" => Key::F4,
        "F5" => Key::F5,
        "F6" => Key::F6,
        "F7" => Key::F7,
        "F8" => Key::F8,
        "F9" => Key::F9,
        "F10" => Key::F10,
        "F11" => Key::F11,
        "F12" => Key::F12,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_keys_inject_as_unicode() {
        let t = KeyTracker::default();
        assert_eq!(t.resolve("a", Some("KeyA")), Some(Key::Unicode('a')));
        assert_eq!(t.resolve("?", Some("Slash")), Some(Key::Unicode('?')));
        // Layout-resolved characters carry through untranslated.
        assert_eq!(t.resolve("ü", None), Some(Key::Unicode('ü')));
    }

    #[test]
    fn named_keys_map_and_unknown_ones_drop() {
        let t = KeyTracker::default();
        assert_eq!(t.resolve("Enter", None), Some(Key::Return));
        assert_eq!(t.resolve("ArrowLeft", None), Some(Key::LeftArrow));
        assert_eq!(t.resolve("MediaPlayPause", None), None);
    }

    #[test]
    fn chords_resolve_through_the_physical_key() {
        let mut t = KeyTracker::default();
        assert_eq!(t.press("Control", Some("ControlLeft")), Some(Key::Control));
        // Ctrl+C lands on the C key even when the sender's layout
        // composed something else under the held modifiers…
        assert_eq!(t.press("ç", Some("KeyC")), Some(Key::Unicode('c')));
        // …and the shifted variant of a chord stays on the same key.
        assert_eq!(t.press("C", Some("KeyC")), Some(Key::Unicode('c')));
        assert_eq!(t.press("!", Some("Digit1")), Some(Key::Unicode('1')));
        // Named keys chord by name, as always.
        assert_eq!(t.resolve("Tab", Some("Tab")), Some(Key::Tab));
        // An older sender carries no code — the character itself stands,
        // lower-cased so the held modifier isn't asked for twice.
        assert_eq!(t.resolve("T", None), Some(Key::Unicode('t')));
        assert_eq!(t.resolve("t", None), Some(Key::Unicode('t')));
    }

    #[test]
    fn shifted_typing_injects_the_unshifted_keycap() {
        let mut t = KeyTracker::default();
        assert_eq!(t.press("Shift", Some("ShiftLeft")), Some(Key::Shift));
        // The held Shift restores the case on this end.
        assert_eq!(t.resolve("A", Some("KeyA")), Some(Key::Unicode('a')));
        // A shifted symbol injects its key's unshifted keycap too, so the
        // held Shift composes the symbol — injecting "!" itself while Shift
        // is down lands on an empty shift level on X11 and types nothing.
        assert_eq!(t.resolve("!", Some("Digit1")), Some(Key::Unicode('1')));
        assert_eq!(t.resolve("?", Some("Slash")), Some(Key::Unicode('/')));
        // No physical code (older sender): a symbol can only stand as
        // itself, but a letter is still de-shifted by lower-casing.
        assert_eq!(t.resolve("@", None), Some(Key::Unicode('@')));
        assert_eq!(t.resolve("Z", None), Some(Key::Unicode('z')));
    }

    #[test]
    fn keyup_releases_what_its_keydown_pressed() {
        let mut t = KeyTracker::default();
        t.press("Shift", Some("ShiftLeft"));
        // Shift+1 goes down as the unshifted keycap '1' (the held Shift
        // composes the '!' on the remote)…
        assert_eq!(t.press("!", Some("Digit1")), Some(Key::Unicode('1')));
        assert_eq!(t.release("Shift", Some("ShiftLeft")), Some(Key::Shift));
        // …and comes up as "1": the release lifts exactly the key that
        // went down, matched by its physical code regardless of the char.
        assert_eq!(t.release("1", Some("Digit1")), Some(Key::Unicode('1')));
        assert!(t.pressed.is_empty());
        // A keyup nothing matches (older sender) still resolves fresh.
        assert_eq!(t.release("a", Some("KeyA")), Some(Key::Unicode('a')));
    }

    #[test]
    fn release_all_unwinds_in_reverse_press_order() {
        let mut t = KeyTracker::default();
        t.press("Control", Some("ControlLeft"));
        t.press("Shift", Some("ShiftLeft"));
        t.press("T", Some("KeyT"));
        assert_eq!(
            t.release_all(),
            vec![Key::Unicode('t'), Key::Shift, Key::Control]
        );
        assert!(t.pressed.is_empty());
    }

    #[test]
    fn repeated_downs_keep_one_held_entry() {
        let mut t = KeyTracker::default();
        t.press("a", Some("KeyA"));
        t.press("a", Some("KeyA"));
        assert_eq!(t.pressed.len(), 1);
        // Identity falls back to the key value when the code is missing.
        t.press("b", None);
        t.press("b", Some(""));
        assert_eq!(t.pressed.len(), 2);
    }

    #[test]
    fn keycaps_resolve_from_dom_codes() {
        assert_eq!(base_char("KeyA"), Some('a'));
        assert_eq!(base_char("Digit7"), Some('7'));
        assert_eq!(base_char("Numpad3"), Some('3'));
        assert_eq!(base_char("NumpadAdd"), Some('+'));
        assert_eq!(base_char("Space"), Some(' '));
        assert_eq!(base_char("BracketLeft"), Some('['));
        // Named keys have no keycap character — they travel by name.
        assert_eq!(base_char("Enter"), None);
        assert_eq!(base_char("F5"), None);
        assert_eq!(base_char("ControlLeft"), None);
    }

    #[test]
    fn coordinates_denormalize_onto_the_named_screen() {
        // A 1920×1080 primary with a 2560×1440 screen to its right.
        let right = ScreenRect {
            id: 7,
            x: 1920,
            y: 0,
            w: 2560,
            h: 1440,
            primary: false,
        };
        assert_eq!(right.denorm(0.0, 0.0), (1920, 0));
        assert_eq!(right.denorm(1.0, 1.0), (1920 + 2559, 1439));
        let (cx, cy) = right.denorm(0.5, 0.5);
        assert_eq!((cx, cy), (1920 + 1280, 720));
        // Out-of-range input clamps inside the screen, not off the edge.
        assert_eq!(right.denorm(7.0, -3.0), (1920 + 2559, 0));
    }

    #[test]
    fn resolve_prefers_the_named_screen_and_falls_back_to_primary() {
        let primary = ScreenRect {
            id: 1,
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
            primary: true,
        };
        let right = ScreenRect {
            id: 7,
            x: 1920,
            y: 0,
            w: 2560,
            h: 1440,
            primary: false,
        };
        let mut map = ScreenMap {
            rects: vec![primary, right],
            refreshed: Instant::now(),
        };
        assert_eq!(map.resolve(Some(7)), right);
        assert_eq!(map.resolve(None), primary);
        // An unknown id re-queries (nothing here in CI) and lands on the
        // primary of whatever the map now holds.
        let mut map = ScreenMap {
            rects: vec![primary, right],
            refreshed: Instant::now(),
        };
        let fallback = map.resolve(Some(99));
        assert!(fallback.primary);
    }

    #[test]
    fn only_real_mouse_buttons_inject() {
        assert_eq!(dom_button(0), Some(Button::Left));
        assert_eq!(dom_button(2), Some(Button::Right));
        assert_eq!(dom_button(4), None);
    }
}
