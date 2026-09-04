//! No-op twin of [`crate::video`] for capture-less builds (`--no-default-features`,
//! i.e. iOS — see the `host` feature in `Cargo.toml`).
//!
//! Same public surface, no capture: starting a stream immediately reports
//! [`VideoStatusState::GrabFailed`] with an honest reason, so a viewer that
//! points a route at this node sees "can't capture here" instead of a black
//! rectangle with no explanation. Decode (the *viewer* side, `video_decode`)
//! is a separate ungated module — this node still watches other machines.

use std::sync::Arc;
use std::time::Instant;

use allmystuff_session::{VideoFrame, VideoStatusState};

/// Which transport a video-carrying route's stream encodes for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoMode {
    /// Standalone JPEG frames over the media channel.
    Mjpeg,
    /// H.264 access units for the mesh's RTP track lane.
    H264,
}

/// What a route's capture thread would point at, if this build had one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoSource {
    /// A monitor: `None` = the primary, `Some(id)` = an extra screen.
    Screen(Option<u32>),
    /// A camera, by inventory device id.
    Camera(String),
}

/// One capture tick's output, headed for the forwarder.
#[derive(Debug, Clone)]
pub enum VideoPacket {
    Jpeg(VideoFrame),
    H264 {
        /// Annex-B access unit.
        data: Vec<u8>,
        /// Process-local recovery metadata; never serialized.
        key: bool,
        /// Capture-tick pacing for the RTP clock (1/fps).
        duration_us: u64,
    },
}

/// Whether per-stream dial-in stats log at info — mirrored so shared call
/// sites in `mesh.rs` compile; nothing here ever produces a stream to log.
pub(crate) fn stats_to_info() -> bool {
    false
}

/// How a video route's path to its viewer flows — mirrored from
/// [`crate::video::LinkClass`] so the shared LAN-gate call sites in
/// `mesh.rs` (`link_class_of`, `seed_peer_links`, `Tune { link, .. }`)
/// compile on a capture-less build. Inert here: nothing streams, so the
/// class never governs a dial.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LinkClass {
    /// The nominated candidate pair is host↔host — a direct local link.
    Lan,
    /// Reflexive or relayed — real internet between the ends.
    Wan,
    /// No nominated pair reported (yet). Conservative until known.
    #[default]
    Unknown,
}

/// One stream's viewer-requested overrides. Accepted and ignored — there is
/// no encoder to re-tune. `link` carries the LAN-gate class for surface
/// parity with the real [`crate::video::Tune`]; it governs nothing here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tune {
    pub max_edge: Option<u32>,
    pub bitrate: Option<u32>,
    pub fps: Option<u32>,
    pub link: LinkClass,
    pub game: bool,
    pub mode: Option<Posture>,
}

/// Mirror of the real module's posture dial (capture-less builds carry
/// the type so `mesh.rs` compiles identically).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Posture {
    #[default]
    Balanced,
    Game,
    Studio,
    StudioLossless,
}

/// Mirror of the real module's wire-name parse.
pub fn parse_posture(s: &str) -> Option<Posture> {
    match s {
        "game" => Some(Posture::Game),
        "studio" => Some(Posture::Studio),
        "studio-lossless" => Some(Posture::StudioLossless),
        "balanced" => Some(Posture::Balanced),
        _ => None,
    }
}

/// Mirror of the real module's pacer grain (the send path in `mesh.rs`
/// is compiled on capture-less builds too — a viewer node still forwards
/// what it's asked to).
pub(crate) const PACE_SLICE_BYTES: usize = 24 * 1024;

/// Mirror of the real module's pacer dial — enabled unless explicitly off.
pub(crate) fn paced_slices_enabled() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        !std::env::var("ALLMYSTUFF_PACED_SLICES")
            .ok()
            .is_some_and(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "0" | "off" | "false"
                )
            })
    });
    *ON
}

const PACED_AU_MARKER_UUID: &[u8; 16] = b"AMS-PACED-AU-V1!";

pub(crate) fn paced_au_marker(chunks: usize) -> Vec<u8> {
    let chunks = u16::try_from(chunks).unwrap_or(u16::MAX);
    let mut out = Vec::with_capacity(26);
    out.extend_from_slice(&[0, 0, 0, 1, 0x06, 0x05, 18]);
    out.extend_from_slice(PACED_AU_MARKER_UUID);
    out.extend_from_slice(&chunks.to_le_bytes());
    out.push(0x80);
    out
}

