//! Bind admitted input routes to the Wayland desktop sessions that supply
//! their pictures. The registry never keeps a capture alive: losing a capture
//! releases its held input and removes its authority to accept more events.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};

use allmystuff_session::InputAction;
use enigo::Key;
use parking_lot::Mutex;

use crate::input_inject::KeyTracker;

/// Input operations on one live compositor session. Absolute coordinates are
/// normalized over this session's captured output, including its logical scale.
pub(crate) trait DesktopControl: Send + Sync {
    fn absolute(&self, x: f64, y: f64) -> Result<(), String>;
    fn relative(&self, dx: f64, dy: f64) -> Result<(), String>;
    fn button(&self, button: i32, down: bool) -> Result<(), String>;
    fn key(&self, keysym: u32, down: bool) -> Result<(), String>;
    fn wheel(&self, dx: f64, dy: f64) -> Result<(), String>;
    fn is_active(&self) -> bool;
}

#[derive(Clone)]
struct Binding {
    id: u64,
    peer: String,
    screen: Option<u32>,
    control: Weak<dyn DesktopControl>,
}

type Routes = Mutex<HashMap<String, RouteState>>;

#[derive(Default)]
struct Registry {
    next_id: u64,
    entries: Vec<Binding>,
    dispatchers: Vec<Weak<Routes>>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}

const PENDING: &str = "Wayland control pending:";

/// A screen route can be accepted before its compositor handshake completes.
/// Only absence of every capture for this peer is retryable; an explicit
/// screen mismatch or ambiguous selection must not type into another output.
pub(crate) fn is_pending(error: &str) -> bool {
    error.starts_with(PENDING)
}

/// The capture owns this guard and must drop it before stopping its compositor
/// session. That order lets route teardown lift held input on the original
/// session, rather than leaving a modifier/button pressed until session death.
pub(crate) struct Registration {
    id: u64,
}

/// `peer` must be the same canonical mesh identity supplied to `Injector::apply`.
pub(crate) fn register(
    peer: &str,
    screen: Option<u32>,
    control: Arc<dyn DesktopControl>,
) -> Registration {
    let mut registry = registry().lock();
    registry.next_id = registry.next_id.wrapping_add(1);
    let id = registry.next_id;
    registry.entries.retain(|e| e.control.strong_count() > 0);
    registry.entries.push(Binding {
        id,
        peer: peer.into(),
        screen,
        control: Arc::downgrade(&control),
    });
    Registration { id }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // Never hold the registry lock while taking a dispatcher's lock: apply
        // looks up a binding while it owns its own route-state lock.
        let dispatchers = {
            let mut registry = registry().lock();
            registry.entries.retain(|entry| entry.id != self.id);
            registry.dispatchers.retain(|d| d.strong_count() > 0);
            registry.dispatchers.clone()
        };
        for dispatcher in dispatchers.into_iter().filter_map(|d| d.upgrade()) {
            dispatcher.lock().retain(|route, state| {
                if state.binding.id != self.id {
                    return true;
                }
                if let Err(error) = state.release() {
                    tracing::warn!(%route, %error, "releasing Wayland capture input failed");
                }
                false
            });
        }
    }
}

fn find_binding(peer: &str, screen: Option<u32>) -> Result<Binding, String> {
    let mut registry = registry().lock();
    registry
        .entries
        .retain(|entry| entry.control.upgrade().is_some_and(|c| c.is_active()));
    let entries: Vec<_> = registry
        .entries
        .iter()
        .filter(|entry| entry.peer == peer)
        .collect();
    if entries.is_empty() {
        return Err(format!(
            "{PENDING} waiting for this peer's shared desktop to become ready"
        ));
    }
    let exact: Vec<_> = entries
        .iter()
        .copied()
        .filter(|entry| entry.screen == screen)
        .collect();
    let candidates = if screen.is_none() && exact.is_empty() && entries.len() == 1 {
        entries
    } else {
        exact
    };
    match candidates.as_slice() {
        [entry] => Ok((*entry).clone()),
        [] => Err(match screen {
            Some(id) => format!(
                "Wayland control unavailable: no live shared desktop for requested screen {id}; reconnect that screen"
            ),
            None => "Wayland control unavailable: no unambiguous live shared desktop; open or select its screen first".into(),
        }),
        _ => Err("Wayland control unavailable: multiple captures match this screen; select an unambiguous desktop".into()),
    }
}

struct RouteState {
    binding: Binding,
    keys: KeyTracker,
    buttons: Vec<i32>,
}

