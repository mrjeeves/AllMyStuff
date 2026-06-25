//! The input *encode* path: turn what a finger (or the on-screen keyboard)
//! does into the normalized [`InputAction`]s a remote's `control` sink
//! injects.
//!
//! The wire contract is deliberately resolution-free: pointer coordinates are
//! normalized `0..1` over the *source screen* of the paired display route, so
//! the phone never needs the remote's pixel size, and keys travel as DOM
//! `KeyboardEvent.key` / `.code` strings, so layouts resolve on the remote.
//! That makes the phone's job pure arithmetic, which is all this module is.
//!
//! Each [`InputEvent`] carries a monotonic `seq` per route so the remote can
//! spot drops/reorders; [`InputEncoder`] owns that counter.

use allmystuff_session::{InputAction, InputEvent};

/// The DOM mouse-button convention the wire uses: 0 left, 1 middle, 2 right.
pub const BUTTON_LEFT: u8 = 0;
pub const BUTTON_MIDDLE: u8 = 1;
pub const BUTTON_RIGHT: u8 = 2;

/// Encodes a phone's pointer/keyboard intent for one input route, minting the
/// per-route sequence numbers as it goes.
#[derive(Debug, Clone)]
pub struct InputEncoder {
    route: String,
    seq: u64,
    /// Which of the remote's monitors coordinates are normalized over — set
    /// to the `screen:<id>` the console is showing (`None` = primary).
    screen: Option<u32>,
}

impl InputEncoder {
    /// A new encoder for `route`, targeting the remote's primary screen.
    pub fn new(route: impl Into<String>) -> Self {
        InputEncoder {
            route: route.into(),
            seq: 0,
            screen: None,
        }
    }

    /// Aim subsequent moves at a specific remote monitor (`<node>:screen:<id>`
    /// → the bare `<id>`), so control follows the screen the viewer is showing.
    pub fn on_screen(mut self, screen: Option<u32>) -> Self {
        self.screen = screen;
        self
    }

    fn next(&mut self, action: InputAction) -> InputEvent {
        let seq = self.seq;
        self.seq += 1;
        InputEvent::new(self.route.clone(), seq, action)
    }

    /// Move the pointer to a normalized `(x, y)` (each clamped to `0..=1`).
    pub fn move_to(&mut self, x: f64, y: f64) -> InputEvent {
        self.next(InputAction::MouseMove {
            x: clamp01(x),
            y: clamp01(y),
            screen: self.screen,
        })
    }

    /// Press or release a mouse button.
    pub fn button(&mut self, button: u8, down: bool) -> InputEvent {
        self.next(InputAction::MouseButton { button, down })
    }

    /// Scroll, in wheel lines (positive = down / right).
    pub fn wheel(&mut self, dx: f64, dy: f64) -> InputEvent {
        self.next(InputAction::Wheel { dx, dy })
    }

    /// Press or release a key. `key` is the DOM `KeyboardEvent.key` value
    /// (`"a"`, `"Enter"`, `"ArrowLeft"`); `code` is the physical
    /// `KeyboardEvent.code` (`"KeyC"`, `"Digit1"`) — pass it whenever you have
    /// it, since chords (Ctrl+C, Shift+1) need the physical key to land and
    /// release correctly regardless of the held modifiers.
    pub fn key(&mut self, key: impl Into<String>, code: Option<String>, down: bool) -> InputEvent {
        self.next(InputAction::Key {
            key: key.into(),
            code,
            down,
        })
    }

    /// A complete left-click at a normalized point: move, press, release.
    /// The natural mapping of a tap. Returns the three events in order.
    pub fn tap(&mut self, x: f64, y: f64) -> Vec<InputEvent> {
        vec![
            self.move_to(x, y),
            self.button(BUTTON_LEFT, true),
            self.button(BUTTON_LEFT, false),
        ]
    }

    /// A complete key *tap*: down then up. The natural mapping of a
    /// soft-keyboard press.
    pub fn type_key(
        &mut self,
        key: impl Into<String> + Clone,
        code: Option<String>,
    ) -> Vec<InputEvent> {
        vec![
            self.key(key.clone(), code.clone(), true),
            self.key(key, code, false),
        ]
    }
}

/// Clamp to the unit interval — a stray gesture off the edge of the stage
/// can't drive the remote pointer past its own screen.
fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(e: &InputEvent) -> serde_json::Value {
        serde_json::to_value(e).unwrap()
    }

    #[test]
    fn moves_are_normalized_and_clamped() {
        let mut enc = InputEncoder::new("route:desk:screen→phone:display-in");
        let e = enc.move_to(1.4, -0.2);
        let j = val(&e);
        assert_eq!(j["t"], "input");
        assert_eq!(j["kind"], "mouse_move");
        assert_eq!(j["x"], 1.0); // clamped down from 1.4
        assert_eq!(j["y"], 0.0); // clamped up from -0.2
        assert_eq!(j["seq"], 0);
        // Primary screen → the screen key is omitted entirely.
        assert!(j.get("screen").is_none() || j["screen"].is_null());
    }

    #[test]
    fn on_screen_threads_the_monitor_id() {
        let mut enc = InputEncoder::new("r").on_screen(Some(2));
        let j = val(&enc.move_to(0.5, 0.5));
        assert_eq!(j["screen"], 2);
    }

    #[test]
    fn seq_is_monotonic_per_encoder() {
        let mut enc = InputEncoder::new("r");
        assert_eq!(val(&enc.move_to(0.0, 0.0))["seq"], 0);
        assert_eq!(val(&enc.button(BUTTON_LEFT, true))["seq"], 1);
        assert_eq!(val(&enc.button(BUTTON_LEFT, false))["seq"], 2);
    }

    #[test]
    fn tap_expands_to_move_press_release_with_running_seqs() {
        let mut enc = InputEncoder::new("r");
        let events = enc.tap(0.25, 0.75);
        assert_eq!(events.len(), 3);
        let kinds: Vec<_> = events
            .iter()
            .map(|e| val(e)["kind"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(kinds, ["mouse_move", "mouse_button", "mouse_button"]);
        assert_eq!(val(&events[1])["down"], true);
        assert_eq!(val(&events[2])["down"], false);
        // seqs run 0,1,2 across the tap.
        let seqs: Vec<_> = events
            .iter()
            .map(|e| val(e)["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, [0, 1, 2]);
    }

    #[test]
    fn key_carries_code_for_chords() {
        let mut enc = InputEncoder::new("r");
        let j = val(&enc.key("c", Some("KeyC".into()), true));
        assert_eq!(j["kind"], "key");
        assert_eq!(j["key"], "c");
        assert_eq!(j["code"], "KeyC");
        assert_eq!(j["down"], true);

        // A key with no physical code omits it (older-receiver compatible).
        let j2 = val(&enc.key("Enter", None, true));
        assert!(j2.get("code").is_none() || j2["code"].is_null());
    }

    #[test]
    fn type_key_is_down_then_up() {
        let mut enc = InputEncoder::new("r");
        let evs = enc.type_key("a", Some("KeyA".into()));
        assert_eq!(val(&evs[0])["down"], true);
        assert_eq!(val(&evs[1])["down"], false);
    }
}