pub(crate) fn paced_au_marker_count(data: &[u8]) -> Option<usize> {
    if data.len() != 26
        || data[..7] != [0, 0, 0, 1, 0x06, 0x05, 18]
        || &data[7..23] != PACED_AU_MARKER_UUID
        || data[25] != 0x80
    {
        return None;
    }
    Some(u16::from_le_bytes([data[23], data[24]]) as usize)
}

/// Mirror of the real module's paced splitter — pure byte logic, kept
/// identical (both codecs, glue rules, byte-exact concatenation).
pub(crate) fn split_annexb_paced(data: &[u8], max_chunk: usize) -> Vec<std::ops::Range<usize>> {
    let mut nals: Vec<(usize, u8)> = Vec::new();
    let mut i = 0usize;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            let hdr = if data[i + 2] == 1 {
                i + 3
            } else if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                i + 4
            } else {
                i += 1;
                continue;
            };
            if hdr < data.len() {
                nals.push((i, data[hdr]));
            }
            i = hdr + 1;
        } else {
            i += 1;
        }
    }
    // Exact parameter-set bytes — see the real module's comment: a masked
    // type test collides with H.264's 0x41 referenced P slice.
    let hevc = nals.iter().any(|&(_, b)| matches!(b, 0x40 | 0x42 | 0x44));
    let is_slice = |b: u8| {
        if hevc {
            b & 0x80 == 0 && ((b >> 1) & 0x3F) <= 21
        } else {
            matches!(b & 0x1F, 1 | 5)
        }
    };
    let mut unit_starts: Vec<usize> = Vec::new();
    let mut pending: Option<usize> = None;
    for &(off, b) in &nals {
        if is_slice(b) {
            unit_starts.push(pending.take().unwrap_or(off));
        } else if pending.is_none() {
            pending = Some(off);
        }
    }
    if unit_starts.len() < 2 {
        return std::iter::once(0..data.len()).collect();
    }
    unit_starts[0] = 0;
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    let mut chunk_start = 0usize;
    let mut chunk_end = 0usize;
    for (k, &s) in unit_starts.iter().enumerate() {
        let e = unit_starts.get(k + 1).copied().unwrap_or(data.len());
        if chunk_end > chunk_start && e - chunk_start > max_chunk {
            out.push(chunk_start..chunk_end);
            chunk_start = s;
        }
        chunk_end = e;
    }
    out.push(chunk_start..chunk_end);
    out
}

/// The host side of a capture-status report: state + optional OS error text.
pub type OnStatus = Arc<dyn Fn(VideoStatusState, Option<String>) + Send + Sync>;

/// The latest decode health a viewer reported for one of our outbound
/// streams. Still recorded — feedback arrives over the wire regardless of
/// whether this node can capture — so the sender-side logging keeps working
/// if a route ever *is* offered.
#[derive(Debug, Clone, Copy)]
pub struct RecvFeedback {
    pub recv_fps: u32,
    pub decode_fails: u32,
    pub queue_depth: u32,
    /// Wire-parity with the real bridge: the viewer's link estimate and
    /// delay trend still arrive; recorded, never acted on (no encoder).
    pub est_kbps: u32,
    pub delay_trend_us_per_s: i32,
    pub at: Instant,
}

/// Mirror of [`crate::video::PipelineFeedback`] — the opaque-ext seam is
/// used by `mesh.rs`, compiled on capture-less builds too. Same shape,
/// same (de)serialization; a viewer node parses/relays feedback exactly
/// like the real backend even though it never streams.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct PipelineFeedback {
    #[serde(default)]
    pub est_kbps: u32,
    #[serde(default)]
    pub delay_trend_us_per_s: i32,
}