impl RouteState {
    fn new(binding: Binding) -> Self {
        Self {
            binding,
            keys: KeyTracker::default(),
            buttons: Vec::new(),
        }
    }

    fn control(&self) -> Result<Arc<dyn DesktopControl>, String> {
        self.binding
            .control
            .upgrade()
            .filter(|control| control.is_active())
            .ok_or_else(|| "Wayland control session ended; reconnect the shared desktop".into())
    }

    fn release(&mut self) -> Result<(), String> {
        let control = self.control();
        let mut next_keys = self.keys.clone();
        let keys = next_keys.release_all();
        let buttons = self.buttons.clone();
        if keys.is_empty() && buttons.is_empty() {
            return Ok(());
        }
        let control = control?;
        let mut first_error = None;
        for key in keys.into_iter().filter_map(keysym) {
            if let Err(error) = control.key(key, false) {
                first_error.get_or_insert(error);
            }
        }
        for button in buttons.into_iter().rev() {
            if let Err(error) = control.button(button, false) {
                first_error.get_or_insert(error);
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        self.keys = next_keys;
        self.buttons.clear();
        Ok(())
    }
}

/// One worker's input state. A control route remembers the captured session it
/// selected, so a keyup/teardown always targets the same backend as its keydown.
pub(crate) struct Injector {
    routes: Arc<Routes>,
}

impl Default for Injector {
    fn default() -> Self {
        let routes = Arc::new(Mutex::new(HashMap::new()));
        let mut registry = registry().lock();
        registry.dispatchers.retain(|d| d.strong_count() > 0);
        registry.dispatchers.push(Arc::downgrade(&routes));
        Self { routes }
    }
}

impl Injector {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn apply(
        &mut self,
        route: &str,
        peer: &str,
        action: InputAction,
    ) -> Result<(), String> {
        match &action {
            InputAction::Unknown => return Ok(()),
            InputAction::MouseMove { x, y, .. } => finite_pair(*x, *y)?,
            InputAction::MouseMoveRel { dx, dy } | InputAction::Wheel { dx, dy } => {
                finite_pair(*dx, *dy)?;
                if *dx == 0.0 && *dy == 0.0 {
                    return Ok(());
                }
            }
            InputAction::MouseButton { button, .. } if evdev_button(*button).is_none() => {
                return Ok(());
            }
            _ => {}
        }

        let mut routes = self.routes.lock();
        let current = routes.get(route).filter(|state| state.binding.peer == peer);
        let binding = match &action {
            InputAction::MouseMove { screen, .. } => find_binding(peer, *screen)?,
            _ => match current {
                Some(state) if state.control().is_ok() => state.binding.clone(),
                _ => find_binding(peer, None)?,
            },
        };
        let replace = routes
            .get(route)
            .is_some_and(|state| state.binding.id != binding.id || state.binding.peer != peer);
        if replace {
            if let Some(old) = routes.get_mut(route) {
                old.release()?;
            }
            routes.remove(route);
        }
        let state = routes
            .entry(route.into())
            .or_insert_with(|| RouteState::new(binding));
        let control = state.control()?;
        match action {
            InputAction::MouseMove { x, y, .. } => {
                control.absolute(x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))
            }
            InputAction::MouseMoveRel { dx, dy } => control.relative(dx, dy),
            InputAction::MouseButton { button, down } => {
                let Some(button) = evdev_button(button) else {
                    return Ok(());
                };
                if state.buttons.contains(&button) == down {
                    return Ok(());
                }
                control.button(button, down)?;
                state.buttons.retain(|held| *held != button);
                if down {
                    state.buttons.push(button);
                }
                Ok(())
            }
            InputAction::Wheel { dx, dy } => control.wheel(dx, dy),
            InputAction::Key { key, code, down } => {
                // A rejected OS event must not commit a pretend key state: in
                // particular a failed keyup remains held for teardown to retry.
                let mut next = state.keys.clone();
                let key = if down {
                    next.press(&key, code.as_deref())
                } else {
                    next.release(&key, code.as_deref())
                };
                if let Some(key) = key.and_then(keysym) {
                    control.key(key, down)?;
                    state.keys = next;
                }
                Ok(())
            }
            InputAction::Unknown => Ok(()),
        }
    }

    pub(crate) fn release_route(&mut self, route: &str) {
        if let Some(mut state) = self.routes.lock().remove(route) {
            if let Err(error) = state.release() {
                tracing::warn!(%route, %error, "releasing Wayland route input failed");
            }
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        for (route, mut state) in self.routes.lock().drain() {
            if let Err(error) = state.release() {
                tracing::warn!(%route, %error, "releasing Wayland worker input failed");
            }
        }
    }
}

fn finite_pair(x: f64, y: f64) -> Result<(), String> {
    if x.is_finite() && y.is_finite() {
        Ok(())
    } else {
        Err("Wayland input coordinates must be finite".into())
    }
}

fn evdev_button(button: u8) -> Option<i32> {
    Some(match button {
        0 => 0x110, // BTN_LEFT
        1 => 0x112, // BTN_MIDDLE
        2 => 0x111, // BTN_RIGHT
        3 => 0x113, // BTN_SIDE
        4 => 0x114, // BTN_EXTRA
        _ => return None,
    })
}

/// The compositor takes XKB keysyms, rather than Unicode scalar values or
/// Enigo's platform-specific enum representation.
fn keysym(key: Key) -> Option<u32> {
    Some(match key {
        Key::Unicode(c) if c != '\0' => match c as u32 {
            value @ 0x20..=0x7e | value @ 0xa0..=0xff => value,
            value => 0x0100_0000 | value,
        },
        Key::Backspace => 0xff08,
        Key::Tab => 0xff09,
        Key::Return => 0xff0d,
        Key::Escape => 0xff1b,
        Key::Home => 0xff50,
        Key::LeftArrow => 0xff51,
        Key::UpArrow => 0xff52,
        Key::RightArrow => 0xff53,
        Key::DownArrow => 0xff54,
        Key::PageUp => 0xff55,
        Key::PageDown => 0xff56,
        Key::End => 0xff57,
        #[cfg(not(target_os = "macos"))]
        Key::Insert => 0xff63,
        Key::Delete => 0xffff,
        Key::Shift => 0xffe1,
        Key::Control => 0xffe3,
        Key::CapsLock => 0xffe5,
        Key::Alt => 0xffe9,
        Key::Meta => 0xffeb,
        Key::F1 => 0xffbe,
        Key::F2 => 0xffbf,
        Key::F3 => 0xffc0,
        Key::F4 => 0xffc1,
        Key::F5 => 0xffc2,
        Key::F6 => 0xffc3,
        Key::F7 => 0xffc4,
        Key::F8 => 0xffc5,
        Key::F9 => 0xffc6,
        Key::F10 => 0xffc7,
        Key::F11 => 0xffc8,
        Key::F12 => 0xffc9,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    #[derive(Debug, PartialEq)]
    enum Event {
        Absolute(f64, f64),
        Relative(f64, f64),
        Button(i32, bool),
        Key(u32, bool),
        Wheel(f64, f64),
    }

    struct Mock {
        active: AtomicBool,
        fail_next: AtomicBool,
        events: Mutex<Vec<Event>>,
    }

    impl Mock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                active: AtomicBool::new(true),
                fail_next: AtomicBool::new(false),
                events: Mutex::new(Vec::new()),
            })
        }

        fn emit(&self, event: Event) -> Result<(), String> {
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Err("mock injection failed".into());
            }
            self.events.lock().push(event);
            Ok(())
        }
    }

    impl DesktopControl for Mock {
        fn absolute(&self, x: f64, y: f64) -> Result<(), String> {
            self.emit(Event::Absolute(x, y))
        }
        fn relative(&self, x: f64, y: f64) -> Result<(), String> {
            self.emit(Event::Relative(x, y))
        }
        fn button(&self, b: i32, down: bool) -> Result<(), String> {
            self.emit(Event::Button(b, down))
        }
        fn key(&self, k: u32, down: bool) -> Result<(), String> {
            self.emit(Event::Key(k, down))
        }
        fn wheel(&self, x: f64, y: f64) -> Result<(), String> {
            self.emit(Event::Wheel(x, y))
        }
        fn is_active(&self) -> bool {
            self.active.load(Ordering::SeqCst)
        }
    }

    fn peer() -> String {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        format!("wayland-test-{}", NEXT.fetch_add(1, Ordering::SeqCst))
    }

    fn move_to(screen: Option<u32>, x: f64, y: f64) -> InputAction {
        InputAction::MouseMove { x, y, screen }
    }

    fn key(key: &str, code: &str, down: bool) -> InputAction {
        InputAction::Key {
            key: key.into(),
            code: Some(code.into()),
            down,
        }
    }

    #[test]
    fn registry_isolates_peers_and_never_redirects_an_explicit_screen() {
        let p = peer();
        let control = Mock::new();
        let _guard = register(&p, Some(7), control.clone());
        let mut injector = Injector::new();
        let absent = injector
            .apply("r", "different-peer", move_to(Some(7), 0.0, 0.0))
            .unwrap_err();
        assert!(is_pending(&absent));
        let wrong_screen = injector
            .apply("r", &p, move_to(Some(9), 0.0, 0.0))
            .unwrap_err();
        assert!(!is_pending(&wrong_screen));
        assert!(control.events.lock().is_empty());
        injector.apply("r", &p, move_to(Some(7), 0.0, 1.0)).unwrap();
        assert_eq!(*control.events.lock(), vec![Event::Absolute(0.0, 1.0)]);
    }

    #[test]
    fn unspecified_screen_uses_only_a_unique_primary_or_single_capture() {
        let p = peer();
        let primary = Mock::new();
        let second = Mock::new();
        let _g1 = register(&p, None, primary.clone());
        let _g2 = register(&p, Some(2), second.clone());
        let mut injector = Injector::new();
        injector.apply("r", &p, move_to(None, 0.5, 0.5)).unwrap();
        let duplicate = Mock::new();
        let g3 = register(&p, None, duplicate.clone());
        assert!(injector
            .apply("r", &p, move_to(None, 0.5, 0.5))
            .unwrap_err()
            .contains("multiple"));
        drop(g3);
        let other = peer();
        let _g4 = register(&other, Some(4), second.clone());
        injector
            .apply("s", &other, move_to(None, 0.5, 0.5))
            .unwrap();
        let _g5 = register(&other, Some(5), primary.clone());
        assert!(injector
            .apply("s", &other, move_to(None, 0.5, 0.5))
            .is_err());
    }

    #[test]
    fn validates_coordinates_before_forwarding_and_ignores_noop_actions() {
        let p = peer();
        let control = Mock::new();
        let _guard = register(&p, None, control.clone());
        let mut injector = Injector::new();
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(injector.apply("r", &p, move_to(None, value, 0.0)).is_err());
            assert!(injector
                .apply("r", &p, InputAction::MouseMoveRel { dx: 0.0, dy: value })
                .is_err());
            assert!(injector
                .apply("r", &p, InputAction::Wheel { dx: value, dy: 0.0 })
                .is_err());
        }
        injector.apply("r", &p, move_to(None, -1.0, 2.0)).unwrap();
        injector
            .apply("none", "missing", InputAction::Unknown)
            .unwrap();
        injector
            .apply(
                "none",
                "missing",
                InputAction::MouseMoveRel { dx: 0.0, dy: 0.0 },
            )
            .unwrap();
        injector
            .apply("none", "missing", InputAction::Wheel { dx: 0.0, dy: 0.0 })
            .unwrap();
        assert_eq!(*control.events.lock(), vec![Event::Absolute(0.0, 1.0)]);
    }

    #[test]
    fn teardown_releases_normalized_chord_and_mouse_button() {
        let p = peer();
        let control = Mock::new();
        let _guard = register(&p, None, control.clone());
        let mut injector = Injector::new();
        injector
            .apply("r", &p, key("Control", "ControlLeft", true))
            .unwrap();
        injector.apply("r", &p, key("ç", "KeyC", true)).unwrap();
        injector
            .apply(
                "r",
                &p,
                InputAction::MouseButton {
                    button: 0,
                    down: true,
                },
            )
            .unwrap();
        injector.release_route("r");
        assert_eq!(
            *control.events.lock(),
            vec![
                Event::Key(0xffe3, true),
                Event::Key(u32::from('c'), true),
                Event::Button(0x110, true),
                Event::Key(u32::from('c'), false),
                Event::Key(0xffe3, false),
                Event::Button(0x110, false),
            ]
        );
    }

    #[test]
    fn keyup_uses_its_original_down_even_when_modifiers_changed() {
        let p = peer();
        let control = Mock::new();
        let _guard = register(&p, None, control.clone());
        let mut injector = Injector::new();
        for event in [
            key("Shift", "ShiftLeft", true),
            key("!", "Digit1", true),
            key("Shift", "ShiftLeft", false),
            key("1", "Digit1", false),
        ] {
            injector.apply("r", &p, event).unwrap();
        }
        assert_eq!(
            *control.events.lock(),
            vec![
                Event::Key(0xffe1, true),
                Event::Key(u32::from('1'), true),
                Event::Key(0xffe1, false),
                Event::Key(u32::from('1'), false)
            ]
        );
    }

    #[test]
    fn switching_capture_releases_the_previous_backend_before_moving() {
        let p = peer();
        let first = Mock::new();
        let second = Mock::new();
        let _g1 = register(&p, None, first.clone());
        let _g2 = register(&p, Some(2), second.clone());
        let mut injector = Injector::new();
        injector
            .apply("r", &p, key("Alt", "AltLeft", true))
            .unwrap();
        injector.apply("r", &p, move_to(Some(2), 0.0, 0.0)).unwrap();
        injector.apply("r", &p, key("a", "KeyA", true)).unwrap();
        assert_eq!(
            *first.events.lock(),
            vec![Event::Key(0xffe9, true), Event::Key(0xffe9, false)]
        );
        assert_eq!(
            *second.events.lock(),
            vec![Event::Absolute(0.0, 0.0), Event::Key(u32::from('a'), true)]
        );
    }

    #[test]
    fn removing_capture_releases_state_and_stops_future_input() {
        let p = peer();
        let control = Mock::new();
        let guard = register(&p, None, control.clone());
        let mut injector = Injector::new();
        injector
            .apply("r", &p, key("Shift", "ShiftLeft", true))
            .unwrap();
        injector
            .apply(
                "r",
                &p,
                InputAction::MouseButton {
                    button: 2,
                    down: true,
                },
            )
            .unwrap();
        drop(guard);
        assert!(injector.apply("r", &p, key("a", "KeyA", true)).is_err());
        assert_eq!(
            *control.events.lock(),
            vec![
                Event::Key(0xffe1, true),
                Event::Button(0x111, true),
                Event::Key(0xffe1, false),
                Event::Button(0x111, false)
            ]
        );
        assert!(injector.routes.lock().is_empty());
    }

    #[test]
    fn failed_keyup_retains_held_state_for_cleanup_and_failed_down_does_not() {
        let p = peer();
        let control = Mock::new();
        let _guard = register(&p, None, control.clone());
        let mut injector = Injector::new();
        injector.apply("r", &p, key("a", "KeyA", true)).unwrap();
        control.fail_next.store(true, Ordering::SeqCst);
        assert!(injector.apply("r", &p, key("a", "KeyA", false)).is_err());
        injector.release_route("r");
        control.fail_next.store(true, Ordering::SeqCst);
        assert!(injector.apply("s", &p, key("b", "KeyB", true)).is_err());
        injector.release_route("s");
        assert_eq!(
            *control.events.lock(),
            vec![
                Event::Key(u32::from('a'), true),
                Event::Key(u32::from('a'), false)
            ]
        );
    }

    #[test]
    fn a_failed_release_keeps_the_original_binding_for_retry() {
        let p = peer();
        let first = Mock::new();
        let second = Mock::new();
        let _g1 = register(&p, None, first.clone());
        let _g2 = register(&p, Some(2), second.clone());
        let mut injector = Injector::new();
        injector
            .apply("r", &p, key("Shift", "ShiftLeft", true))
            .unwrap();
        first.fail_next.store(true, Ordering::SeqCst);
        assert!(injector.apply("r", &p, move_to(Some(2), 0.5, 0.5)).is_err());
        assert!(second.events.lock().is_empty());
        injector.apply("r", &p, move_to(Some(2), 0.5, 0.5)).unwrap();
        assert_eq!(
            *first.events.lock(),
            vec![Event::Key(0xffe1, true), Event::Key(0xffe1, false)]
        );
        assert_eq!(*second.events.lock(), vec![Event::Absolute(0.5, 0.5)]);
    }

    #[test]
    fn registry_and_route_state_do_not_keep_capture_alive() {
        let p = peer();
        let control = Mock::new();
        let weak = Arc::downgrade(&control);
        let _guard = register(&p, None, control.clone());
        let mut injector = Injector::new();
        injector.apply("r", &p, move_to(None, 0.0, 0.0)).unwrap();
        drop(control);
        assert!(weak.upgrade().is_none());
        assert!(injector.apply("r", &p, key("a", "KeyA", true)).is_err());
    }

    #[test]
    fn keysyms_use_xkb_unicode_and_named_key_encodings() {
        assert_eq!(keysym(Key::Unicode('ü')), Some(0xfc));
        assert_eq!(keysym(Key::Unicode('界')), Some(0x0100_754c));
        assert_eq!(keysym(Key::Meta), Some(0xffeb));
        assert_eq!(keysym(Key::Unicode('\0')), None);
    }
}
