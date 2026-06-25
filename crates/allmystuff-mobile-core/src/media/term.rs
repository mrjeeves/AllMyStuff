//! A terminal route, both directions, for an xterm.js view on the phone.
//!
//! Bytes are opaque on the wire — the emulator and the remote PTY speak VT
//! between themselves; the phone just carries keystrokes up
//! ([`TermEvent::Data`]) and resizes up ([`TermEvent::Resize`]), and renders
//! the PTY bytes that come down. [`TermPlane`] owns the per-route `seq`
//! counter for what it sends and demuxes what arrives.

use allmystuff_session::{MediaPayload, TermEvent, TermFrame};

/// One end of a terminal route. Send keystrokes + resizes; feed inbound media
/// payloads to [`TermPlane::accept`] to pull out the PTY output for this
/// route.
#[derive(Debug, Clone)]
pub struct TermPlane {
    route: String,
    seq: u64,
}

impl TermPlane {
    pub fn new(route: impl Into<String>) -> Self {
        TermPlane {
            route: route.into(),
            seq: 0,
        }
    }

    pub fn route(&self) -> &str {
        &self.route
    }

    fn next(&mut self, event: TermEvent) -> TermFrame {
        let seq = self.seq;
        self.seq += 1;
        TermFrame::new(self.route.clone(), seq, event)
    }

    /// Send raw keystroke bytes to the remote shell (what xterm.js produces
    /// from a keypress, paste, or the on-screen key bar).
    pub fn send(&mut self, bytes: impl Into<Vec<u8>>) -> TermFrame {
        self.next(TermEvent::Data {
            bytes: bytes.into(),
        })
    }

    /// Tell the host the emulator's new size so it resizes the PTY and the
    /// shell reflows.
    pub fn resize(&mut self, cols: u16, rows: u16) -> TermFrame {
        self.next(TermEvent::Resize { cols, rows })
    }

    /// Pull this route's terminal event out of an inbound media payload, or
    /// `None` if it's not a terminal frame for *this* route. Returns the
    /// host's [`TermEvent::Data`] (bytes to write to the emulator) or
    /// [`TermEvent::Exit`] (the shell ended).
    pub fn accept(&self, payload: &MediaPayload) -> Option<TermEvent> {
        match payload {
            MediaPayload::Terminal(frame) if frame.route == self.route => Some(frame.event.clone()),
            _ => None,
        }
    }

    /// Convenience over [`TermPlane::accept`]: just the PTY bytes to paint,
    /// dropping resize/exit/unknown events.
    pub fn accept_output(&self, payload: &MediaPayload) -> Option<Vec<u8>> {
        match self.accept(payload) {
            Some(TermEvent::Data { bytes }) => Some(bytes),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(f: &TermFrame) -> serde_json::Value {
        serde_json::to_value(f).unwrap()
    }

    #[test]
    fn keystrokes_carry_base64_data_and_increment_seq() {
        let mut t = TermPlane::new("route:desk:terminal→phone:term-view:1");
        let a = val(&t.send(b"ls\n".to_vec()));
        assert_eq!(a["t"], "term");
        assert_eq!(a["kind"], "data");
        assert_eq!(a["seq"], 0);
        // bytes travel as a base64 string, never a number array.
        assert!(a["bytes"].is_string());

        let b = val(&t.resize(120, 40));
        assert_eq!(b["kind"], "resize");
        assert_eq!(b["cols"], 120);
        assert_eq!(b["rows"], 40);
        assert_eq!(b["seq"], 1);
    }

    #[test]
    fn round_trips_a_data_frame_back_to_its_bytes() {
        // Build the frame the host would send, by serializing one we made and
        // decoding it the way the media channel does.
        let mut host = TermPlane::new("route:r");
        let on_wire = serde_json::to_value(host.send(b"hi".to_vec())).unwrap();
        let payload = MediaPayload::decode(on_wire).expect("decodes as a term frame");

        let viewer = TermPlane::new("route:r");
        assert_eq!(viewer.accept_output(&payload), Some(b"hi".to_vec()));
    }

    #[test]
    fn output_for_a_different_route_is_ignored() {
        let mut host = TermPlane::new("route:other");
        let payload =
            MediaPayload::decode(serde_json::to_value(host.send(b"x".to_vec())).unwrap()).unwrap();
        let viewer = TermPlane::new("route:mine");
        assert!(viewer.accept(&payload).is_none());
    }

    #[test]
    fn exit_is_surfaced_but_not_as_output() {
        let mut host = TermPlane::new("route:r");
        // Serialize an Exit frame and decode it.
        let exit = host.next(TermEvent::Exit { code: Some(0) });
        let payload = MediaPayload::decode(serde_json::to_value(exit).unwrap()).unwrap();
        let viewer = TermPlane::new("route:r");
        assert!(matches!(
            viewer.accept(&payload),
            Some(TermEvent::Exit { code: Some(0) })
        ));
        assert!(viewer.accept_output(&payload).is_none());
    }
}