impl PipelineFeedback {
    pub fn to_ext(&self) -> serde_json::Value {
        if self.est_kbps == 0 && self.delay_trend_us_per_s == 0 {
            return serde_json::Value::Null;
        }
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
    pub fn from_ext(ext: &serde_json::Value) -> Self {
        serde_json::from_value(ext.clone()).unwrap_or_default()
    }
}

/// Mirror of [`crate::video::RouteDials`] for surface parity — the GUI's
/// effective-dials op compiles against this build too, though a capture-less
/// node never streams and so always answers `None` (see
/// [`VideoBridge::route_dials`]).
#[derive(Debug, Clone)]
pub struct RouteDials {
    pub posture: &'static str,
    pub encoder_label: String,
    pub codec: &'static str,
    pub target_bitrate_bps: u32,
    pub ceiling_bps: u32,
    pub rate_floor_bps: u32,
    pub fps_target: u32,
    pub edge_cap: u32,
    pub out_w: u32,
    pub out_h: u32,
    pub recv_fps: u32,
    pub decode_fails: u32,
    pub queue_depth: u32,
    pub est_kbps: u32,
    pub delay_trend_us_per_s: i32,
    pub feedback_age_ms: Option<u64>,
}

/// Handle the mesh holds either way; capture starts fail loudly, everything
/// else is a shrug.
#[derive(Default)]
pub struct VideoBridge {
    feedback: parking_lot::Mutex<std::collections::HashMap<String, RecvFeedback>>,
}

impl VideoBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// No capture backend exists on this build: report the failure straight
    /// through `on_status` so the viewer's `vstat` line says why the stream
    /// is dark, and produce no packets.
    pub fn start_capture<F, S>(
        &self,
        route_id: String,
        _mode: VideoMode,
        _source: VideoSource,
        _tune: Tune,
        _on_packet: F,
        on_status: S,
    ) where
        F: Fn(VideoPacket) -> bool + Send + Sync + 'static,
        S: Fn(VideoStatusState, Option<String>) + Send + Sync + 'static,
    {
        tracing::info!("video capture for {route_id} unavailable: capture-less build");
        on_status(
            VideoStatusState::GrabFailed,
            Some("this device cannot capture its screen or camera".into()),
        );
    }

    pub fn force_idr(&self, _route_id: &str) {}

    pub fn route_supports_gdr(&self, _route_id: &str) -> bool {
        false
    }

    /// Mirror of the real bridge's frame-health heal — no capture routes
    /// exist on this build, so there is never a lane to wave.
    pub fn route_wave_or_refresh(&self, _route_id: &str) {}

    /// No capture routes exist on this build, so there are never any ids to
    /// re-class. Present for surface parity with the real bridge (the LAN
    /// gate sweeps this in `refresh_peer_networks`).
    pub fn route_ids(&self) -> Vec<String> {
        Vec::new()
    }

    /// The LAN gate learning a route's link class — a no-op with nothing
    /// captured here. Returns `false` (no retune happened), the same
    /// contract as the real bridge on an unchanged/absent route.
    pub fn retune_link(&self, _route_id: &str, _link: LinkClass) -> bool {
        false
    }

    /// A viewer's Tune against a stream this build can't produce — accepted
    /// and ignored, matching the real bridge's signature.
    pub fn retune_dials(
        &self,
        _route_id: &str,
        _max_edge: Option<u32>,
        _bitrate: Option<u32>,
        _fps: Option<u32>,
        _game: bool,
        _mode: Option<&str>,
    ) {
    }

    /// Mirror of the real bridge's per-route posture read (the mesh
    /// forwarder's pacing hint) — nothing streams here, so balanced.
    pub fn route_game(&self, _route_id: &str) -> bool {
        false
    }

    /// The GUI effective-dials read — a capture-less node never streams a
    /// route, so there are never any dials to report. `None`, the same
    /// contract the real bridge returns for a route it isn't the streamer of.
    pub fn route_dials(&self, _route_id: &str) -> Option<RouteDials> {
        None
    }

    /// Mirror of the real bridge's pacing read — nothing streams here,
    /// so the pacer keeps its constants.
    pub fn route_pace(&self, _route_id: &str) -> (bool, bool, u32, u32) {
        (false, false, 0, 60)
    }

    /// Keep the capture-less surface aligned with the real bridge. There is no
    /// encoder rate controller to hold here, but stale receiver health still
    /// belongs to the recovery episode and must be cleared.
    pub fn note_recovery(&self, route_id: &str) {
        self.feedback.lock().remove(route_id);
    }

    pub fn note_feedback(
        &self,
        route_id: &str,
        recv_fps: u32,
        decode_fails: u32,
        queue_depth: u32,
        est_kbps: u32,
        delay_trend_us_per_s: i32,
    ) {
        self.feedback.lock().insert(
            route_id.to_string(),
            RecvFeedback {
                recv_fps,
                decode_fails,
                queue_depth,
                est_kbps,
                delay_trend_us_per_s,
                at: Instant::now(),
            },
        );
    }

    #[allow(dead_code)]
    pub fn latest_feedback(&self, route_id: &str) -> Option<RecvFeedback> {
        self.feedback.lock().get(route_id).copied()
    }

    pub fn retune(&self, _route_id: &str, _tune: Tune) {}

    pub fn stop(&self, route_id: &str) {
        self.feedback.lock().remove(route_id);
    }
}

/// No monitors to enumerate — the capability list simply carries no
/// `screen:<id>` entries (and no primary `screen` either; the bridge layer
/// decides that from the scan, not from here).
pub fn extra_screens() -> Vec<allmystuff_bridge::ScreenSource> {
    Vec::new()
}
