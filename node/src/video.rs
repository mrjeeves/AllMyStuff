//! The video media plane: capture of this machine's screens **and
//! cameras**, so an active display or camera route actually streams
//! pixels — H.264 on the mesh's track lane, MJPEG as the compatibility
//! floor (the piKVM transport: every frame a standalone JPEG, no codec
//! state to desync).
//!
//! Mirrors [`crate::audio::AudioBridge`]'s shape: each sourcing route runs
//! a dedicated thread that captures **what its source capability names**
//! (the synthetic `screen` is the primary monitor, `screen:<id>` one of
//! the others, a camera capability the OS camera it scanned — see
//! [`VideoSource`]), fits it to the transport's ceiling, encodes, and
//! hands the packet to a callback the mesh forwards. Everything from the
//! encoder down — transports, tuning, refresh asks, stats, the status
//! reports — is one pipeline; only the frame *source* differs, and camera
//! frames enter through the same pump the persistent screen sessions use
//! ([`crate::camera_capture`]).
//!
//! Capture prefers a **persistent session**: our own DXGI Output
//! Duplication on Windows (see [`crate::win_capture`] for why xcap's
//! recorder can't carry it), `xcap`'s `VideoRecorder` elsewhere (PipeWire
//! ScreenCast on Wayland, AVFoundation on macOS). The OS negotiates the
//! stream once per route and pushes frames, often only on damage. The
//! alternative — one `capture_image()` screenshot per tick — pays the
//! platform's full one-shot cost every frame (the Wayland portal literally
//! has the compositor write a PNG to disk per call), which is what made
//! v1's framerate so dire. The paced one-shot loop remains as the X11 path
//! (xcap's X11 "recorder" is that same screenshot in an unpaced hot loop)
//! and as the fallback wherever a session can't start (denied portal,
//! headless session, an output another session holds) — so the stream
//! degrades to v1 behaviour, never to nothing.
//!
//! The H.264 encoder budgets its bitrate from the monitor's true pixel
//! count and runs native resolution up to 4K by default; the edge, rate,
//! and bitrate are env-dialable for dial-in sessions (see the knobs by
//! [`target_fps`]).
//!
//! Two costs are skipped outright when they buy nothing: a frame whose
//! pixels match the previous one isn't re-encoded or re-sent (an idle
//! desktop costs one buffer compare per tick, with a periodic refresh so
//! late joiners aren't stranded), and when the link can't keep up the
//! bounded forwarder drops captures rather than queueing stale ones.
//!
//! Three failure modes the capture used to suffer silently are handled
//! here as part of the stream:
//!
//!  * **A sleeping display.** Hosting a route holds a keep-awake guard
//!    and nudges an already-asleep display at start (see [`crate::wake`])
//!    — damage-driven backends produce nothing from a dark screen, and a
//!    deep-sleeping DisplayPort monitor detaches from the desktop
//!    entirely (which is why a failed monitor lookup gets one retry
//!    after the nudge has had a beat to re-attach things).
//!  * **Wayland consent.** Route starts ride a portal session with a
//!    **restore token** ([`crate::wayland_capture`]): the compositor's
//!    share-picker is a once-per-machine event, and every start after it
//!    is silent — which is what an unattended host needs.
//!  * **Silence with no explanation.** Capture state changes travel to
//!    the viewer in-band (`vstat` media frames, [`StatusReporter`]):
//!    "waiting for consent", "display asleep", "no monitor", "grabs
//!    failing" — so the far end reads the real condition, never a
//!    wordless black stage.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use allmystuff_session::{VideoFrame, VideoStatusState};

use crate::wake;

/// Which transport a video-carrying route's stream encodes for — picked
/// by the mesh from the offer's `video` accepts (see `RouteControl::Offer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoMode {
    /// Standalone JPEG frames over the media channel — the v1 transport
    /// and the universal fallback.
    Mjpeg,
    /// H.264 access units for the mesh's RTP track lane.
    H264,
}

/// What a route's capture thread points at — the one place a display
/// route and a camera route differ. Everything downstream (encoder,
/// transport, tune/refresh, status) is shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoSource {
    /// A monitor: `None` = the primary (the synthetic `screen`
    /// capability), `Some(id)` = the monitor with that enumeration id
    /// (a `screen:<id>` capability — see [`extra_screens`]).
    Screen(Option<u32>),
    /// A camera, by the inventory device id its capability embeds
    /// (`cam:video0`) — resolved back to the OS device by
    /// [`crate::camera_capture`].
    Camera(String),
}

/// One capture tick's output, headed for the forwarder. (H.264 units
/// carry no key flag — the receiving daemon re-derives IDR-ness from the
/// NAL stream itself when it reassembles units off RTP.)
#[derive(Debug, Clone)]
pub enum VideoPacket {
    Jpeg(VideoFrame),
    H264 {
        /// Annex-B access unit.
        data: Vec<u8>,
        /// Independently decodable access unit. This stays process-local for
        /// queue recovery; the existing RTP payload remains unchanged.
        key: bool,
        /// Capture-tick pacing for the RTP clock (1/fps).
        duration_us: u64,
    },
}

/// Pipeline counters for one outbound stream, logged every
/// [`STATS_EVERY`] — the dial-in line: effective fps out, where each
/// frame's milliseconds went (scale vs encode), payload bitrate, and the
/// three ways a tick can produce nothing (unchanged screen, downstream
/// drop, rate-control skip). Times accumulate; the log shows per-frame
/// averages over the window.
pub struct StreamStats {
    route_id: String,
    label: &'static str,
    since: Instant,
    sent: u32,
    keyframes: u32,
    static_skipped: u32,
    dropped: u32,
    bytes: u64,
    scale: Duration,
    encode: Duration,
    /// Per-frame samples (ms) inside the current window, for the p95s —
    /// smoothness lives in the tail, not the average: a clean 8 ms mean
    /// with a 40 ms p95 *is* the stutter the viewer feels.
    scale_ms: Vec<f32>,
    encode_ms: Vec<f32>,
    /// M1 capture-age samples (ms): compositor present → encode start —
    /// the pixels' staleness before the encoder ever saw them (channel
    /// wait and freshest-wins displacement included). GPU lane only;
    /// empty elsewhere and the line omits the span.
    age_ms: Vec<f32>,
    out_w: u32,
    out_h: u32,
    /// The pollable per-route cell this stats object publishes its output
    /// geometry + codec into every frame (cheap atomics), so the GUI's
    /// effective-dials panel has fresh numbers independent of the 5 s log
    /// cadence. Shared with [`VideoBridge::route_dials`]; see [`route_live`].
    live: Arc<RouteLive>,
}

impl StreamStats {
    fn new(route_id: &str, mode: VideoMode) -> Self {
        StreamStats {
            route_id: route_id.to_string(),
            label: match mode {
                VideoMode::H264 => "H.264",
                VideoMode::Mjpeg => "MJPEG",
            },
            since: Instant::now(),
            sent: 0,
            keyframes: 0,
            static_skipped: 0,
            dropped: 0,
            bytes: 0,
            scale: Duration::ZERO,
            encode: Duration::ZERO,
            scale_ms: Vec::new(),
            encode_ms: Vec::new(),
            age_ms: Vec::new(),
            out_w: 0,
            out_h: 0,
            live: route_live_cell(route_id),
        }
    }

    /// Record one frame's capture age (present → encode start), M1's
    /// first span.
    #[cfg(windows)]
    fn add_age(&mut self, d: Duration) {
        self.age_ms.push(d.as_secs_f32() * 1000.0);
    }

    /// Record one conversion's cost: the total feeds the window average,
    /// the sample feeds the p95.
    fn add_scale(&mut self, d: Duration) {
        self.scale += d;
        self.scale_ms.push(d.as_secs_f32() * 1000.0);
    }

    /// Record one encode call's cost — see [`Self::add_scale`].
    fn add_encode(&mut self, d: Duration) {
        self.encode += d;
        self.encode_ms.push(d.as_secs_f32() * 1000.0);
    }

    /// Re-label the stats line after the encoder's transport settled (a
    /// route whose H.264 init fell to the MJPEG floor).
    fn set_mode(&mut self, mode: VideoMode) {
        self.label = match mode {
            VideoMode::H264 => "H.264",
            VideoMode::Mjpeg => "MJPEG",
        };
    }

    /// Publish the encoder-rung label for the effective-dials panel. The CPU
    /// pipeline names itself in [`HealingEncoder::new`]; [`run_capture`] sets
    /// the coarse "GPU (hardware)" before the GPU lane runs. The exact
    /// NVENC/MF/AMF rung lives inside `run_gpu_lane`, which this pass leaves
    /// untouched — so this stays a best-effort label, never a claim about a
    /// specific silicon path.
    #[cfg(windows)]
    fn set_encoder(&self, label: impl Into<String>) {
        *self.live.encoder.lock() = label.into();
    }

    fn p95(samples: &mut [f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        samples.sort_by(f32::total_cmp);
        samples[(samples.len() * 95 / 100).min(samples.len() - 1)]
    }

    fn maybe_log(&mut self) {
        // Publish the live output geometry + codec every frame (cheap
        // atomics) so the GUI's 1 Hz effective-dials poll always reads fresh
        // numbers, independent of the 5 s log cadence below. Both encode
        // lanes call maybe_log() once per frame.
        self.live.out_w.store(self.out_w, Ordering::Relaxed);
        self.live.out_h.store(self.out_h, Ordering::Relaxed);
        self.live.codec.store(
            match self.label {
                "H.264" => 1,
                "MJPEG" => 2,
                _ => 0,
            },
            Ordering::Relaxed,
        );
        let elapsed = self.since.elapsed();
        if elapsed < STATS_EVERY {
            return;
        }
        let secs = elapsed.as_secs_f64();
        let frames = self.sent.max(1) as f64;
        let scale_p95 = Self::p95(&mut self.scale_ms);
        let encode_p95 = Self::p95(&mut self.encode_ms);
        // The bandwidth at each sender layer, so a field log names where
        // the bits go: `raw` = the pixels entering the encoder (NV12 for
        // H.264, RGBA-equivalent for MJPEG counts the same 12 bpp — the
        // convert layer's output either way), `wire` = the encoded bytes
        // handed to the daemon's track lane (what the ICE path carries,
        // before RTP overhead).
        let raw_mbps =
            self.sent as f64 * self.out_w as f64 * self.out_h as f64 * 1.5 * 8.0 / secs / 1e6;
        // M1's first span, when the lane carries it: how old the pixels
        // already were at encode start.
        let age = if self.age_ms.is_empty() {
            String::new()
        } else {
            let avg = self.age_ms.iter().sum::<f32>() / self.age_ms.len() as f32;
            format!(" · age {avg:.1}ms (p95 {:.1})", Self::p95(&mut self.age_ms))
        };
        let line = format!(
            "video out {}: {:.1} fps {} {}×{} · raw {:.0} Mbps → wire {:.1} Mbps{age} · scale {:.1}ms (p95 {scale_p95:.1}) · encode {:.1}ms (p95 {encode_p95:.1}) · {} key · {} static-skip · {} dropped",
            self.route_id,
            self.sent as f64 / secs,
            self.label,
            self.out_w,
            self.out_h,
            raw_mbps,
            (self.bytes as f64 * 8.0) / secs / 1_000_000.0,
            self.scale.as_secs_f64() * 1000.0 / frames,
            self.encode.as_secs_f64() * 1000.0 / frames,
            self.keyframes,
            self.static_skipped,
            self.dropped,
        );
        if stats_to_info() {
            tracing::info!("{line}");
        } else {
            tracing::debug!("{line}");
        }
        self.since = Instant::now();
        self.sent = 0;
        self.keyframes = 0;
        self.static_skipped = 0;
        self.dropped = 0;
        self.bytes = 0;
        self.scale = Duration::ZERO;
        self.encode = Duration::ZERO;
        self.scale_ms.clear();
        self.encode_ms.clear();
        self.age_ms.clear();
    }
}

/// MJPEG ceiling on the longest frame edge. Defaults to **1920** so a 1080p
/// desktop streams at native HD — at 1280 a 1080p screen was downscaled to
/// 720p, too soft to read text. JPEG frames are chunked under the 64 KiB
/// data-channel ceiling, so a higher edge costs bandwidth, not correctness.
/// (MJPEG is the compatibility floor; the H.264 path carries the full
/// picture.) Override: `ALLMYSTUFF_MJPEG_MAX_EDGE`.
fn mjpeg_max_edge() -> u32 {
    static EDGE: std::sync::LazyLock<u32> =
        std::sync::LazyLock::new(|| env_u32("ALLMYSTUFF_MJPEG_MAX_EDGE", 1920).clamp(320, 3840));
    *EDGE
}
/// Mid-range JPEG quality — piKVM's default neighbourhood; text stays
/// legible, photos stay cheap.
const JPEG_QUALITY: u8 = 60;
/// An unchanged screen still re-sends one frame this often, so a viewer
/// that lost a frame (or joined a quiet stream) is never stranded on a
/// stale picture. Every tick in between costs one buffer compare.
const STATIC_REFRESH: Duration = Duration::from_secs(2);
/// Forced-IDR cadence floor (ms) — bounds how long a viewer that joined
/// mid-stream, lost an unrepaired packet, or rebuilt its decoder waits for a
/// clean decode entry. The cadence when a viewer reports trouble (or hasn't
/// reported), i.e. today's behaviour, and the default the adaptation starts
/// from. 2 s costs a fraction of the stream: cheap insurance next to a
/// multi-second freeze.
const IDR_MS_TIGHT: u64 = 2000;
/// The forced-IDR ceiling (ms) — the relaxed cadence for a viewer that's
/// keeping up cleanly. A keyframe is the costliest, most loss-exposed thing
/// on the wire (hundreds of packets, all-or-nothing); a healthy link doesn't
/// need one every 2 s, since the viewer asks for a fresh entry the instant it
/// actually glitches. Stretching cuts keyframe bursts → less loss exposure
/// and bandwidth, without slowing real recovery.
const IDR_MS_RELAXED: u64 = 8000;
/// Feedback older than this is treated as absent — a viewer that went quiet
/// (or whose link died) must not hold the cadence relaxed.
const FEEDBACK_FRESH: Duration = Duration::from_secs(6);
/// Post-quiesce quality refinement: after the convergence IDR, this many
/// spaced, **non-IDR** re-encodes of the retained picture run. The motion
/// burst that precedes a quiet spell typically leaves the last picture
/// coarsely quantized — rate control absorbed a scene change with the VBV
/// drained — and with damage-driven capture delivering nothing while the
/// user reads the new window, nothing would ever sharpen it ("the classic
/// low-bitrate transition, in slow motion"). Re-encoding identical input
/// lets the encoder spend idle bandwidth on exactly the pixels the viewer
/// is now staring at. H.264 only — an MJPEG re-encode of identical pixels
/// is byte-identical, pure waste.
const REFINE_PASSES: u8 = 2;
/// Delay from the convergence IDR to the first refinement — a beat for the
/// rate control's budget to breathe after the burst.
const REFINE_AFTER: Duration = Duration::from_millis(350);
/// Spacing between refinement passes.
const REFINE_SPACING: Duration = Duration::from_millis(800);
/// How long a degraded (screenshot-fallback) spell runs before the route
/// re-attempts a DXGI duplication session. Duplication failures are usually
/// transient — a fullscreen-exclusive app, the UAC desktop, a driver reset —
/// and without this a route stayed pinned on soft, CPU-hungry GDI grabs for
/// its whole life once it fell.
#[cfg(windows)]
const DXGI_REPROMOTE_AFTER: Duration = Duration::from_secs(30);

/// The forced-IDR interval (ms) the latest receiver feedback implies. The
/// conservative half of the adaptation: relax to [`IDR_MS_RELAXED`] only on
/// *confirmed* health (recent report, no decode failures, queue draining);
/// anything else — no feedback, stale feedback, any glitch — stays at the
/// [`IDR_MS_TIGHT`] floor, i.e. exactly today's behaviour.
fn adaptive_idr_ms(fb: Option<RecvFeedback>) -> u64 {
    match fb {
        Some(fb)
            if fb.at.elapsed() < FEEDBACK_FRESH && fb.decode_fails == 0 && fb.queue_depth <= 8 =>
        {
            IDR_MS_RELAXED
        }
        _ => IDR_MS_TIGHT,
    }
}
/// How often each stream logs its pipeline counters — the dial-in line:
/// effective fps, where the per-frame milliseconds go, and the bitrate.
const STATS_EVERY: Duration = Duration::from_secs(5);

// The performance dials, each overridable for dial-in sessions without a
// rebuild (read once per process). Defaults aim at "the screen, full
// fidelity": H.264 carries up to a native 4K frame at a rate budgeted
// from its true pixel count.

/// How the ICE-nominated path to the stream's viewer actually flows —
/// the daemon's own LAN/STUN/TURN taxonomy (`PeerInfo.selected_pair`,
/// host↔host = LAN), carried into the stream so the *automatic* dials
/// can be generous exactly where generosity is free. Explicit viewer
/// Tune fields and the env dials always win over this gate. `Unknown`
/// (ICE not settled yet, or an old daemon that doesn't report the pair)
/// is treated as WAN — start conservative, upgrade when the class lands.
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

/// Game mode (`ALLMYSTUFF_GAME_MODE=1`): the cadence-and-latency-first
/// posture for driving a game over the stream. Two automatic dials move —
/// the off-LAN fps floor rises to 60 ([`auto_fps`]) and the encoder's burst
/// headroom tightens ([`burst_bounds`]) so a scene change queues for half
/// the time. The caveat is documented honestly: the transport is still
/// open-loop (no pacer/BWE), so 60 fps over a weak WAN link is the
/// operator's deliberate trade until the transport work lands — which is
/// exactly why this is an explicit opt-in and not the default.
pub(crate) fn game_mode() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| match std::env::var("ALLMYSTUFF_GAME_MODE") {
            Ok(v) if !v.is_empty() && v != "0" => {
                tracing::info!("ALLMYSTUFF_GAME_MODE on: 60 fps floor + tight burst bounds");
                true
            }
            _ => false,
        });
    *ON
}

/// Capture cadence to aim for — a ceiling, not a promise. Session capture
/// sustains it (damage-driven backends produce less on quiet screens);
/// the one-shot fallback runs at whatever the platform's screenshot path
/// allows. Override: `ALLMYSTUFF_VIDEO_FPS`.
/// The automatic fps target with an explicit game posture — the per-route
/// dial (already OR'd with the env override by [`Tune::game`]).
pub(crate) fn target_fps_for(link: LinkClass, game: bool) -> u32 {
    static FPS: std::sync::LazyLock<Option<u32>> =
        std::sync::LazyLock::new(|| env_u32_opt("ALLMYSTUFF_VIDEO_FPS"));
    FPS.unwrap_or(auto_fps(link, game)).clamp(1, 240)
}

/// The automatic cadence: 60 on a LAN — this is a Parsec-tier 4K60 stream;
/// 30 made fast motion look choppy. It's a ceiling, not a promise
/// (damage-driven backends produce less on quiet screens). Off-LAN (or
/// before the path class is known) the default steps back to 30: the
/// transport is still open-loop (no pacer/BWE yet), and doubling the
/// cadence there buys queueing latency, not smoothness — unless
/// [`game_mode`] pins the cadence-first trade deliberately. The env dial
/// overrides everything; an explicit viewer Tune never reaches this.
fn auto_fps(link: LinkClass, game: bool) -> u32 {
    match (link, game) {
        (LinkClass::Lan, _) => 60,
        (_, true) => 60,
        _ => 30,
    }
}

/// The rate controller's burst posture: `(peak, vbv_window)` for a target
/// bitrate. The standard posture gives a fast-motion / scene-change frame
/// ~2× headroom over a ~1 s window — quality-first, and bare mean-rate CBR
/// (the setting before it) was exactly the "blocky on fast motion" symptom.
/// Game mode trims to 1.5× over ~½ s: a burst that queues for a full second
/// is *felt* as input lag mid-game, and the post-quiesce refinement passes
/// now repair what the tighter budget costs a transition. Shared by every
/// backend that exposes peak/VBV (MF on Windows, the FFmpeg vendor
/// encoders; VideoToolbox manages its own and ignores these).
#[cfg(any(windows, feature = "hwenc", test))]
pub(crate) fn burst_bounds(bitrate: u32, game: bool) -> (u32, u32) {
    if game {
        (bitrate.saturating_mul(3) / 2, bitrate / 2)
    } else {
        (bitrate.saturating_mul(2), bitrate)
    }
}

/// H.264 ceiling on the longest edge. 3840 means "native up to 4K" — no
/// downscale on anything up to a UHD monitor (openh264's own hard limit
/// is 3840×2160). Dimensions are forced even (4:2:0 chroma needs it).
/// Override: `ALLMYSTUFF_VIDEO_MAX_EDGE`.
fn h264_max_edge() -> u32 {
    static EDGE: std::sync::LazyLock<u32> =
        std::sync::LazyLock::new(|| env_u32("ALLMYSTUFF_VIDEO_MAX_EDGE", 3840).clamp(320, 3840));
    *EDGE
}

/// Target bitrate for one stream's encode, budgeted from what it actually
/// carries: ~0.16 bits per pixel per frame — the density 1080p30 was
/// tuned crisp at (10 Mbps) — clamped to 8 Mbps up to a cap the link
/// class earns. On a LAN pair the cap is 80 Mbps, so a 4K60 desktop
/// (~80 Mbps at this density) and a 3440×1440@60 ultrawide (~48 Mbps)
/// reach their budget instead of being pinned at the old 40 Mbps
/// ceiling, which was *itself* the QP wall that blocked fast motion.
/// Off-LAN (and before the class is known) the cap stays 40 Mbps: the
/// transport is open-loop, and the roadmap's own rule is to never ship
/// the raised cap WAN-wide without BWE. Explicit viewer Tune bitrates
/// bypass this entirely.
/// Override (a fixed bps for every stream): `ALLMYSTUFF_VIDEO_BITRATE`.
fn h264_bitrate_for(w: u32, h: u32, fps: u32, link: LinkClass) -> u32 {
    static OVERRIDE: std::sync::LazyLock<u32> =
        std::sync::LazyLock::new(|| env_u32("ALLMYSTUFF_VIDEO_BITRATE", 0));
    if *OVERRIDE > 0 {
        return *OVERRIDE;
    }
    let cap = match link {
        LinkClass::Lan => 80_000_000,
        LinkClass::Wan | LinkClass::Unknown => 40_000_000,
    };
    let px = u64::from(w) * u64::from(h);
    let bps = px * u64::from(fps) * 16 / 100;
    bps.clamp(8_000_000, cap) as u32
}

/// A `u32` env dial with no default — `None` when unset/unparseable, so
/// the caller can distinguish "operator pinned it" from "use the gate".
fn env_u32_opt(key: &str) -> Option<u32> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => match v.trim().parse() {
            Ok(n) => {
                tracing::info!("{key}={n} (override)");
                Some(n)
            }
            Err(_) => {
                tracing::warn!("{key}={v} isn't a number — using the automatic dial");
                None
            }
        },
        _ => None,
    }
}

/// A `u32` env dial; `default` when unset or unparseable.
fn env_u32(key: &str, default: u32) -> u32 {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => match v.trim().parse() {
            Ok(n) => {
                tracing::info!("{key}={n} (override)");
                n
            }
            Err(_) => {
                tracing::warn!("{key}={v} isn't a number — using {default}");
                default
            }
        },
        _ => default,
    }
}

/// Whether the periodic pipeline stats print at info. Off by default —
/// steady-state runs stay quiet; set `ALLMYSTUFF_VIDEO_STATS=1` while
/// dialing performance in (without it the same lines land at debug, so
/// the `ALLMYSTUFF_GUI_LOG` filter can also reach them).
pub(crate) fn stats_to_info() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var("ALLMYSTUFF_VIDEO_STATS").is_ok_and(|v| !v.is_empty() && v != "0")
    });
    *ON
}

/// One stream's viewer-requested overrides (`RouteControl::Tune`): each
/// `None` falls back to the global dial / pixel budget. Applied by
/// restarting the route's capture, so a change costs one IDR's worth of
/// hiccup, never a desynced encoder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tune {
    pub max_edge: Option<u32>,
    pub bitrate: Option<u32>,
    pub fps: Option<u32>,
    /// The viewer's per-route game-mode ask (latency-first posture: GDR
    /// on the SDK rung, 60 fps floor off-LAN). `ALLMYSTUFF_GAME_MODE=1`
    /// still forces it node-wide; this is the wire dial. Kept alongside
    /// [`Tune::mode`] for hosts/viewers that predate the tri-state.
    pub game: bool,
    /// The named posture, when the viewer speaks the tri-state (parsed
    /// at the wire boundary — [`parse_posture`]). Wins over `game`; see
    /// [`Tune::posture`]. `Option<Posture>` (Copy) so `Tune` stays the
    /// by-value dial bundle every retune path copies around.
    pub mode: Option<Posture>,
    /// How the path to this stream's viewer flows (host side fills it
    /// from the daemon's nominated-pair class). Not a viewer dial: it
    /// gates only what the AUTOMATIC fps/bitrate fall back to.
    pub link: LinkClass,
}

/// A stream's tuned character — the three-way dial that replaced the
/// old quality slider. Balanced is the stability/quality default; Game
/// trades for latency and instant recovery (GDR, tight VBV, 1 ms
/// pacing); Studio trades bandwidth for fidelity on links that have it
/// (the LAN "full pipe" mode — high-bitrate quality-first encoding
/// today; 4:4:4 chroma and lossless land with the hardware-decode
/// viewer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posture {
    Balanced,
    Game,
    Studio,
    /// Studio's top shelf: mathematically lossless HEVC (constQP-0,
    /// transquant bypass) on the NVENC rung, decoded by the viewer's
    /// NVDEC lane. No rate control exists in this posture — bandwidth is
    /// content entropy (measured: ~0.1 Mbps idle · 65–95 Mbps desktop ·
    /// Gbps-class on noise, which the in-lane guard catches). Falls soft
    /// to lossy Studio wherever the HEVC rung can't open.
    StudioLossless,
}

/// The wire name → posture parse, at the tune boundary. Unknown names
/// (a future mode this build predates) read as "no named ask" so the
/// legacy `game` bool still steers.
pub fn parse_posture(s: &str) -> Option<Posture> {
    match s {
        "game" => Some(Posture::Game),
        "studio" => Some(Posture::Studio),
        "studio-lossless" => Some(Posture::StudioLossless),
        "balanced" => Some(Posture::Balanced),
        _ => None,
    }
}

impl Tune {
    /// The route's posture: the named wire dial when present, else the
    /// legacy `game` bool, else the node-wide env override. Studio is
    /// LAN-gated — asked for off-LAN it degrades to Balanced (a 200 Mbps
    /// stream on a WAN path would be self-harm), logged at the use site.
    pub(crate) fn posture(&self) -> Posture {
        let p = self.mode.unwrap_or(if self.game {
            Posture::Game
        } else {
            Posture::Balanced
        });
        // Studio is NOT LAN-gated: the viewer's warning dialog is the
        // guardrail and the user owns the trade — they asked to be able to
        // slam the full pipe wherever they want it. The env override still
        // promotes a bare Balanced to Game node-wide.
        match p {
            Posture::Balanced if game_mode() => Posture::Game,
            p => p,
        }
    }

    /// The route's effective game posture (see [`Tune::posture`]).
    pub(crate) fn game(&self) -> bool {
        self.posture() == Posture::Game
    }

    /// Studio fidelity active.
    #[allow(dead_code)]
    pub(crate) fn studio(&self) -> bool {
        self.posture() == Posture::Studio
    }

    fn fps(&self) -> u32 {
        self.fps
            .unwrap_or_else(|| target_fps_for(self.link, self.game()))
            .clamp(1, 240)
    }
    fn h264_edge(&self) -> u32 {
        self.max_edge.unwrap_or_else(h264_max_edge).clamp(320, 3840)
    }
    /// MJPEG honours the Res control the same way H.264 does — up to a 4K
    /// hard cap (chunked under the 64 KiB data channel) — so the pill moves
    /// both encodings. Untuned it defaults to HD ([`mjpeg_max_edge`], 1920)
    /// rather than 4K, since a JPEG frame is far heavier than an H.264 one.
    fn mjpeg_edge(&self) -> u32 {
        self.max_edge
            .unwrap_or_else(mjpeg_max_edge)
            .clamp(320, 3840)
    }
    /// MJPEG has no bitrate, so the Rate control maps to JPEG quality — the
    /// same pill means something on both encodings. `None` (auto) keeps the
    /// neutral [`JPEG_QUALITY`] default.
    fn jpeg_quality(&self) -> u8 {
        self.bitrate.map_or(JPEG_QUALITY, mjpeg_quality_for)
    }
}

/// Map a target bitrate (the console's Rate pill: 4–40 Mbps) to a JPEG
/// quality. Higher rate → crisper frames; the curve spans the pill range so
/// "Speed" reads softer and "Quality" reads sharp.
fn mjpeg_quality_for(bps: u32) -> u8 {
    if bps <= 5_000_000 {
        45
    } else if bps <= 10_000_000 {
        55
    } else if bps <= 18_000_000 {
        65
    } else if bps <= 30_000_000 {
        78
    } else {
        88
    }
}

/// The host side of a capture-status report: state + optional OS error
/// text, forwarded to the viewer as a `vstat` media frame by the mesh.
pub type OnStatus = Arc<dyn Fn(VideoStatusState, Option<String>) + Send + Sync>;

/// Dedupe wrapper over [`OnStatus`]: only state *transitions* hit the
/// wire, so a failing grab loop costs one frame, not one per tick.
struct StatusReporter {
    cb: OnStatus,
    last: Option<VideoStatusState>,
}

impl StatusReporter {
    fn new(cb: OnStatus) -> Self {
        StatusReporter { cb, last: None }
    }

    fn report(&mut self, state: VideoStatusState, detail: Option<String>) {
        if self.last == Some(state) {
            return;
        }
        self.last = Some(state);
        // The injector pulses the display awake on inbound clicks only
        // while a stream is dark — this transition is what tells it.
        wake::set_stream_dark(!matches!(state, VideoStatusState::Ok));
        (self.cb)(state, detail);
    }
}

struct RouteVideo {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Everything needed to restart this capture on a retune.
    mode: VideoMode,
    source: VideoSource,
    on_packet: Arc<dyn Fn(VideoPacket) -> bool + Send + Sync>,
    on_status: OnStatus,
    /// One-shot "give the viewer a clean entry now" flag the encoder
    /// consumes (IDR for H.264, an immediate resend for MJPEG).
    refresh: Arc<AtomicBool>,
    /// The H.264 forced-IDR interval (ms), adapted from receiver feedback
    /// ([`note_feedback`] → [`adaptive_idr_ms`]); the encode thread reads it
    /// each frame. Default [`IDR_MS_TIGHT`] = today's fixed cadence.
    idr_ms: Arc<AtomicU64>,
    /// The tune this capture was started with — the controller compares the
    /// viewer's reported fps against its fps target.
    tune: Tune,
    /// The receiver-driven resolution cap (see [`AutoAdapt`]); fresh per
    /// capture, so a manual retune resets it.
    auto: Arc<AutoAdapt>,
}

impl Drop for RouteVideo {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The latest decode health a viewer reported for one of our outbound
/// streams (receiver → sender). Observability today; the signal the stream
/// adaptation — recovery cadence, then bitrate/res auto-scaling — reads next.
#[derive(Debug, Clone, Copy)]
pub struct RecvFeedback {
    pub recv_fps: u32,
    pub decode_fails: u32,
    pub queue_depth: u32,
    /// The viewer's chunk-train bottleneck estimate (kbps; 0 = none yet) —
    /// dispersion of the pacer's own timed bursts, measured at arrival.
    pub est_kbps: u32,
    /// One-way-delay trend (µs of added delay per second, signed): a
    /// sustained positive slope is a standing queue growing *before* loss
    /// says so. 0 = flat/unknown.
    pub delay_trend_us_per_s: i32,
    pub at: Instant,
}

/// The video pipeline's OWN feedback payload — the typed shape of the
/// opaque `ext` bag on `RouteControl::VideoFeedback`. This is the seam
/// that keeps pipeline tuning backend-only: the protocol/session crates
/// carry `ext` as an unexamined `serde_json::Value`, and every
/// receiver-side signal the closed loop needs is a field HERE, added
/// without ever touching a wire crate or the GUI. `#[serde(default)]`
/// throughout so an older/leaner peer's absent keys read as zero.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct PipelineFeedback {
    /// The viewer's chunk-train bandwidth estimate (kbps; 0 = none yet).
    #[serde(default)]
    pub est_kbps: u32,
    /// One-way-delay trend (µs of added delay per second, signed).
    #[serde(default)]
    pub delay_trend_us_per_s: i32,
    // Future receiver-side signals (last-good ts for LTR, per-plane loss,
    // paint-interval jitter, …) land here — backend-only, no wire change.
}

impl PipelineFeedback {
    /// Pack into the opaque wire `ext`; `Null` when nothing is set, so an
    /// all-default report adds zero bytes and reads identically to a peer
    /// that sends no ext at all.
    pub fn to_ext(&self) -> serde_json::Value {
        if self.est_kbps == 0 && self.delay_trend_us_per_s == 0 {
            return serde_json::Value::Null;
        }
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    /// Parse from the opaque wire `ext`; unknown/absent shapes yield the
    /// all-zero default (a peer that never learned this shape).
    pub fn from_ext(ext: &serde_json::Value) -> Self {
        serde_json::from_value(ext.clone()).unwrap_or_default()
    }
}

// ---- receiver-driven auto-adaptation ---------------------------------
//
// The viewer reports its decode health every couple of seconds
// (`RouteControl::VideoFeedback`); [`adaptive_idr_ms`] already reads it for
// the recovery cadence. This is the other half that was marked "auto-scaling
// later": when the viewer demonstrably can't keep up — a 60 fps encode
// arriving at 0–6 fps, the exact "console shows a slideshow" failure — the
// sender steps its encode **resolution** down one rung, and steps back up
// once the viewer has been healthy for a sustained stretch (fast down, slow
// up, so the picture never oscillates).
//
// Resolution is the one dial that needs no capture restart: the encode path
// fits every frame to the effective edge per frame, and a changed fit
// re-inits the encoder through the existing budget-size rebuild (bitrate
// re-budgets from the smaller frame automatically; the next unit out is an
// IDR). A manual retune replaces the route and so resets the cap — the
// viewer's new picks change the conditions, and the controller re-learns.

/// Master switch for the receiver-driven resolution auto-adaptation:
/// **OFF by default** — set `ALLMYSTUFF_AUTO_ADAPT=1` to opt in. The stream
/// runs at its full requested resolution and never silently steps itself
/// down: the deal is native quality, and quality is the user's to pick (the
/// Speed↔Quality slider / Res pills), not the stream's to quietly lower. The
/// real cause of the "standing behind" feed was the viewer-side canvas
/// demotion (fixed) and a too-aggressive decode-queue valve (removed), not
/// the absence of this lever — so it earns its keep only as an explicit
/// opt-in, never as a default that can soften a healthy 4K60. The
/// [`AutoAdapt`] logic below stays intact and unit-tested for when it's
/// turned on. The adaptive **IDR cadence** ([`adaptive_idr_ms`]) is a
/// separate, benign recovery lever and stays on regardless.
fn auto_adapt_enabled() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| env_u32("ALLMYSTUFF_AUTO_ADAPT", 0) != 0);
    *ON
}

/// The auto-cap rungs, descending. `0` (uncapped) sits above the first.
const AUTO_EDGES: &[u32] = &[2560, 1920, 1280, 960];
/// Consecutive struggling reports (~2 s apart) before a step down.
const AUTO_BAD_STREAK: u32 = 3;
/// Consecutive healthy reports before a step back up — deliberately long:
/// stepping up too eagerly re-breaks the viewer and oscillates.
const AUTO_GOOD_STREAK: u32 = 20;
/// Settle time after any step before another *down* step — the encoder
/// rebuild and the viewer's pipeline refill need a beat to show up in the
/// feedback before it's evidence about the new rung.
const AUTO_DOWN_HOLD: Duration = Duration::from_secs(8);
/// Hold after any step before a step *up*.
const AUTO_UP_HOLD: Duration = Duration::from_secs(30);

/// One route's receiver-driven cap on the encode edge, shared between the
/// feedback path (writer, via [`AutoAdapt::observe`]) and the encode thread
/// (reader, via [`AutoAdapt::edge_cap`] each frame).
pub(crate) struct AutoAdapt {
    /// The current cap on the longest encode edge; `0` = uncapped.
    edge: AtomicU32,
    state: Mutex<AdaptState>,
}

#[derive(Default)]
struct AdaptState {
    bad: u32,
    good: u32,
    last_step: Option<Instant>,
}

impl AutoAdapt {
    fn new() -> Arc<Self> {
        Arc::new(AutoAdapt {
            edge: AtomicU32::new(0),
            state: Mutex::new(AdaptState::default()),
        })
    }

    /// The cap the encode path applies (min with the tuned edge), if any.
    fn edge_cap(&self) -> Option<u32> {
        match self.edge.load(Ordering::Relaxed) {
            0 => None,
            e => Some(e),
        }
    }

    /// Fold one feedback report in; returns `Some((from, to))` when the cap
    /// stepped (0 = uncapped), for the caller to log. `now` is passed in so
    /// the streak/hold logic is unit-testable.
    fn observe(&self, fb: &RecvFeedback, fps_target: u32, now: Instant) -> Option<(u32, u32)> {
        // Struggling: arriving at under a quarter of the encode rate (the
        // field failure was 0–6 fps of 60), or a queue backing far up.
        // Healthy: at least three quarters of it, decoding cleanly, queue
        // drained. The band between counts as neither — streaks reset, no
        // step. Decode failures alone are corruption, not overload — the
        // adaptive IDR cadence owns those.
        // `recv_fps` is a raw peer-controlled u32 off the feedback wire;
        // `saturating_mul` keeps a hostile value from wrapping (and from
        // aborting outright if overflow-checks are ever enabled).
        let bad = fb.recv_fps.saturating_mul(4) < fps_target || fb.queue_depth > 24;
        let good = fb.recv_fps.saturating_mul(4) >= fps_target.saturating_mul(3)
            && fb.decode_fails == 0
            && fb.queue_depth <= 8;
        let mut st = self.state.lock();
        if bad {
            st.bad += 1;
            st.good = 0;
        } else if good {
            st.good += 1;
            st.bad = 0;
        } else {
            st.bad = 0;
            st.good = 0;
        }
        let cur = self.edge.load(Ordering::Relaxed);
        let held_for = |hold: Duration, st: &AdaptState| {
            st.last_step.is_none_or(|t| now.duration_since(t) >= hold)
        };
        if st.bad >= AUTO_BAD_STREAK && held_for(AUTO_DOWN_HOLD, &st) {
            // Next rung strictly below the current cap (uncapped → first).
            let Some(next) = AUTO_EDGES.iter().copied().find(|&e| cur == 0 || e < cur) else {
                st.bad = 0; // already at the floor — nothing left to give
                return None;
            };
            self.edge.store(next, Ordering::Relaxed);
            *st = AdaptState {
                last_step: Some(now),
                ..AdaptState::default()
            };
            return Some((cur, next));
        }
        if st.good >= AUTO_GOOD_STREAK && cur != 0 && held_for(AUTO_UP_HOLD, &st) {
            let next = match AUTO_EDGES.iter().position(|&e| e == cur) {
                Some(0) | None => 0,
                Some(i) => AUTO_EDGES[i - 1],
            };
            self.edge.store(next, Ordering::Relaxed);
            *st = AdaptState {
                last_step: Some(now),
                ..AdaptState::default()
            };
            return Some((cur, next));
        }
        None
    }
}

#[derive(Default)]
pub struct VideoBridge {
    routes: Mutex<HashMap<String, RouteVideo>>,
    /// Per-route receiver health, keyed by route id (see [`RecvFeedback`]).
    feedback: Mutex<HashMap<String, RecvFeedback>>,
    /// Recent loss-report instants per route — the density signal the GDR
    /// wave chooser reads (a lossy spell heals with a short, fat wave; a
    /// one-off keeps the smooth default).
    loss_marks: Mutex<HashMap<String, std::collections::VecDeque<Instant>>>,
}

impl VideoBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin streaming `source` for `route_id`, encoding for `mode` — a
    /// monitor for a display route, a camera for a video one. `on_packet`
    /// is called with each encoded packet; it returns `false` when the
    /// packet was dropped downstream (backpressure), which is fine — the
    /// next capture simply carries the newer picture. `on_status` is
    /// called on capture-state transitions, for the viewer's benefit (see
    /// [`StatusReporter`]).
    pub fn start_capture<F, S>(
        &self,
        route_id: String,
        mode: VideoMode,
        source: VideoSource,
        tune: Tune,
        on_packet: F,
        on_status: S,
    ) where
        F: Fn(VideoPacket) -> bool + Send + Sync + 'static,
        S: Fn(VideoStatusState, Option<String>) + Send + Sync + 'static,
    {
        // `Effect::StartMedia` is deduplicated by the session state machine,
        // so reaching this boundary means a real incarnation. A same-id
        // predecessor can remain in the registry after its worker exited (or
        // while a stale StopMedia is being ignored); stop and join it before
        // the successor captures so callbacks and monitor sessions never
        // overlap.
        let displaced = self.take_existing_capture(&route_id);
        if displaced.is_some() {
            tracing::warn!("replacing prior video capture worker for same-id route {route_id}");
        }
        drop(displaced);
        self.spawn_route(
            route_id,
            mode,
            source,
            tune,
            Arc::new(on_packet),
            Arc::new(on_status),
        );
    }

    fn take_existing_capture(&self, route_id: &str) -> Option<RouteVideo> {
        self.routes.lock().remove(route_id)
    }

    /// The route ids with a live capture pump — for the mesh to sweep
    /// when a peer's link class changes.
    pub fn route_ids(&self) -> Vec<String> {
        self.routes.lock().keys().cloned().collect()
    }

    /// A viewer's Tune (the console pills/slider): the dials change, the
    /// link class the gate learned stays — a viewer retune must not
    /// quietly reset a LAN stream to the conservative Unknown dials.
    pub fn retune_dials(
        &self,
        route_id: &str,
        max_edge: Option<u32>,
        bitrate: Option<u32>,
        fps: Option<u32>,
        game: bool,
        mode: Option<&str>,
    ) {
        let link = self
            .routes
            .lock()
            .get(route_id)
            .map(|r| r.tune.link)
            .unwrap_or_default();
        self.retune(
            route_id,
            Tune {
                max_edge,
                bitrate,
                fps,
                link,
                game,
                mode: mode.and_then(parse_posture),
            },
        );
    }

    /// Re-class a live route's link (the LAN gate learning the truth after
    /// the stream started): respawns the capture only when the class
    /// actually changed AND no viewer dial pins the affected knobs — a
    /// restart costs one IDR hiccup, so a no-op reclassification must cost
    /// nothing. Returns whether a retune happened.
    pub fn retune_link(&self, route_id: &str, link: LinkClass) -> bool {
        let current = { self.routes.lock().get(route_id).map(|r| r.tune) };
        let Some(t) = current else { return false };
        if t.link == link {
            return false;
        }
        // With BOTH automatic dials pinned by the viewer, the class change
        // can't alter the stream — skip the restart entirely.
        if t.fps.is_some() && t.bitrate.is_some() {
            return false;
        }
        // Instrumented: a capped stream's whole story is often this line — a
        // CEC dial that never leaves Unknown/Wan stays at the 30fps cap, while
        // a direct LAN dial flips to Lan and unlocks 60. Posted logs then show
        // exactly which happened on each path.
        let new_t = Tune { link, ..t };
        tracing::info!(
            "video relink {route_id}: {:?} → {:?} · fps cap {} → {}",
            t.link,
            link,
            t.fps(),
            new_t.fps(),
        );
        self.retune(route_id, new_t);
        true
    }

    fn spawn_route(
        &self,
        route_id: String,
        mode: VideoMode,
        source: VideoSource,
        tune: Tune,
        on_packet: Arc<dyn Fn(VideoPacket) -> bool + Send + Sync>,
        on_status: OnStatus,
    ) {
        let stop = Arc::new(AtomicBool::new(false));
        let refresh = Arc::new(AtomicBool::new(false));
        let idr_ms = Arc::new(AtomicU64::new(IDR_MS_TIGHT));
        let auto = AutoAdapt::new();
        let (stop_thread, refresh_thread, idr_thread, auto_thread, cb) = (
            stop.clone(),
            refresh.clone(),
            idr_ms.clone(),
            auto.clone(),
            on_packet.clone(),
        );
        let status_cb = on_status.clone();
        let id = route_id.clone();
        let src = source.clone();
        let thread = std::thread::spawn(move || {
            // This thread exists for the moments the machine is loaded, and
            // its sleeps pace a 60 fps budget — hold the 1 ms timer quantum
            // and boost the thread (priority, EcoQoS opt-out, P-core
            // preference) for the stream's lifetime.
            let _timer = crate::os_perf::TimerResolutionGuard::hold();
            crate::os_perf::boost_media_thread();
            let what = match &src {
                VideoSource::Screen(_) => "screen",
                VideoSource::Camera(_) => "camera",
            };
            if let Err(e) = run_capture(
                &stop_thread,
                &refresh_thread,
                &idr_thread,
                &auto_thread,
                &id,
                mode,
                &src,
                tune,
                cb,
                status_cb,
            ) {
                tracing::warn!("{what} capture for {id} stopped: {e}");
            }
        });
        let displaced = {
            let mut routes = self.routes.lock();
            routes.insert(
                route_id,
                RouteVideo {
                    stop,
                    thread: Some(thread),
                    mode,
                    source,
                    on_packet,
                    on_status,
                    refresh,
                    idr_ms,
                    tune,
                    auto,
                },
            )
        };
        // Join any concurrently displaced capture thread (RouteVideo::drop) only after the
        // routes lock is released — joining a thread under the lock would
        // block every other route op, and on the async start path stall a
        // tokio worker. `start_capture` normally removed its predecessor
        // before spawning; the explicit drop also makes two racing starts safe.
        drop(displaced);
    }

    /// Ask `route_id`'s encoder for a clean decode entry on its next
    /// frame — the viewer's decoder lost its place. No-op for a route
    /// this machine isn't streaming.
    /// Whether `route_id` is currently in the game posture (viewer dial
    /// or env override) — the mesh forwarder reads this per access unit
    /// to pick the pacing quanta. Unknown route = balanced.
    pub fn route_game(&self, route_id: &str) -> bool {
        self.routes
            .lock()
            .get(route_id)
            .map(|r| r.tune.game())
            .unwrap_or(false)
    }

    /// The effective encode dials for `route_id` when THIS node is its
    /// streamer — the "what we're actually doing" half of the console's
    /// quality panel: the resolved posture, the encoder rung, the codec on
    /// the wire, the AIMD bitrate target + its posture ceiling, the fps and
    /// edge targets, and the actual output geometry. `None` when the route
    /// isn't encoded here (the ordinary remote-view case — the viewer shows
    /// its own measured actuals instead). Read-only and lock-cheap: a brief
    /// lock on each of the three per-route maps, no wire traffic. Poll ~1 Hz
    /// while a panel is open.
    pub fn route_dials(&self, route_id: &str) -> Option<RouteDials> {
        let (tune, edge_cap) = {
            let routes = self.routes.lock();
            let r = routes.get(route_id)?;
            (r.tune, effective_h264_edge(r.tune, &r.auto))
        };
        let posture = match tune.posture() {
            Posture::Balanced => "balanced",
            Posture::Game => "game",
            Posture::Studio => "studio",
            Posture::StudioLossless => "studio-lossless",
        };
        // The closed-loop bitrate the encoder is actually aiming at right now
        // (AIMD moves it under Game), and the posture budget it climbs back
        // toward. Absent until the encode lane registers its rate cell.
        let (target_bitrate_bps, ceiling_bps) = route_rates()
            .lock()
            .get(route_id)
            .map(|rr| {
                (
                    rr.target.load(Ordering::Relaxed),
                    rr.ceiling.load(Ordering::Relaxed),
                )
            })
            .unwrap_or((0, 0));
        let (out_w, out_h, codec, encoder_label) = route_live()
            .lock()
            .get(route_id)
            .map(|l| {
                (
                    l.out_w.load(Ordering::Relaxed),
                    l.out_h.load(Ordering::Relaxed),
                    match l.codec.load(Ordering::Relaxed) {
                        1 => "H.264",
                        2 => "MJPEG",
                        _ => "",
                    },
                    l.encoder.lock().clone(),
                )
            })
            .unwrap_or((0, 0, "", String::new()));
        Some(RouteDials {
            posture,
            encoder_label,
            codec,
            target_bitrate_bps,
            ceiling_bps,
            fps_target: tune.fps(),
            edge_cap,
            out_w,
            out_h,
        })
    }

    pub fn force_idr(&self, route_id: &str) {
        if let Some(r) = self.routes.lock().get(route_id) {
            r.refresh.store(true, Ordering::SeqCst);
            tracing::debug!("refresh requested for {route_id}");
        }
    }

    /// Frame health's targeted heal: a GDR lane restarts its refresh
    /// wave — spread intra, no keyframe wall, no smear left behind — and
    /// any route without a registered wave falls back to the IDR refresh.
    /// The wave's LENGTH is chosen from loss density: a second report
    /// within ten seconds says the link is in a lossy spell, so the heal
    /// shortens to 3 frames (~50 ms artifact window, fatter per-frame
    /// intra the single-frame VBV smooths); a one-off keeps the smooth
    /// default. A store over an in-flight request restarts it — a second
    /// loss mid-wave must re-heal, not be absorbed silently.
    pub fn route_wave_or_refresh(&self, route_id: &str) {
        let now = Instant::now();
        let lossy_spell = {
            let mut marks = self.loss_marks.lock();
            let q = marks.entry(route_id.to_string()).or_default();
            q.push_back(now);
            while q
                .front()
                .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(10))
            {
                q.pop_front();
            }
            q.len() >= 2
        };
        if let Some(flag) = wave_flags().lock().get(route_id) {
            let fps = self
                .routes
                .lock()
                .get(route_id)
                .map(|r| r.tune.fps())
                .unwrap_or(60);
            let frames = if lossy_spell {
                3
            } else {
                default_wave_frames(fps)
            };
            flag.store(frames, Ordering::SeqCst);
            tracing::debug!(
                "wave restart requested for {route_id} ({frames} frames{})",
                if lossy_spell { ", lossy spell" } else { "" }
            );
            return;
        }
        self.force_idr(route_id);
    }

    /// What the mesh forwarder needs to pace this route's bursts: the
    /// posture (game trims the budget), whether the nominated path is
    /// WAN-class (Unknown counts — conservative until ICE says LAN), and
    /// the current send rate in bps (0 = no live encode lane; the pacer
    /// keeps its LAN constants).
    pub fn route_pace(&self, route_id: &str) -> (bool, bool, u32, u32) {
        let (game, wan, fps) = self
            .routes
            .lock()
            .get(route_id)
            .map(|r| (r.tune.game(), r.tune.link != LinkClass::Lan, r.tune.fps()))
            .unwrap_or((false, false, 60));
        let rate = route_rates()
            .lock()
            .get(route_id)
            .map(|r| r.target.load(Ordering::Relaxed))
            .unwrap_or(0);
        (game, wan, rate, fps)
    }

    /// Record the decode health a viewer reported for one of our streams
    /// (receiver → sender). Logged at info when the link looks unhealthy
    /// (decode failures, or the queue backing up) so a struggling stream is
    /// visible from the *sender's* logs, and stored for the stream
    /// adaptation to read.
    pub fn note_feedback(
        &self,
        route_id: &str,
        recv_fps: u32,
        decode_fails: u32,
        queue_depth: u32,
        est_kbps: u32,
        delay_trend_us_per_s: i32,
    ) {
        let fb = RecvFeedback {
            recv_fps,
            decode_fails,
            queue_depth,
            est_kbps,
            delay_trend_us_per_s,
            at: Instant::now(),
        };
        self.feedback.lock().insert(route_id.to_string(), fb);
        if decode_fails > 0 || queue_depth > 8 {
            tracing::info!(
                "video feedback {route_id}: viewer {recv_fps} fps · {decode_fails} decode-fail · queue {queue_depth}{}",
                if est_kbps > 0 {
                    format!(" · est {:.1} Mbps", est_kbps as f64 / 1000.0)
                } else {
                    String::new()
                }
            );
        } else {
            tracing::debug!(
                "video feedback {route_id}: viewer {recv_fps} fps · queue {queue_depth}"
            );
        }
        // Closed-loop bitrate: AIMD against the posture lane's budget,
        // applied by the encode thread through the in-place reconfigure
        // (no reset, no IDR, no visible seam). Reserved to the postures
        // whose use case it serves in every case — Game by default (see
        // [`rate_adapt_mode`]). Skips besides the gate: user-pinned
        // bitrates (their wire to own), lossless (no rate to move), and
        // routes with no live rate registration (CPU lane, MJPEG floor).
        {
            let (pinned, target_fps, game) = {
                let routes = self.routes.lock();
                routes
                    .get(route_id)
                    .map(|r| (r.tune.bitrate.is_some(), r.tune.fps(), r.tune.game()))
                    .unwrap_or((true, 0, false))
            };
            if !pinned && rate_adapt_allowed(game, rate_adapt_mode()) {
                if let Some(rate) = route_rates().lock().get(route_id).cloned() {
                    let current = rate.target.load(Ordering::Relaxed);
                    let ceiling = rate.ceiling.load(Ordering::Relaxed);
                    if current > 0 && ceiling > 0 {
                        let step = rate_adapt_step(
                            &mut rate.adapt.lock(),
                            &fb,
                            target_fps,
                            current,
                            ceiling,
                            Instant::now(),
                        );
                        if let Some(next) = step {
                            rate.target.store(next, Ordering::Relaxed);
                            tracing::info!(
                                "video rate {route_id}: {:.1} → {:.1} Mbps (ceiling {:.1}) — {}",
                                current as f64 / 1e6,
                                next as f64 / 1e6,
                                ceiling as f64 / 1e6,
                                if next < current {
                                    "congestion evidence"
                                } else {
                                    "sustained health"
                                }
                            );
                        }
                    }
                }
            }
        }
        // Adapt the H.264 forced-IDR cadence for this route: relax it when the
        // viewer is keeping up cleanly, tighten it the moment it isn't. The
        // encode thread reads the new value on its next frame.
        let want = adaptive_idr_ms(Some(fb));
        let adapt = {
            let routes = self.routes.lock();
            let Some(r) = routes.get(route_id) else {
                return;
            };
            let was = r.idr_ms.swap(want, Ordering::Relaxed);
            if was != want {
                tracing::debug!("video {route_id}: forced-IDR cadence {was}ms → {want}ms");
            }
            (r.auto.clone(), r.tune)
        };
        // The auto-scale half: step the encode resolution down when the
        // viewer demonstrably can't keep up, back up when it recovers
        // (see [`AutoAdapt`]). Run outside the routes lock — the observe
        // takes its own.
        let (auto, tune) = adapt;
        // Auto-scale governs only the AUTOMATIC Res dial: a viewer that
        // pinned `max_edge` (the slider / Res pill) said exactly what it
        // wants, so the controller stands down for that stream instead of
        // re-tuning under the user's hands — that fight is why this valve
        // was once disabled outright (and the console's standing decode
        // backlog is what disabling it cost). `auto_adapt_enabled` is the
        // operator kill switch on top.
        if !auto_adapt_enabled() || tune.max_edge.is_some() {
            return;
        }
        if let Some((from, to)) = auto.observe(&fb, tune.fps(), Instant::now()) {
            let name = |e: u32| {
                if e == 0 {
                    "native".to_string()
                } else {
                    format!("≤{e}")
                }
            };
            tracing::info!(
                "video auto-adapt {route_id}: viewer at {recv_fps} fps of {} — encode edge {} → {}                  (bitrate re-budgets at the new size; next unit is an IDR)",
                tune.fps(),
                name(from),
                name(to),
            );
        }
    }

    /// The most recent feedback a viewer reported for `route_id`, if any —
    /// the hook the stream adaptation (recovery cadence, auto-scale) reads.
    pub fn latest_feedback(&self, route_id: &str) -> Option<RecvFeedback> {
        self.feedback.lock().get(route_id).copied()
    }

    /// Restart `route_id`'s capture with the viewer's quality picks.
    pub fn retune(&self, route_id: &str, tune: Tune) {
        // A duplicate of the current dials must not pay a capture restart —
        // that's a thread teardown+join, a fresh encoder ladder, and an IDR
        // hiccup. Slider/pill UIs re-send their value on release, and the
        // session layer applies no debounce, so the dedupe lives here.
        if self
            .routes
            .lock()
            .get(route_id)
            .is_some_and(|r| r.tune == tune)
        {
            tracing::debug!("route {route_id} retune is a no-op (same dials); keeping the capture");
            return;
        }
        let Some(old) = self.routes.lock().remove(route_id) else {
            return;
        };
        let (mode, source, on_packet, on_status) = (
            old.mode,
            old.source.clone(),
            old.on_packet.clone(),
            old.on_status.clone(),
        );
        drop(old); // joins the old capture thread; its session releases
        let posture = match tune.posture() {
            Posture::Balanced => "balanced",
            Posture::Game => "game",
            Posture::Studio => "studio",
            Posture::StudioLossless => "studio-lossless",
        };
        tracing::info!(
            "route {route_id} retuned: posture {posture} · edge {} · bitrate {} · fps {}",
            tune.max_edge.map_or("auto".into(), |v| v.to_string()),
            tune.bitrate
                .map_or("auto".into(), |v| format!("{:.1} Mbps", v as f64 / 1e6)),
            tune.fps.map_or("auto".into(), |v| v.to_string()),
        );
        self.spawn_route(
            route_id.to_string(),
            mode,
            source,
            tune,
            on_packet,
            on_status,
        );
    }

    pub fn stop(&self, route_id: &str) {
        // Bind the removed route so its Drop (the capture-thread join) runs
        // after the routes lock guard is released, never under it — an
        // unbound `remove(..);` would drop the RouteVideo (and join) while the
        // guard is still held (temporary drop order), blocking the lock on a
        // thread join.
        let removed = self.routes.lock().remove(route_id);
        self.feedback.lock().remove(route_id);
        // Reap the pollable live cell too, so a stopped route stops
        // reporting stale dials to the effective-reality panel.
        route_live().lock().remove(route_id);
        drop(removed);
    }
}

#[allow(clippy::too_many_arguments)]
fn run_capture(
    stop: &AtomicBool,
    refresh: &Arc<AtomicBool>,
    idr_ms: &Arc<AtomicU64>,
    auto: &Arc<AutoAdapt>,
    route_id: &str,
    mode: VideoMode,
    source: &VideoSource,
    tune: Tune,
    on_packet: Arc<dyn Fn(VideoPacket) -> bool + Send + Sync>,
    on_status: OnStatus,
) -> Result<(), String> {
    let on_packet = &*on_packet;
    let mut reporter = StatusReporter::new(on_status);

    // A camera source skips the whole monitor story: open the device,
    // pump its frames into the same encoder. Open failures are told to
    // the viewer in-band, like every capture condition — "no camera"
    // when there's nothing to open, "camera failed" (with the OS text)
    // when there is but it won't stream: held by another app, or a
    // permission denial.
    if let VideoSource::Camera(device) = source {
        // A hosted camera is active use the OS can't see — hold the
        // machine awake like a hosted screen (without forcing the panel
        // on; the camera doesn't need the display lit).
        let _awake = wake::DisplayAwake::hold("hosting a camera stream");
        let fps = tune.fps();
        let mut encoder = HealingEncoder::new(route_id, mode, (0, 0), tune, refresh, idr_ms, auto)?;
        let mut stats = StreamStats::new(route_id, encoder.mode());
        tracing::info!(
            "video stream start {route_id}: {} · link {:?} · fps cap {fps} · camera",
            match mode {
                VideoMode::H264 => "H.264",
                VideoMode::Mjpeg => "MJPEG",
            },
            tune.link,
        );
        let (session, frames) = match crate::camera_capture::open(device, fps) {
            Ok(open) => open,
            Err(e) => {
                let state = if e.contains("no camera") {
                    VideoStatusState::NoCamera
                } else {
                    VideoStatusState::CameraFailed
                };
                reporter.report(state, Some(e.clone()));
                return Err(e);
            }
        };
        let result = pump_frames_with_stall(
            stop,
            fps,
            &frames,
            |f: crate::camera_capture::RawFrame| (f.rgba, f.width, f.height),
            on_packet,
            &mut encoder,
            &mut stats,
            &mut reporter,
            VideoStatusState::CameraFailed,
            None,
            None,
        );
        drop(session);
        // A camera whose stream died mid-route (unplugged) ends with the
        // viewer told why, not just a frozen last frame.
        if let Err(e) = &result {
            if !stop.load(Ordering::SeqCst) {
                reporter.report(VideoStatusState::CameraFailed, Some(e.clone()));
            }
        }
        return result;
    }
    let monitor_id = match source {
        VideoSource::Screen(id) => *id,
        VideoSource::Camera(_) => unreachable!("camera handled above"),
    };

    // A hosted screen is active use the OS can't see: hold the display
    // awake for the stream's lifetime, and force one that's already dark
    // back on — neither capture path can grab from a sleeping panel.
    let _awake = wake::DisplayAwake::hold("hosting a screen stream");
    wake::force_display_on();
    // The monitor up front: its resolution budgets the encoder's bitrate.
    // One retry after a beat: the wake may still be re-attaching outputs
    // (a deep-sleeping DisplayPort monitor detaches from the desktop
    // entirely, and re-enumeration after wake takes a moment).
    let monitor = match select_monitor(monitor_id) {
        Ok(m) => m,
        Err(first) => {
            std::thread::sleep(Duration::from_millis(1500));
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            match select_monitor(monitor_id) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(
                        "no monitor to capture for {route_id}: {e} (first attempt: {first})"
                    );
                    reporter.report(VideoStatusState::NoMonitor, Some(e.clone()));
                    return Err(e);
                }
            }
        }
    };
    #[cfg(windows)]
    let stable_monitor_name = match monitor.name() {
        Ok(name) if !name.trim().is_empty() => Some(name),
        Ok(_) if monitor_id.is_some() => {
            let e = "requested monitor has no stable Windows display name".to_string();
            reporter.report(VideoStatusState::NoMonitor, Some(e.clone()));
            return Err(e);
        }
        Err(e) if monitor_id.is_some() => {
            let e = format!("requested monitor's stable Windows display name is unreadable: {e}");
            reporter.report(VideoStatusState::NoMonitor, Some(e.clone()));
            return Err(e);
        }
        _ => None,
    };
    let source_hint = (monitor.width().unwrap_or(0), monitor.height().unwrap_or(0));
    // A physically-rotated monitor: Windows DXGI hands over the raw scan-out and
    // reports its own rotation per frame (the orient pass rotates it upright);
    // every other backend delivers the upright presentation image (rotation 0).
    // `source_hint` is the monitor's reported size — it only pre-budgets the
    // encoder's starting bitrate; the first real frame locks the true size.
    let fps = tune.fps();
    let mut stats = StreamStats::new(route_id, mode);
    // One provenance line per stream — the numbers that explain a capped rate
    // at a glance: codec, whether the path was classed LAN (60) or WAN/Unknown
    // (30 cap), the effective fps ceiling, and the source size. A posted CEC
    // dial's line vs a direct LAN dial's makes the cap self-evident.
    tracing::info!(
        "video stream start {route_id}: {} · link {:?} · fps cap {fps} · source {}×{}",
        match mode {
            VideoMode::H264 => "H.264",
            VideoMode::Mjpeg => "MJPEG",
        },
        tune.link,
        source_hint.0,
        source_hint.1,
    );

    // Windows, H.264: the GPU zero-copy lane runs INSTEAD of the CPU
    // pipeline when the whole chain comes up (duplication, cursor, and
    // colour conversion on one D3D11 device; the encoder MFT reading the
    // NV12 textures in place — see `win_capture::start_gpu` and
    // `run_gpu_lane`). It's tried before any CPU encoder exists, so a
    // GPU-lane route holds ONE hardware encoder session, not two. Every
    // failure is soft — the reason is logged and the proven CPU path below
    // takes over; a mode or edge-cap change restarts the lane with fresh
    // geometry, budgeted so a flapping driver can't loop forever. The
    // operator's adapter pin wins over the lane: pinning exists to encode
    // on a *different* GPU than the display's, which is inherently
    // cross-adapter — the CPU lane's system-memory NV12 serves that.
    #[cfg(windows)]
    if mode == VideoMode::H264
        && tune.posture() == Posture::StudioLossless
        && !gpu_lane_allowed(tune.posture())
    {
        tracing::warn!(
            "Studio·Lossless requested for {route_id}, but its HEVC media framing is quarantined; \
             falling back to high-bitrate lossy Studio H.264 (set ALLMYSTUFF_GPU_LANE=1 only for fork experiments)"
        );
    }
    #[cfg(windows)]
    if mode == VideoMode::H264
        && gpu_lane_allowed(tune.posture())
        && !crate::mediafoundation::adapter_pin_active()
    {
        if let Ok(mid) = monitor.id() {
            // Coarse rung label for the effective-dials panel while the GPU
            // lane owns the stream; the CPU fallback relabels itself in
            // HealingEncoder::new. The exact NVENC/MF/AMF rung is chosen
            // inside run_gpu_lane, which this pass deliberately leaves alone.
            stats.set_encoder("GPU (hardware)");
            let mut window = Instant::now();
            let mut attempts = 0u32;
            loop {
                if stop.load(Ordering::SeqCst) {
                    return Ok(());
                }
                if window.elapsed() > Duration::from_secs(60) {
                    window = Instant::now();
                    attempts = 0;
                }
                attempts += 1;
                if attempts > 10 {
                    tracing::warn!(
                        "GPU lane for {route_id} restarting too often; using the CPU lane"
                    );
                    break;
                }
                match run_gpu_lane(
                    stop,
                    fps,
                    route_id,
                    mid,
                    stable_monitor_name.as_deref(),
                    tune,
                    refresh,
                    idr_ms,
                    auto,
                    on_packet,
                    &mut stats,
                    &mut reporter,
                ) {
                    GpuEnd::Stopped => return Ok(()),
                    GpuEnd::Restart => continue,
                    GpuEnd::Fallback(why) => {
                        tracing::info!(
                            "GPU lane unavailable for {route_id} ({why}); using the CPU lane"
                        );
                        break;
                    }
                }
            }
        }
    }

    let mut encoder =
        HealingEncoder::new(route_id, mode, source_hint, tune, refresh, idr_ms, auto)?;
    // The negotiated transport pre-labelled the stats line; correct it if
    // the encoder fell to the MJPEG floor.
    stats.set_mode(encoder.mode());

    // Windows: our own DXGI Output Duplication session — damage-driven,
    // per-monitor, releasable (see `win_capture` for why xcap's recorder
    // can't carry this). A failed start or a dead session falls back to the
    // screenshot loop, but only for [`DXGI_REPROMOTE_AFTER`] at a time:
    // duplication failures are usually transient (a fullscreen-exclusive
    // app, the UAC desktop, a driver reset), and a route used to stay
    // pinned on soft, expensive GDI grabs for its whole life once it fell.
    #[cfg(windows)]
    {
        match monitor.id() {
            Ok(mid) => loop {
                match crate::win_capture::start_named(mid, stable_monitor_name.as_deref()) {
                    Ok((session, frames, reclaim)) => {
                        tracing::info!(
                            "DXGI duplication started for {route_id} (monitor {mid:#x})"
                        );
                        let reclaim_fn = |buf: Vec<u8>| {
                            let _ = reclaim.try_send(buf);
                        };
                        let result = pump_frames(
                            stop,
                            fps,
                            &frames,
                            move |f: crate::win_capture::RawFrame| {
                                // DXGI is the only raw, unrotated scan-out; its own
                                // DXGI_OUTDUPL_DESC.Rotation (carried on the frame)
                                // is authoritative for making it upright.
                                orient_to_monitor(f.rgba, f.width, f.height, f.rotation_deg)
                            },
                            on_packet,
                            &mut encoder,
                            &mut stats,
                            &mut reporter,
                            Some(&reclaim_fn),
                        );
                        drop(session);
                        match result {
                            Ok(()) => return Ok(()),
                            Err(e) => {
                                if stop.load(Ordering::SeqCst) {
                                    return Ok(());
                                }
                                tracing::warn!(
                                    "DXGI duplication for {route_id} ended ({e}); \
                                     falling back to per-frame screenshots"
                                );
                            }
                        }
                    }
                    Err(e) => tracing::warn!(
                        "DXGI duplication for {route_id} unavailable ({e}); \
                         falling back to per-frame screenshots"
                    ),
                }
                // A fresh monitor handle for the fallback — the duplication
                // may have died to a mode change that moved things around.
                let monitor = select_monitor(monitor_id).inspect_err(|e| {
                    reporter.report(VideoStatusState::NoMonitor, Some(e.clone()));
                })?;
                if !run_oneshot_capture(
                    stop,
                    fps,
                    route_id,
                    monitor_id,
                    monitor,
                    on_packet,
                    &mut encoder,
                    &mut stats,
                    &mut reporter,
                    Some(DXGI_REPROMOTE_AFTER),
                )? {
                    return Ok(()); // route stopped while degraded
                }
                tracing::info!(
                    "retrying DXGI duplication for {route_id} after the screenshot spell"
                );
            },
            Err(e) => tracing::warn!(
                "monitor id unreadable for {route_id} ({e}); \
                 falling back to per-frame screenshots"
            ),
        }
    }

    // Linux Wayland: our own portal session — the only sanctioned capture
    // there, run with a restore token so an unattended start is silent
    // (see `wayland_capture`). When no token is stored yet, the consent
    // dialog needs a human at this machine — say so to the viewer before
    // the wait. A failed or refused session degrades to the per-grab
    // path below, which keeps explaining itself in-band — and so does a
    // session that *opens* but never delivers a frame: a restored grant
    // can point at an output the compositor no longer paints (silent
    // token restores skip the initial frame some compositors only send
    // on fresh consent; a KVM-switched or re-plugged monitor strands the
    // grant entirely), and without a deadline that's a black stage
    // forever with "display asleep" as a wrong diagnosis.
    #[cfg(target_os = "linux")]
    if wayland_session() {
        let had_token = crate::wayland_capture::has_restore_token(monitor_id);
        if !had_token {
            reporter.report(VideoStatusState::WaitingConsent, None);
        }
        match crate::wayland_capture::open(monitor_id) {
            Ok((session, frames)) => {
                tracing::info!("wayland screencast session started for {route_id}");
                let result = pump_frames_with_stall(
                    stop,
                    fps,
                    &frames,
                    move |f: crate::wayland_capture::RawFrame| {
                        // The portal hands over the upright presentation image.
                        orient_to_monitor(f.rgba, f.width, f.height, 0)
                    },
                    on_packet,
                    &mut encoder,
                    &mut stats,
                    &mut reporter,
                    VideoStatusState::DisplayAsleep,
                    Some(WAYLAND_FIRST_FRAME_DEADLINE),
                    None,
                );
                drop(session);
                if stop.load(Ordering::SeqCst) {
                    return Ok(());
                }
                match result {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        // A frameless restored session means the saved token
                        // points at an output the compositor no longer paints:
                        // forget it so the next connect re-prompts for fresh
                        // consent (the real recovery) instead of replaying a
                        // dead grant into another frameless wait.
                        if had_token && e.contains("no frame") {
                            crate::wayland_capture::forget_token(monitor_id);
                            tracing::warn!(
                                "wayland screencast for {route_id} delivered no frames; \
                                 dropped its restore token — reconnect to re-consent"
                            );
                        } else {
                            tracing::warn!("wayland screencast for {route_id} ended: {e}");
                        }
                        reporter.report(VideoStatusState::GrabFailed, Some(e));
                    }
                }
            }
            Err(e) => {
                tracing::warn!("wayland screencast for {route_id} unavailable: {e}");
                reporter.report(VideoStatusState::GrabFailed, Some(e));
            }
        }
        // On Wayland the per-frame Screenshot fallback is *not* an acceptable
        // degrade: xcap's grab there routes through the xdg-desktop-portal
        // Screenshot API, and most compositors (GNOME especially) play their
        // screenshot-flash animation on every single grab — the whole panel
        // strobes white at the capture rate, useless and alarming. The
        // ScreenCast portal is the only sane capture path on Wayland, so when
        // it can't run we surface that and stop rather than flash the screen.
        return Ok(());
    }

    // macOS: xcap's AVFoundation session. Two attempts, each with a
    // freshly enumerated monitor: a route that restarts can hand the
    // session a stale display handle, and re-enumerating is exactly what
    // heals that. Only then do we settle for per-frame screenshots — the
    // dire-framerate path of last resort.
    #[cfg(target_os = "macos")]
    for attempt in 0..2 {
        let monitor = select_monitor(monitor_id)?;
        match run_session_capture(
            stop,
            fps,
            route_id,
            &monitor,
            on_packet,
            &mut encoder,
            &mut stats,
            &mut reporter,
        ) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if stop.load(Ordering::SeqCst) {
                    return Ok(());
                }
                if attempt == 0 {
                    tracing::warn!(
                        "capture session for {route_id} unavailable ({e}); \
                         retrying with a fresh monitor handle"
                    );
                    std::thread::sleep(Duration::from_millis(300));
                } else {
                    tracing::warn!(
                        "capture session for {route_id} unavailable ({e}); \
                         falling back to per-frame screenshots"
                    );
                }
            }
        }
    }
    let monitor = select_monitor(monitor_id).inspect_err(|e| {
        reporter.report(VideoStatusState::NoMonitor, Some(e.clone()));
    })?;
    run_oneshot_capture(
        stop,
        fps,
        route_id,
        monitor_id,
        monitor,
        on_packet,
        &mut encoder,
        &mut stats,
        &mut reporter,
        None,
    )
    .map(|_| ())
}

/// Mirrors xcap's (private) `wayland_detect`, so our path choice matches
/// the one xcap will take internally.
#[cfg(target_os = "linux")]
fn wayland_session() -> bool {
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
    session.eq_ignore_ascii_case("wayland") || display.to_lowercase().contains("wayland")
}

/// How long a just-started session may produce nothing before the
/// viewer is told the display is dark. Only the *first* frame is held
/// to this — once one arrived, a quiet channel is an idle desktop on a
/// damage-driven backend, not a problem.
const FIRST_FRAME_STALL: Duration = Duration::from_secs(5);

/// How long a Wayland portal session may stay frameless before the
/// route gives up on it and degrades to per-frame grabs. Longer than
/// [`FIRST_FRAME_STALL`] so the viewer still hears the honest interim
/// status, and long enough for two wake pulses (one per ~3 s) to coax
/// out a first frame if sleep really was the story. Wayland-only: on
/// Windows/macOS a frameless session reliably *is* a dark display and
/// the wait is the right behaviour, but a portal stream restored from
/// a token can be silently dead on an awake desktop, and only the
/// one-shot path (the Screenshot portal) still produces pixels there.
///
/// Linux-only: the sole caller is the Wayland capture arm, so the constant
/// would be dead code elsewhere (and `-D warnings` rejects it on macOS/Windows).
#[cfg(target_os = "linux")]
const WAYLAND_FIRST_FRAME_DEADLINE: Duration = Duration::from_secs(20);

/// Drain a session's frame channel into the encoder, paced to the target
/// rate: each tick encodes the *freshest* pending frame; a backlog is
/// skipped, never transcoded late. Generic over the session's frame type
/// (`raw` extracts RGBA + dimensions) so the platform backends — our DXGI
/// duplication, xcap's recorders — share one pump. (The Wayland arm
/// calls [`pump_frames_with_stall`] directly: it's the one session a
/// first-frame deadline applies to.)
#[cfg(any(windows, target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
fn pump_frames<T, X>(
    stop: &AtomicBool,
    fps: u32,
    frames: &mpsc::Receiver<T>,
    raw: X,
    on_packet: &(dyn Fn(VideoPacket) -> bool + Send + Sync),
    encoder: &mut HealingEncoder,
    stats: &mut StreamStats,
    reporter: &mut StatusReporter,
    reclaim: Option<&dyn Fn(Vec<u8>)>,
) -> Result<(), String>
where
    X: Fn(T) -> (Vec<u8>, u32, u32),
{
    pump_frames_with_stall(
        stop,
        fps,
        frames,
        raw,
        on_packet,
        encoder,
        stats,
        reporter,
        VideoStatusState::DisplayAsleep,
        None,
        reclaim,
    )
}

/// Hand a batch of encoded packets to the forwarder, counting each send and
/// drop. A backlogged hardware encoder can drain several units in one call;
/// every one must reach the wire **in order** or the viewer's P-frame chain
/// snaps (the smear-until-IDR failure this replaced).
fn emit_packets(
    packets: Vec<VideoPacket>,
    on_packet: &(dyn Fn(VideoPacket) -> bool + Send + Sync),
    stats: &mut StreamStats,
) {
    for p in packets {
        if on_packet(p) {
            stats.sent += 1;
        } else {
            stats.dropped += 1;
        }
    }
}

/// [`pump_frames`] with the first-frame-stall condition named by the
/// caller: a frameless screen session is a dark display (worth wake
/// pressure on the panel), a frameless camera is the camera failing —
/// different words to the viewer, and no point lighting the screen.
/// `first_frame_deadline` bounds how long the session may stay
/// frameless before the pump gives up with an error (so the caller can
/// degrade to another capture path); `None` waits as long as the route
/// lives. Only the first frame is ever held to it. `reclaim`, when the
/// backend offers one, receives each source frame's buffer once the
/// capture side is done with it — the backend copies the next frame into
/// warm pages instead of a fresh demand-zeroed allocation.
#[allow(clippy::too_many_arguments)]
fn pump_frames_with_stall<T, X>(
    stop: &AtomicBool,
    fps: u32,
    frames: &mpsc::Receiver<T>,
    raw: X,
    on_packet: &(dyn Fn(VideoPacket) -> bool + Send + Sync),
    encoder: &mut HealingEncoder,
    stats: &mut StreamStats,
    reporter: &mut StatusReporter,
    stall_state: VideoStatusState,
    first_frame_deadline: Option<Duration>,
    reclaim: Option<&dyn Fn(Vec<u8>)>,
) -> Result<(), String>
where
    X: Fn(T) -> (Vec<u8>, u32, u32),
{
    let budget = Duration::from_secs(1) / fps.max(1);
    let started = Instant::now();
    let from_screen = stall_state == VideoStatusState::DisplayAsleep;
    let mut got_any = false;

    // The pump is a two-stage pipeline: THIS thread (capture side) drains
    // the backend, orients, and converts to the encode side's advertised
    // layout, while a scoped encode thread runs the codec — so conversion
    // of frame N overlaps the encode of frame N−1 instead of sharing one
    // 16.6 ms budget (the measured serial cost was the ~40 fps ceiling).
    // The stage channel is shallow and the encode side drains to freshest:
    // latency never accumulates, staleness is dropped. The encode side owns
    // the quiet-spell logic (quiesce IDR + refinement) since quiet is "no
    // staged frames", and publishes its input needs (layout + fit edge)
    // through atomics the capture side reads per frame — a healing rebuild
    // that changes either costs at most one dropped frame.
    let (initial_format, initial_edge) = encoder.current_needs();
    let need_format = AtomicU8::new(stage_need_to_u8(initial_format));
    let need_edge = AtomicU32::new(initial_edge);
    let (stage_tx, stage_rx) = mpsc::sync_channel::<Staged>(2);
    // Spent multi-megabyte convert buffers ride back to the capture side for
    // reuse: steady state converts into already-touched pages instead of
    // paying an OS large-allocation + demand-zeroing per frame per thread —
    // the churn that measurably held the pipelined pump at the serial rate.
    let (spent_tx, spent_rx) = mpsc::sync_channel::<Vec<u8>>(4);
    // Set by the encode side on ANY exit so the capture side never keeps
    // converting into a dead stage; its error surfaces through the join.
    let encode_done = AtomicBool::new(false);

    std::thread::scope(|s| {
        let need_format = &need_format;
        let need_edge = &need_edge;
        let encode_done = &encode_done;
        let encode_side = s.spawn(move || -> Result<(), String> {
            crate::os_perf::boost_media_thread();
            // Finished-with buffers go home to the capture side; a full
            // return lane just means the pool is topped up and this one
            // deallocates normally.
            let mut recycle = move |buf: Vec<u8>| {
                let _ = spent_tx.try_send(buf);
            };
            // Whether the current quiet spell already got its convergence
            // re-emit. Re-armed by every staged frame.
            let mut quiesced = false;
            // The post-quiesce refinement (see [`REFINE_PASSES`]): armed by
            // the convergence IDR, disarmed by any staged frame.
            let mut refines_left: u8 = 0;
            let mut refine_at: Option<Instant> = None;
            let result = loop {
                if stop.load(Ordering::SeqCst) {
                    break Ok(());
                }
                match stage_rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(mut staged) => {
                        // Freshest-wins: a backlog means we're behind; the
                        // newest picture is the only one worth encoding —
                        // and a displaced frame's buffer goes straight home.
                        // Displaced frames count as drops so the stats line
                        // shows "encode side behind" distinctly from frames
                        // lost downstream.
                        while let Ok(newer) = stage_rx.try_recv() {
                            stats.dropped += 1;
                            if let Prepared::Yuv(_, buf) =
                                std::mem::replace(&mut staged, newer).frame
                            {
                                recycle(buf);
                            }
                        }
                        quiesced = false;
                        refines_left = 0;
                        refine_at = None;
                        stats.add_scale(staged.scale_spent);
                        match encoder.encode_staged(staged, stats, &mut recycle) {
                            Ok(packets) => emit_packets(packets, on_packet, stats),
                            Err(e) => break Err(e),
                        }
                        stats.maybe_log();
                        let (format, edge) = encoder.current_needs();
                        need_format.store(stage_need_to_u8(format), Ordering::Relaxed);
                        need_edge.store(edge, Ordering::Relaxed);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        // The screen went QUIET (change-driven capture
                        // delivers nothing while nothing moves). Every
                        // refresh mechanism — STATIC_REFRESH, the IDR
                        // cadence, a viewer's refresh ask — lives inside
                        // `encode`, which no longer runs; so if the
                        // transport dropped the tail of the last motion
                        // burst, the viewer stays frozen BEHIND the true
                        // screen until the next motion. Serve convergence
                        // from the encoder's retained last picture: one
                        // clean re-emit per quiet spell (this first ~250 ms
                        // timeout is the quiesce debounce), plus one per
                        // refresh ask that lands while quiet — then the
                        // refinement passes sharpen what the burst left
                        // coarse.
                        if !quiesced || encoder.wants_refresh() {
                            quiesced = true;
                            match encoder.re_emit(stats, true) {
                                Ok(packets) => {
                                    let emitted = !packets.is_empty();
                                    emit_packets(packets, on_packet, stats);
                                    if emitted && encoder.mode() == VideoMode::H264 {
                                        refines_left = REFINE_PASSES;
                                        refine_at = Some(Instant::now() + REFINE_AFTER);
                                    }
                                }
                                Err(e) => break Err(e),
                            }
                            stats.maybe_log();
                        } else if refines_left > 0 && refine_at.is_some_and(|t| Instant::now() >= t)
                        {
                            match encoder.re_emit(stats, false) {
                                Ok(packets) => {
                                    emit_packets(packets, on_packet, stats);
                                    refines_left -= 1;
                                    refine_at =
                                        (refines_left > 0).then(|| Instant::now() + REFINE_SPACING);
                                }
                                Err(e) => break Err(e),
                            }
                            stats.maybe_log();
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(()),
                }
            };
            encode_done.store(true, Ordering::SeqCst);
            result
        });

        // Capture side (this thread). `local_spare` keeps buffers reclaimed
        // from frames the stage refused (encode side behind) so their pages
        // stay in rotation without a round trip.
        let mut local_spare: Vec<Vec<u8>> = Vec::new();
        let captured: Result<(), String> = loop {
            if stop.load(Ordering::SeqCst) || encode_done.load(Ordering::SeqCst) {
                break Ok(());
            }
            // A bounded wait keeps the stop flag responsive on idle screens.
            let mut frame = match frames.recv_timeout(Duration::from_millis(250)) {
                Ok(f) => f,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // A screen session that opened fine but never delivers
                    // is a dark display: damage-driven backends send nothing
                    // from a sleeping screen, and even a still desktop hands
                    // over its first frame on connect. Keep wake pressure on
                    // the panel the whole frameless window (the pulse
                    // rate-limits itself) — one polite wiggle at start
                    // demonstrably isn't enough on Windows.
                    if !got_any {
                        if from_screen {
                            wake::force_display_on();
                        }
                        if started.elapsed() >= FIRST_FRAME_STALL {
                            reporter.report(stall_state, None);
                        }
                        if let Some(deadline) = first_frame_deadline {
                            if started.elapsed() >= deadline {
                                break Err(format!(
                                    "no frame within {}s of session start",
                                    deadline.as_secs()
                                ));
                            }
                        }
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err("capture session ended".to_string());
                }
            };
            got_any = true;
            reporter.report(VideoStatusState::Ok, None);
            let frame_start = Instant::now();
            while let Ok(newer) = frames.try_recv() {
                frame = newer;
            }
            let (rgba, sw, sh) = raw(frame);
            let staged = match stage_need_from_u8(need_format.load(Ordering::Relaxed)) {
                None => Staged {
                    frame: Prepared::Rgba(rgba),
                    dw: sw,
                    dh: sh,
                    sw,
                    sh,
                    scale_spent: Duration::ZERO,
                },
                Some(format) => {
                    // The convert indexes by dims — a short backend buffer
                    // must be dropped here, not panicked on (the encode
                    // side's own guard covers the RGBA lane).
                    if rgba.len() < (sw as usize) * (sh as usize) * 4 {
                        tracing::debug!(
                            "capture frame too short ({} bytes for {sw}x{sh}); dropped",
                            rgba.len()
                        );
                        continue;
                    }
                    let edge = need_edge.load(Ordering::Relaxed).max(320);
                    let (dw, dh) = fit_within_even(sw, sh, edge);
                    let t0 = Instant::now();
                    // Convert into a recycled buffer when one has come home.
                    let mut buf = local_spare
                        .pop()
                        .or_else(|| spent_rx.try_recv().ok())
                        .unwrap_or_default();
                    match format {
                        YuvFormat::I420 => scale_rgba_to_i420_into(&rgba, sw, sh, dw, dh, &mut buf),
                        YuvFormat::Nv12 => scale_rgba_to_nv12_into(&rgba, sw, sh, dw, dh, &mut buf),
                    }
                    let scale_spent = t0.elapsed();
                    // The source frame is spent — hand its pages back to the
                    // capture backend if it runs a reclaim lane.
                    if let Some(reclaim) = reclaim {
                        reclaim(rgba);
                    }
                    Staged {
                        frame: Prepared::Yuv(format, buf),
                        dw,
                        dh,
                        sw,
                        sh,
                        scale_spent,
                    }
                }
            };
            // try_send: a full stage means the encode side is behind; this
            // frame is stale by definition and the next conversion carries
            // a fresher picture — but its buffer is worth keeping.
            if let Err(refused) = stage_tx.try_send(staged) {
                let staged = match refused {
                    mpsc::TrySendError::Full(st) | mpsc::TrySendError::Disconnected(st) => st,
                };
                if let Prepared::Yuv(_, buf) = staged.frame {
                    if local_spare.len() < 2 {
                        local_spare.push(buf);
                    }
                }
            }
            if let Some(rest) = budget.checked_sub(frame_start.elapsed()) {
                std::thread::sleep(rest);
            }
        };
        // Hang up the stage so the encode side drains out, then surface its
        // verdict — an encode-side death outranks a clean capture exit, and
        // the viewer hears why instead of a frozen frame.
        drop(stage_tx);
        let encoded = encode_side
            .join()
            .unwrap_or_else(|_| Err("encode stage panicked".to_string()));
        if let Err(e) = encoded {
            if !stop.load(Ordering::SeqCst) {
                reporter.report(VideoStatusState::GrabFailed, Some(e.clone()));
            }
            return Err(e);
        }
        captured
    })
}

/// Bring a captured frame upright to match a **physically-rotated** monitor.
///
/// `rotation_deg` is **authoritative and backend-supplied**. Windows DXGI
/// Output Duplication hands over the raw, *unrotated* scan-out and reports its
/// true clockwise orientation via `DXGI_OUTDUPL_DESC.Rotation` (0/90/180/270),
/// carried on the frame. Every other backend (X11 grab, Wayland portal, macOS
/// AVFoundation, Windows GDI/WGC fallback) delivers the already-upright
/// *presentation* image and passes 0, falling through the 0-turn path
/// untouched.
///
/// This replaces an earlier orientation-mismatch heuristic that compared the
/// buffer against the monitor's reported size: it read as "not rotated at all"
/// whenever the monitor library reported native (unrotated) dimensions, so the
/// mismatch never fired. DXGI's rotation is the actual scan-out orientation and
/// does not under-report, so it is trusted exclusively. Quarter-turns = deg/90,
/// clockwise (`rotate_rgba`'s convention): ROTATE90→1, ROTATE270→3 — undoing
/// the display's counter-clockwise rotation (direction verified against the
/// canonical DXGI Desktop Duplication sample's vertex transform).
fn orient_to_monitor(rgba: Vec<u8>, bw: u32, bh: u32, rotation_deg: u32) -> (Vec<u8>, u32, u32) {
    let turns = ((rotation_deg / 90) % 4) as u8;
    if turns == 0 {
        log_orient(rotation_deg, bw, bh, turns, bw, bh);
        return (rgba, bw, bh);
    }
    // A short buffer must never reach the rotator's indexing (a panic
    // aborts the node under the release profile). Hand it back unrotated —
    // the encoder's own length guard rejects it as a stream error.
    if rgba.len() < (bw as usize) * (bh as usize) * 4 {
        return (rgba, bw, bh);
    }
    // rotate_rgba swaps dims for odd quarter-turns and preserves them for 180°.
    let (rotated, ow, oh) = allmystuff_pixels::rotate_rgba(&rgba, bw, bh, turns);
    log_orient(rotation_deg, bw, bh, turns, ow, oh);
    (rotated, ow, oh)
}

/// One-line ground-truth log for [`orient_to_monitor`], emitted once per
/// distinct (rotation, dims, turns) tuple so a rotated-monitor report is
/// self-diagnosing without printing a line per frame. Survives multiple
/// concurrent monitors (each distinct config logs exactly once, ever).
fn log_orient(rotation_deg: u32, bw: u32, bh: u32, turns: u8, ow: u32, oh: u32) {
    use std::collections::hash_map::DefaultHasher;
    use std::collections::HashSet;
    use std::hash::{Hash, Hasher};
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();
    let mut h = DefaultHasher::new();
    (rotation_deg, bw, bh, turns).hash(&mut h);
    let fresh = SEEN
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map(|mut s| s.insert(h.finish()))
        .unwrap_or(false);
    if fresh {
        tracing::info!(
            target: "capture::orient",
            "rotation_deg={rotation_deg} buf={bw}x{bh} -> turns={turns} out={ow}x{oh}"
        );
    }
}

/// Explicit opt-in for the GPU zero-copy capture→encode lane
/// (`ALLMYSTUFF_GPU_LANE=1`).
///
/// The lane proves that its D3D objects can be created, but that does not
/// prove that the capture texture is ready without a long driver wait when
/// the encoder consumes it. Field A/B runs found both direct NVENC and the
/// NVIDIA MFT blocking for hundreds of milliseconds on these live textures,
/// while the same encoders stayed in the low milliseconds on CPU-DXGI input.
/// Keep normal production postures on that bounded-latency path; the shared-
/// texture lane remains available for explicit experimental runs. This also
/// quarantines Studio·Lossless in production: its direct encoder emits HEVC,
/// while the existing negotiated media track is H.264-framed. Until the fork
/// grows a data-plane HEVC track contract, the selected posture fails soft to
/// high-bitrate lossy Studio instead of opening a stream the viewer cannot
/// decode.
#[cfg(windows)]
fn gpu_lane_preference() -> Option<bool> {
    static CHOICE: std::sync::LazyLock<Option<bool>> = std::sync::LazyLock::new(|| {
        let raw = std::env::var("ALLMYSTUFF_GPU_LANE").ok();
        let choice = explicit_bool_choice(raw.as_deref());
        match choice {
            Some(true) => {
                tracing::info!("ALLMYSTUFF_GPU_LANE=1 — experimental GPU zero-copy lane enabled")
            }
            Some(false) => {
                tracing::info!("ALLMYSTUFF_GPU_LANE=0 — using CPU-DXGI capture + hardware encode")
            }
            None => {}
        }
        choice
    });
    *CHOICE
}

#[cfg(windows)]
fn gpu_lane_policy(_posture: Posture, gpu_lane_choice: Option<bool>) -> bool {
    gpu_lane_choice.unwrap_or(false)
}

#[cfg(windows)]
fn gpu_lane_allowed(posture: Posture) -> bool {
    gpu_lane_policy(posture, gpu_lane_preference())
}

/// How a GPU-lane run ended. Hard errors fold into `Fallback`'s reason —
/// the lane never takes the route down; the CPU path always gets its turn.
#[cfg(windows)]
enum GpuEnd {
    /// The route was stopped — done.
    Stopped,
    /// The lane's geometry went stale (mode change, edge-cap change) —
    /// start a fresh lane.
    Restart,
    /// The lane can't run here (rotated output, no capable MFT, persistent
    /// encoder failure…) — use the CPU path, and say why.
    Fallback(String),
}

/// Opt-in for the direct-NVENC rung of the GPU lane
/// (`ALLMYSTUFF_NVENC=1`). Default OFF until soaked — the MF rung is the
/// proven default; flipping this on puts the SDK session first with the
/// MF rung as its in-lane fallback.
#[cfg(windows)]
fn explicit_bool_choice(value: Option<&str>) -> Option<bool> {
    match value?.trim().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" => Some(true),
        "0" | "off" | "false" => Some(false),
        _ => None,
    }
}

#[cfg(windows)]
fn nvenc_preference() -> Option<bool> {
    static CHOICE: std::sync::LazyLock<Option<bool>> = std::sync::LazyLock::new(|| {
        let raw = std::env::var("ALLMYSTUFF_NVENC").ok();
        let choice = explicit_bool_choice(raw.as_deref());
        match choice {
            Some(true) => tracing::info!("ALLMYSTUFF_NVENC=1 — direct NVENC rung enabled"),
            Some(false) => {
                tracing::info!("ALLMYSTUFF_NVENC=0 — direct NVENC rung disabled")
            }
            None => {}
        }
        choice
    });
    *CHOICE
}

#[cfg(windows)]
fn nvenc_opt_in() -> bool {
    // Balanced, Game, and lossy Studio use the vendor MFT by default. The
    // direct SDK rung is real hardware, but field production found a driver
    // combination where its synchronous LockBitstream call blocked for
    // 0.4–1.0 s per 1440p frame while the NVIDIA MFT on the same machine
    // stayed in the low milliseconds. Keep the experimental rung explicit
    // until it has its own bounded-latency watchdog.
    nvenc_preference().unwrap_or(false)
}

#[cfg(windows)]
fn nvenc_lossless_allowed() -> bool {
    // Studio·Lossless has no equivalent MFT rung: its HEVC const-QP-0
    // encoder is direct NVENC. Selecting that posture is itself an explicit
    // request, while ALLMYSTUFF_NVENC=0 must still be able to pin the proven
    // MFT ladder for diagnostics.
    nvenc_preference().unwrap_or(true)
}

/// The GPU lane's texture-eating encoder: the direct-NVENC SDK session
/// (opt-in, game-mode levers) or the Media Foundation MFT (the proven
/// default, and the in-lane fallback the healer steps down to).
#[cfg(windows)]
enum GpuCodec {
    Nvenc(crate::nvenc::NvencH264),
    Amf(crate::amf::AmfAvc),
    Mf(crate::mediafoundation::MediaFoundationH264),
}

#[cfg(windows)]
impl GpuCodec {
    fn encode_texture(
        &mut self,
        tex: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        force_idr: bool,
    ) -> Result<EncodeOutcome, String> {
        match self {
            GpuCodec::Nvenc(e) => e.encode_texture(tex, force_idr),
            GpuCodec::Amf(e) => e.encode_texture(tex, force_idr),
            GpuCodec::Mf(e) => e.encode_texture(tex, force_idr),
        }
    }

    fn label(&self) -> &str {
        match self {
            GpuCodec::Nvenc(e) => e.label(),
            GpuCodec::Amf(e) => e.label(),
            GpuCodec::Mf(e) => e.label(),
        }
    }

    /// Re-aim the rate controller in place. NVENC moves mean/peak/VBV
    /// with no reset; the MF rung honors what its codec API exposes
    /// (partial on some drivers — `false` means nothing moved).
    fn set_bitrate(&mut self, bitrate: u32) -> bool {
        match self {
            GpuCodec::Nvenc(e) => e.set_bitrate(bitrate),
            GpuCodec::Amf(e) => e.set_bitrate(bitrate),
            GpuCodec::Mf(e) => e.set_bitrate(bitrate),
        }
    }
}

/// Number of capture-ring textures kept out of rotation behind the encoder.
/// Direct SDK sessions need two; the asynchronous MF/AMF rungs need four.
/// The override is diagnostic-only and remains clamped to the ring's safe
/// operating range.
#[cfg(windows)]
fn gpu_retirement_depth(enc: &GpuCodec) -> usize {
    static RETIRE: std::sync::LazyLock<Option<usize>> = std::sync::LazyLock::new(|| {
        std::env::var("ALLMYSTUFF_RING_RETIRE")
            .ok()
            .and_then(|v| v.parse().ok())
    });
    RETIRE
        .unwrap_or(if matches!(enc, GpuCodec::Mf(_) | GpuCodec::Amf(_)) {
            4
        } else {
            2
        })
        .clamp(1, crate::gpu_pipeline::NV12_RING - 4)
}

/// Retain the newest source texture and return every texture beyond the
/// encoder rung's retirement depth to capture immediately. This is used for
/// consumed frames, unconsumed submissions, and encoder errors alike: the
/// latter two must keep their final pixels available for a quiet-path retry.
#[cfg(windows)]
fn retain_gpu_frame(
    retained: &mut std::collections::VecDeque<crate::win_capture::GpuFrame>,
    frame: crate::win_capture::GpuFrame,
    enc: &GpuCodec,
    release: &std::sync::mpsc::SyncSender<usize>,
) {
    retained.push_back(frame);
    let depth = gpu_retirement_depth(enc);
    while retained.len() > depth {
        if let Some(old) = retained.pop_front() {
            let _ = release.try_send(old.slot);
        }
    }
}

/// Open the best hardware H.264 MFT **on the lane's adapter**, bound to
/// its device manager — the encoder must live where the textures live.
/// `None` when no MFT on that adapter accepts the manager and types.
#[cfg(windows)]
fn open_gpu_encoder(
    adapter: windows::Win32::Foundation::LUID,
    w: u32,
    h: u32,
    fps: u32,
    bitrate: u32,
    game: bool,
    manager: &windows::Win32::Media::MediaFoundation::IMFDXGIDeviceManager,
) -> Option<crate::mediafoundation::MediaFoundationH264> {
    for hw in crate::mediafoundation::hardware_h264_mfts_on(adapter) {
        match hw.open_with_manager_for_route(w, h, fps, bitrate, Some(manager), game) {
            Ok(enc) => return Some(enc),
            Err(e) => tracing::debug!("GPU-lane MFT {} declined: {e}", hw.name()),
        }
    }
    None
}

/// One GPU-lane encoder error: skip, reopen, or give up per the shared
/// [`RebuildPolicy`]. `Ok(Some(_))` = a fresh encoder (the caller arms the
/// refresh flag — its first unit is an IDR); `Ok(None)` = skip this frame;
/// `Err(why)` = the lane is done, fall back.
#[cfg(windows)]
fn gpu_heal(
    policy: &mut RebuildPolicy,
    route_id: &str,
    err: &str,
    reopen: impl FnOnce() -> Option<crate::mediafoundation::MediaFoundationH264>,
) -> Result<Option<crate::mediafoundation::MediaFoundationH264>, String> {
    match policy.on_error(Instant::now()) {
        PolicyVerdict::SkipFrame => {
            tracing::debug!(
                "GPU-lane encoder for {route_id} still failing ({err}); rebuild not yet due"
            );
            Ok(None)
        }
        PolicyVerdict::GiveUp => Err(format!(
            "encoder failed persistently after {REBUILD_MAX} reopens: {err}"
        )),
        PolicyVerdict::Rebuild => {
            tracing::warn!("GPU-lane encoder for {route_id} failed ({err}); reopening");
            match reopen() {
                Some(enc) => Ok(Some(enc)),
                None => Err(format!("encoder reopen failed after: {err}")),
            }
        }
    }
}

/// The GPU zero-copy screen pump: frames arrive as NV12 *textures* from
/// [`crate::win_capture::start_gpu`] (duplication, cursor composite, and
/// colour conversion all on one D3D11 device) and go to a
/// device-manager-bound hardware MFT that reads them in place — no CPU
/// pixel work anywhere on the lane. Runs INSTEAD of the CPU pump when the
/// chain comes up; every failure is soft (the caller falls back to the
/// proven CPU path, which keeps its own healing ladder).
///
/// Mirrors the CPU pump's cadence machinery: freshest-wins drains, fps
/// pacing, the adaptive IDR cadence + viewer refresh flag, first-frame
/// wake pressure, and the quiet-spell convergence IDR + refinement passes
/// (re-encoding the retained last *texture*, whose ring slot stays
/// checked out until a newer frame replaces it). The static-skip byte
/// compare has no analog here on purpose: damage-driven duplication IS
/// the static gate.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn run_gpu_lane(
    stop: &AtomicBool,
    fps: u32,
    route_id: &str,
    mid: u32,
    stable_monitor_name: Option<&str>,
    tune: Tune,
    refresh: &Arc<AtomicBool>,
    idr_ms: &Arc<AtomicU64>,
    auto: &Arc<AutoAdapt>,
    on_packet: &(dyn Fn(VideoPacket) -> bool + Send + Sync),
    stats: &mut StreamStats,
    reporter: &mut StatusReporter,
) -> GpuEnd {
    let edge = effective_h264_edge(tune, auto);
    let lane = match crate::win_capture::start_gpu_named(mid, stable_monitor_name, edge) {
        Ok(l) => l,
        Err(e) => return GpuEnd::Fallback(e),
    };
    let (dw, dh) = lane.out_size;
    let bitrate = tuned_bitrate(tune, dw, dh, fps);
    let posture = tune.posture();
    if matches!(posture, Posture::Studio | Posture::StudioLossless) && tune.link != LinkClass::Lan {
        tracing::info!(
            "studio mode for {route_id} on a {:?} link — honoring the viewer's ask at {:.0} Mbps (their wire to own)",
            tune.link,
            bitrate as f64 / 1e6
        );
    }
    let game = posture == Posture::Game;
    let requested_lossless = posture == Posture::StudioLossless;
    // The lossy-fallback opens (rung unavailable, noise guard) run as
    // plain Studio — the posture's spirit with rate control back on.
    let studio = posture == Posture::Studio || requested_lossless;
    let mut enc = 'open: {
        if requested_lossless && nvenc_lossless_allowed() {
            // Studio·Lossless: the HEVC constQP-0 rung. Anything short of
            // a clean open degrades to lossy Studio below — the viewer
            // asked for fidelity, and 150 Mbps VBR is the honest next
            // rung, not a torn-down route.
            match crate::nvenc::NvencH264::open_lossless_hevc_on_device(&lane.device, dw, dh, fps) {
                Ok(n) => break 'open GpuCodec::Nvenc(n),
                Err(e) => tracing::warn!(
                    "lossless HEVC rung unavailable for {route_id} ({e}); running lossy studio"
                ),
            }
        }
        if nvenc_opt_in() {
            // The SDK rung first: same textures, direct session, the
            // game-mode levers (in game mode: GDR instead of IDR walls).
            match crate::nvenc::NvencH264::open_on_device(
                &lane.device,
                dw,
                dh,
                fps,
                bitrate,
                game,
                studio,
            ) {
                Ok(n) => break 'open GpuCodec::Nvenc(n),
                Err(e) => tracing::info!(
                    "direct NVENC unavailable for {route_id} ({e}); trying the AMF rung"
                ),
            }
        }
        // AMD's native SDK rung — the 9060 XT field host's first-class
        // path: GDR game mode, guaranteed in-place bitrate, posture
        // presets, pacer slices. Refuses instantly on non-AMD adapters
        // (a vendor check before any AMF call), so every other box falls
        // straight through to MF. `ALLMYSTUFF_AMF=0` pins MF for A/B on
        // the Radeon.
        if std::env::var("ALLMYSTUFF_AMF")
            .map(|v| v != "0")
            .unwrap_or(true)
        {
            match crate::amf::AmfAvc::open_on_device(
                &lane.device,
                dw,
                dh,
                fps,
                bitrate,
                game,
                studio,
            ) {
                Ok(a) => break 'open GpuCodec::Amf(a),
                Err(e) => tracing::debug!("AMF rung not taken for {route_id}: {e}"),
            }
        }
        match open_gpu_encoder(lane.adapter_luid, dw, dh, fps, bitrate, game, &lane.manager) {
            Some(m) => GpuCodec::Mf(m),
            None => return GpuEnd::Fallback("no hardware MFT accepted the shared device".into()),
        }
    };
    // Studio·Lossless noise guard: constQP-0 has no rate control, so
    // content the codec can't predict (full-screen confetti, static)
    // produces raw-sized frames at Gbps rates and ~2× encode latency.
    // Sustained oversize — most of the last two seconds above ~half a
    // byte per pixel (≈884 Mbps at 1440p60) — swaps the session to lossy
    // Studio in place. One-way: flapping back and forth would look worse
    // than either mode; the posture re-arms on the next retune.
    let noise_frame_bytes = (dw as usize * dh as usize) / 2;
    let mut noise_window: std::collections::VecDeque<bool> = std::collections::VecDeque::new();
    // Fast trip: ~300 ms of bytes at 4× the oversize rate ends the wait
    // early — the 2 s ratio window alone let ~2 s of Gbps-class frames
    // through before reacting (red team, pacing item 6).
    let mut noise_recent: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    let noise_burst_bytes = noise_frame_bytes * 72; // 18 frames × 4×
                                                    // Armed only when the lossless rung actually opened — a fallback that
                                                    // already runs rate-controlled studio has nothing to guard (red team,
                                                    // encoder finding 7). Runtime behavior follows what actually opened, not
                                                    // merely the requested posture. A failed lossless open has already landed
                                                    // on rate-controlled Studio and must immediately participate in rate
                                                    // adaptation and pacing.
    let mut lossless_active = enc.label().contains("studio-lossless");
    let mut noise_guard_armed = lossless_active;
    // Hold boost clocks while this route streams — the encode engine's
    // own load doesn't (the soak evidence lives on the struct). RAII:
    // every lane exit path drops it and the GPU goes back to sleep.
    let _clock_keeper = crate::gpu_pipeline::ClockKeeper::start(&lane.device);
    // GDR streams have no periodic-IDR cadence by design — the refresh
    // wave is the convergence mechanism; only explicit asks (viewer join,
    // quiesce rescue) force an IDR through. Mutable: a mid-stream heal
    // onto the MF rung loses GDR, and the cadence must come back with it
    // (red team, encoder finding 4).
    // Both SDK rungs speak GDR: NVENC arms per-picture waves; AMF runs a
    // continuous rolling refresh (a wave permanently in flight), so its
    // loss heal needs no arming at all.
    let mut gdr = game && matches!(&enc, GpuCodec::Nvenc(_) | GpuCodec::Amf(_));
    // Frame health's wave flag: registered while this GDR lane lives so
    // a viewer's loss report heals with a wave instead of an IDR wall;
    // the guard unregisters on every lane exit path. The value carries
    // the requested wave length (frames; 0 = idle).
    let wave = std::sync::Arc::new(AtomicU32::new(0));
    struct WaveReg(Option<String>);
    impl Drop for WaveReg {
        fn drop(&mut self) {
            if let Some(id) = self.0.take() {
                wave_flags().lock().remove(&id);
            }
        }
    }
    let _wave_reg = WaveReg(gdr.then(|| {
        wave_flags()
            .lock()
            .insert(route_id.to_string(), wave.clone());
        route_id.to_string()
    }));
    // The route's rate seam: the AIMD controller (feedback path) writes
    // `target`, this thread applies it through the in-place reconfigure,
    // and the mesh pacer reads it to spread bursts at the rate the link
    // is actually asked to carry. Not registered for lossless — constQP
    // has no rate to move, and the pacer's lossless shaping is the
    // LAN-tuned path on purpose.
    let rate = std::sync::Arc::new(RouteRate {
        target: AtomicU32::new(bitrate),
        ceiling: AtomicU32::new(bitrate),
        adapt: Mutex::new(RateAdaptState::default()),
    });
    struct RateReg(Option<String>);
    impl Drop for RateReg {
        fn drop(&mut self) {
            if let Some(id) = self.0.take() {
                route_rates().lock().remove(&id);
            }
        }
    }
    let mut rate_registered = false;
    let register_rate = |registered: &mut bool| {
        if !*registered {
            route_rates()
                .lock()
                .insert(route_id.to_string(), rate.clone());
            *registered = true;
        }
    };
    if !lossless_active {
        register_rate(&mut rate_registered);
    }
    let _rate_reg = RateReg(Some(route_id.to_string()));
    let mut applied_rate = bitrate;
    tracing::info!(
        "GPU zero-copy lane for {route_id}: {} · {}×{} fitted to {dw}×{dh} · {:.1} Mbps @ {fps} fps",
        enc.label(),
        lane.src_size.0,
        lane.src_size.1,
        bitrate as f64 / 1e6
    );
    (stats.out_w, stats.out_h) = (dw, dh);

    let budget = Duration::from_secs(1) / fps.max(1);
    let started = Instant::now();
    let mut got_any = false;
    let mut policy = RebuildPolicy::new();
    // The newest source texture plus the rung's bounded retirement tail,
    // kept checked out. The newest is the quiet path's re-offer picture —
    // including when SubmitInput did not consume it or an encoder error is
    // about to heal onto MF. The tail is the slot-race fix: an asynchronous
    // encoder can still be reading an older texture after a later submit.
    let mut retained: std::collections::VecDeque<crate::win_capture::GpuFrame> =
        std::collections::VecDeque::with_capacity(gpu_retirement_depth(&enc));
    let mut last_idr: Option<Instant> = None;
    let mut last_emit: Option<Instant> = None;
    // Watchdog: an MFT that consumes frames but never produces a unit is a
    // silent brick. The CPU ladder's synthetic frame-send test can't run
    // here (it would need a texture from this very lane), so the live
    // stream is the send test.
    let mut units_ever = false;
    let mut consumed_without_unit = 0u32;
    let mut quiesced = false;
    let mut refines_left: u8 = 0;
    let mut refine_at: Option<Instant> = None;

    loop {
        if stop.load(Ordering::SeqCst) {
            return GpuEnd::Stopped;
        }
        // Game mode wakes the quiet path 5× faster: the quiesce IDR (and
        // with it the refinement ladder) lands within ~50 ms of motion
        // stopping instead of ~250 — the "screen snaps crisp the moment
        // you stop" feel. Balanced keeps the relaxed tick.
        let quiet_tick = Duration::from_millis(if gdr || game { 50 } else { 250 });
        match lane.frames.recv_timeout(quiet_tick) {
            Ok(mut frame) => {
                let frame_start = Instant::now();
                got_any = true;
                reporter.report(VideoStatusState::Ok, None);
                // Freshest-wins: displaced frames' slots go straight home.
                while let Ok(newer) = lane.frames.try_recv() {
                    stats.dropped += 1;
                    let _ = lane.release.try_send(frame.slot);
                    frame = newer;
                }
                // The receiver's edge cap may have moved (auto-adapt); the
                // blt output size is baked into the lane, so a changed fit
                // rebuilds the lane rather than mis-scaling.
                let want = fit_within_even(
                    lane.src_size.0,
                    lane.src_size.1,
                    effective_h264_edge(tune, auto),
                );
                if want != lane.out_size {
                    tracing::info!(
                        "GPU lane for {route_id}: fitted size {}×{} -> {}×{} — rebuilding lane",
                        lane.out_size.0,
                        lane.out_size.1,
                        want.0,
                        want.1
                    );
                    return GpuEnd::Restart;
                }
                quiesced = false;
                refines_left = 0;
                refine_at = None;
                stats.add_scale(frame.spent);
                // M1: how stale the pixels are as encoding begins.
                if let Some(presented) = frame.presented {
                    stats.add_age(presented.elapsed());
                }
                (stats.out_w, stats.out_h) = (frame.out_w, frame.out_h);
                let refresh_asked = refresh.swap(false, Ordering::SeqCst);
                let wave_frames = wave.swap(0, Ordering::SeqCst);
                if gdr && wave_frames > 0 {
                    match &mut enc {
                        GpuCodec::Nvenc(n) => n.arm_wave(wave_frames),
                        // AMF's rolling refresh is continuous — the
                        // requested wave is already in flight by
                        // construction; consuming the flag (no IDR
                        // fallback) is the correct heal.
                        GpuCodec::Amf(_) => {}
                        GpuCodec::Mf(_) => {}
                    }
                }
                // Closed-loop bitrate: apply the controller's target in
                // place. A rung that can't move (MF partial coverage
                // mid-heal) pins the target back so the controller never
                // chases a knob wired to nothing.
                let want_rate = rate.target.load(Ordering::Relaxed);
                if want_rate != applied_rate && want_rate > 0 && !lossless_active {
                    if enc.set_bitrate(want_rate) {
                        tracing::info!(
                            "{}: rate re-aimed {:.1} → {:.1} Mbps in place for {route_id}",
                            enc.label(),
                            applied_rate as f64 / 1e6,
                            want_rate as f64 / 1e6,
                        );
                        applied_rate = want_rate;
                    } else {
                        rate.target.store(applied_rate, Ordering::Relaxed);
                    }
                }
                let idr_every = Duration::from_millis(idr_ms.load(Ordering::Relaxed));
                let force_idr = refresh_asked
                    || (!gdr && last_idr.is_none_or(|idr| idr.elapsed() >= idr_every));
                let t1 = Instant::now();
                match enc.encode_texture(&frame.tex, force_idr) {
                    Ok(outcome) => {
                        stats.add_encode(t1.elapsed());
                        let consumed = outcome.consumed;
                        // Keep the submitted source in the same bounded
                        // retirement deque even when the encoder reports it
                        // unconsumed. A quiet tick can then re-offer those
                        // exact pixels; a newer capture naturally supersedes
                        // it at the back of the deque.
                        retain_gpu_frame(&mut retained, frame, &enc, &lane.release);
                        if !consumed {
                            // Nothing served this input (including a forced
                            // refresh), so retry until it is consumed or a
                            // newer capture supersedes these pixels.
                            refresh.store(true, Ordering::SeqCst);
                        }
                        if outcome.units.is_empty() {
                            if consumed && !units_ever {
                                consumed_without_unit += 1;
                                if consumed_without_unit > 90 {
                                    return GpuEnd::Fallback(format!(
                                        "{} consumed {consumed_without_unit} frames without \
                                         producing a unit",
                                        enc.label()
                                    ));
                                }
                            }
                        } else {
                            units_ever = true;
                        }
                        if noise_guard_armed {
                            let frame_bytes: usize =
                                outcome.units.iter().map(|(d, _)| d.len()).sum();
                            noise_window.push_back(frame_bytes > noise_frame_bytes);
                            if noise_window.len() > 120 {
                                noise_window.pop_front();
                            }
                            noise_recent.push_back(frame_bytes);
                            if noise_recent.len() > 18 {
                                noise_recent.pop_front();
                            }
                            let burst = noise_recent.len() == 18
                                && noise_recent.iter().sum::<usize>() > noise_burst_bytes;
                            if burst
                                || (noise_window.len() == 120
                                    && noise_window.iter().filter(|&&over| over).count() >= 90)
                            {
                                noise_window.clear();
                                noise_guard_armed = false;
                                tracing::warn!(
                                    "lossless noise guard for {route_id}: sustained \
                                     incompressible content — swapping to lossy studio in place"
                                );
                                match crate::nvenc::NvencH264::open_on_device(
                                    &lane.device,
                                    dw,
                                    dh,
                                    fps,
                                    bitrate,
                                    false,
                                    true,
                                ) {
                                    Ok(n) => {
                                        // The codec morphs HEVC→H.264 on the
                                        // wire; the viewer's bridge re-sniffs
                                        // at the forced IDR and rebuilds its
                                        // decoder — the same seam a posture
                                        // retune crosses.
                                        enc = GpuCodec::Nvenc(n);
                                        lossless_active = false;
                                        register_rate(&mut rate_registered);
                                        refresh.store(true, Ordering::SeqCst);
                                    }
                                    Err(e) => {
                                        // Couldn't degrade: keep streaming
                                        // lossless (correct, just heavy) and
                                        // let the window re-arm for another
                                        // try.
                                        tracing::warn!(
                                            "noise-guard studio reopen failed ({e}); staying lossless"
                                        );
                                        noise_guard_armed = true;
                                    }
                                }
                            }
                        }
                        let packets = packetize_units(
                            outcome.units,
                            fps,
                            &mut last_emit,
                            &mut last_idr,
                            stats,
                        );
                        emit_packets(packets, on_packet, stats);
                    }
                    Err(e) => {
                        // Preserve the final damaged picture before any heal
                        // attempt. A successful MF reopen can re-offer it on
                        // the next quiet tick instead of waiting for damage
                        // capture to produce pixels that may never recur.
                        retain_gpu_frame(&mut retained, frame, &enc, &lane.release);
                        refresh.store(true, Ordering::SeqCst);
                        match gpu_heal(&mut policy, route_id, &e, || {
                            open_gpu_encoder(
                                lane.adapter_luid,
                                dw,
                                dh,
                                fps,
                                bitrate,
                                game,
                                &lane.manager,
                            )
                        }) {
                            Ok(Some(next)) => {
                                // A heal always lands on the MF rung — a
                                // failing SDK session steps down rather
                                // than retrying itself. The MF rung has
                                // no GDR: the periodic-IDR cadence takes
                                // recovery duty back.
                                enc = GpuCodec::Mf(next);
                                disable_gdr_route(route_id, &mut gdr, &wave);
                                lossless_active = false;
                                register_rate(&mut rate_registered);
                                refresh.store(true, Ordering::SeqCst);
                            }
                            Ok(None) => {}
                            Err(why) => return GpuEnd::Fallback(why),
                        }
                    }
                }
                stats.maybe_log();
                if let Some(rest) = budget.checked_sub(frame_start.elapsed()) {
                    std::thread::sleep(rest);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !got_any {
                    // Frameless from birth = a dark display (damage-driven
                    // capture sends nothing from a sleeping screen). Keep
                    // wake pressure on the panel, like the CPU pump.
                    wake::force_display_on();
                    if started.elapsed() >= FIRST_FRAME_STALL {
                        reporter.report(VideoStatusState::DisplayAsleep, None);
                    }
                    continue;
                }
                // Quiet spell: serve convergence from the newest retained
                // texture — one clean IDR per spell (plus any refresh ask
                // that lands while quiet), then the refinement passes
                // sharpen what the burst left coarse (the CPU pump's twin).
                let Some(kept) = retained.back() else {
                    continue;
                };
                if !quiesced || refresh.load(Ordering::SeqCst) {
                    quiesced = true;
                    let refresh_asked = refresh.load(Ordering::SeqCst);
                    let t1 = Instant::now();
                    match enc.encode_texture(&kept.tex, true) {
                        Ok(outcome) => {
                            stats.add_encode(t1.elapsed());
                            if outcome.consumed {
                                if refresh_asked {
                                    refresh.store(false, Ordering::SeqCst);
                                }
                            } else {
                                // The retained texture is still the newest
                                // source. Keep asking until this exact input
                                // is consumed or a capture supersedes it.
                                refresh.store(true, Ordering::SeqCst);
                            }
                            let consumed = outcome.consumed;
                            let emitted = !outcome.units.is_empty();
                            let packets = packetize_units(
                                outcome.units,
                                fps,
                                &mut last_emit,
                                &mut last_idr,
                                stats,
                            );
                            emit_packets(packets, on_packet, stats);
                            if consumed && emitted {
                                refines_left = REFINE_PASSES;
                                refine_at = Some(Instant::now() + REFINE_AFTER);
                            }
                        }
                        Err(e) => {
                            // The source is already retained; guarantee the
                            // healed (or delayed-rebuild) encoder re-offers it.
                            refresh.store(true, Ordering::SeqCst);
                            match gpu_heal(&mut policy, route_id, &e, || {
                                open_gpu_encoder(
                                    lane.adapter_luid,
                                    dw,
                                    dh,
                                    fps,
                                    bitrate,
                                    game,
                                    &lane.manager,
                                )
                            }) {
                                Ok(Some(next)) => {
                                    enc = GpuCodec::Mf(next);
                                    disable_gdr_route(route_id, &mut gdr, &wave);
                                    lossless_active = false;
                                    register_rate(&mut rate_registered);
                                    refresh.store(true, Ordering::SeqCst);
                                }
                                Ok(None) => {}
                                Err(why) => return GpuEnd::Fallback(why),
                            }
                        }
                    }
                    stats.maybe_log();
                } else if refines_left > 0 && refine_at.is_some_and(|t| Instant::now() >= t) {
                    let t1 = Instant::now();
                    match enc.encode_texture(&kept.tex, false) {
                        Ok(outcome) => {
                            stats.add_encode(t1.elapsed());
                            let consumed = outcome.consumed;
                            if !consumed {
                                refresh.store(true, Ordering::SeqCst);
                            }
                            let packets = packetize_units(
                                outcome.units,
                                fps,
                                &mut last_emit,
                                &mut last_idr,
                                stats,
                            );
                            emit_packets(packets, on_packet, stats);
                            if consumed {
                                refines_left -= 1;
                                refine_at =
                                    (refines_left > 0).then(|| Instant::now() + REFINE_SPACING);
                            }
                        }
                        Err(e) => {
                            refresh.store(true, Ordering::SeqCst);
                            match gpu_heal(&mut policy, route_id, &e, || {
                                open_gpu_encoder(
                                    lane.adapter_luid,
                                    dw,
                                    dh,
                                    fps,
                                    bitrate,
                                    game,
                                    &lane.manager,
                                )
                            }) {
                                Ok(Some(next)) => {
                                    enc = GpuCodec::Mf(next);
                                    disable_gdr_route(route_id, &mut gdr, &wave);
                                    lossless_active = false;
                                    register_rate(&mut rate_registered);
                                    refresh.store(true, Ordering::SeqCst);
                                }
                                Ok(None) => {}
                                Err(why) => return GpuEnd::Fallback(why),
                            }
                        }
                    }
                    stats.maybe_log();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // The capture thread ended: a mode change (the restart
                // builds the new geometry) or an unrecoverable duplication
                // (the restart's `start_gpu` error then routes to the CPU
                // path, whose fallback ladder owns the reporting).
                return if stop.load(Ordering::SeqCst) {
                    GpuEnd::Stopped
                } else {
                    GpuEnd::Restart
                };
            }
        }
    }
}

/// Stream from xcap's persistent AVFoundation capture session. Set-up
/// happens once; frames arrive as the OS produces them — damage-driven
/// backends send nothing while the screen is still. (Wayland rides
/// `wayland_capture` instead; X11 prefers the paced one-shot loop.)
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
fn run_session_capture(
    stop: &AtomicBool,
    fps: u32,
    route_id: &str,
    monitor: &xcap::Monitor,
    on_packet: &(dyn Fn(VideoPacket) -> bool + Send + Sync),
    encoder: &mut HealingEncoder,
    stats: &mut StreamStats,
    reporter: &mut StatusReporter,
) -> Result<(), String> {
    let (recorder, frames) = monitor.video_recorder().map_err(|e| e.to_string())?;
    recorder.start().map_err(|e| e.to_string())?;
    tracing::info!("screen capture session started for {route_id}");
    let result = pump_frames(
        stop,
        fps,
        &frames,
        // AVFoundation delivers the upright presentation image.
        move |f| orient_to_monitor(f.raw, f.width, f.height, 0),
        on_packet,
        encoder,
        stats,
        reporter,
        None,
    );
    let _ = recorder.stop();
    result
}

/// One screenshot per tick — the X11 path and the universal fallback.
/// Every grab pays the platform's full one-shot cost, so the effective
/// rate is whatever that path allows; the encoder's unchanged-frame gate
/// at least makes idle screens cheap to *send*. When `retry_capture_after`
/// is set, the loop returns `Ok(true)` once that long has passed — the
/// caller's cue to re-attempt a real capture session; `Ok(false)` means the
/// route stopped.
#[allow(clippy::too_many_arguments, unused_mut)]
fn run_oneshot_capture(
    stop: &AtomicBool,
    fps: u32,
    route_id: &str,
    requested_monitor_id: Option<u32>,
    mut monitor: xcap::Monitor,
    on_packet: &(dyn Fn(VideoPacket) -> bool + Send + Sync),
    encoder: &mut HealingEncoder,
    stats: &mut StreamStats,
    reporter: &mut StatusReporter,
    retry_capture_after: Option<Duration>,
) -> Result<bool, String> {
    #[cfg(not(windows))]
    let _ = requested_monitor_id;
    let budget = Duration::from_secs(1) / fps.max(1);
    let began = Instant::now();
    let mut failures = 0u64;
    #[cfg(windows)]
    let stable_name = requested_monitor_id
        .and_then(|_| monitor.name().ok())
        .filter(|name| !name.trim().is_empty());
    while !stop.load(Ordering::SeqCst) {
        if let Some(after) = retry_capture_after {
            if began.elapsed() >= after {
                return Ok(true);
            }
        }
        let started = Instant::now();
        // Grab failures and encoder failures are different conditions with
        // different fates: a failing grab (screen lock, denied permission)
        // loops in hope, while an encoder the healer gave up on ends the
        // stream — looping full-rate screenshots into a dead encoder is the
        // zombie-stream failure this used to produce.
        match monitor.capture_image() {
            Ok(image) => {
                let (sw, sh) = (image.width(), image.height());
                // capture_image (X11 grab, Windows GDI/WGC fallback) is upright.
                let (rgba, sw, sh) = orient_to_monitor(image.into_raw(), sw, sh, 0);
                match encoder.encode(rgba, sw, sh, stats) {
                    Ok(packets) => {
                        failures = 0;
                        reporter.report(VideoStatusState::Ok, None);
                        emit_packets(packets, on_packet, stats);
                    }
                    Err(e) => {
                        reporter.report(VideoStatusState::GrabFailed, Some(e.clone()));
                        return Err(e);
                    }
                }
            }
            Err(e) => {
                let e = e.to_string();
                #[cfg(windows)]
                if invalid_monitor_handle(&e) {
                    let old_id = monitor.id().ok();
                    let started_reacquire = Instant::now();
                    tracing::warn!(
                        "screen grab for {route_id} lost monitor handle {:?} ({e}); re-enumerating{}",
                        old_id,
                        stable_name
                            .as_deref()
                            .map(|name| format!(" {name}"))
                            .unwrap_or_default()
                    );
                    loop {
                        if stop.load(Ordering::SeqCst) {
                            return Ok(false);
                        }
                        if retry_capture_after.is_some_and(|after| began.elapsed() >= after) {
                            return Ok(true);
                        }
                        std::thread::sleep(Duration::from_millis(250));
                        match reacquire_monitor(requested_monitor_id, stable_name.as_deref()) {
                            Ok(fresh) => {
                                let new_id = fresh.id().ok();
                                tracing::info!(
                                    "screen grab for {route_id} rebound monitor {:?} -> {:?} in {:.0} ms{}",
                                    old_id,
                                    new_id,
                                    started_reacquire.elapsed().as_secs_f64() * 1000.0,
                                    stable_name
                                        .as_deref()
                                        .map(|name| format!(" ({name})"))
                                        .unwrap_or_default()
                                );
                                monitor = fresh;
                                failures = 0;
                                break;
                            }
                            Err(reacquire) => {
                                if failures == 0 || failures.is_multiple_of(16) {
                                    tracing::warn!(
                                        "screen grab for {route_id} waiting for monitor reacquisition: {reacquire}"
                                    );
                                }
                                failures = failures.saturating_add(1);
                                reporter.report(VideoStatusState::NoMonitor, Some(reacquire));
                            }
                        }
                    }
                    continue;
                }
                // A transient grab failure (screen lock, monitor sleep)
                // shouldn't end the stream — but a *persistent* one (a
                // denied screen-recording permission, a Wayland portal
                // that never granted) must be loud, not a debug whisper:
                // it reads as "connected but no pixels" at the far end.
                // The viewer hears it too, in-band — and the panel keeps
                // getting wake pressure in case sleep is the whole story.
                wake::force_display_on();
                failures += 1;
                if failures == 1 || failures.is_multiple_of(100) {
                    tracing::warn!("screen grab failing for {route_id} ({failures}x): {e}");
                } else {
                    tracing::debug!("screen grab failed for {route_id}: {e}");
                }
                reporter.report(VideoStatusState::GrabFailed, Some(e));
            }
        }
        stats.maybe_log();
        if let Some(rest) = budget.checked_sub(started.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    Ok(false)
}

#[cfg(windows)]
fn invalid_monitor_handle(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("0x800705b5") || error.contains("invalid monitor handle")
}

/// Re-enumerate a Windows monitor after its raw `HMONITOR` was invalidated.
/// The synthetic primary source follows the current primary. An explicit
/// secondary source stays bound to its `\\.\DISPLAYn` device name; showing a
/// different monitor is a privacy/correctness failure, so absence is loud.
#[cfg(windows)]
fn reacquire_monitor(
    requested_monitor_id: Option<u32>,
    stable_name: Option<&str>,
) -> Result<xcap::Monitor, String> {
    match monitor_reacquire_key(requested_monitor_id, stable_name)? {
        MonitorReacquireKey::Primary => select_monitor(None),
        MonitorReacquireKey::DeviceName(name) => {
            let monitors = xcap::Monitor::all().map_err(|e| e.to_string())?;
            monitors
                .into_iter()
                .find(|monitor| {
                    monitor
                        .name()
                        .is_ok_and(|candidate| candidate.eq_ignore_ascii_case(&name))
                })
                .ok_or_else(|| format!("requested monitor {name} is not currently attached"))
        }
    }
}

#[cfg(windows)]
#[derive(Debug, PartialEq, Eq)]
enum MonitorReacquireKey {
    Primary,
    DeviceName(String),
}

/// Select the only privacy-safe identity for monitor reacquisition. A raw
/// `HMONITOR` is deliberately absent from this enum: Windows may recycle it
/// for another output after topology churn, so falling back to the stale
/// number could expose the wrong display.
#[cfg(windows)]
fn monitor_reacquire_key(
    requested_monitor_id: Option<u32>,
    stable_name: Option<&str>,
) -> Result<MonitorReacquireKey, String> {
    if requested_monitor_id.is_none() {
        return Ok(MonitorReacquireKey::Primary);
    }
    stable_name
        .filter(|name| !name.trim().is_empty())
        .map(|name| MonitorReacquireKey::DeviceName(name.to_string()))
        .ok_or_else(|| {
            "requested monitor lost its stable display name; refusing stale-handle fallback"
                .to_string()
        })
}

/// The monitor a capture should run on: by enumeration id when the route
/// names one, else the primary. A named monitor that's gone (unplugged
/// since the scan) is an error: silently substituting the primary would show
/// the wrong display and can expose pixels the viewer never selected.
fn select_monitor(monitor_id: Option<u32>) -> Result<xcap::Monitor, String> {
    let monitors = xcap::Monitor::all().map_err(|e| e.to_string())?;
    if let Some(id) = monitor_id {
        for m in &monitors {
            if m.id().ok() == Some(id) {
                return Ok(m.clone());
            }
        }
        return Err(format!(
            "monitor {id:#x} not found (unplugged or topology changed)"
        ));
    }
    let mut first = None;
    for m in monitors {
        if m.is_primary().unwrap_or(false) {
            return Ok(m);
        }
        first.get_or_insert(m);
    }
    first.ok_or_else(|| "no monitor to capture".to_string())
}

/// Every monitor beyond the primary, as `(id, label)` for the bridge's
/// per-monitor `screen:<id>` capabilities — so a multi-monitor machine
/// gets one console tab per screen. The primary stays the synthetic
/// `screen` capability (and the universal fallback), so a single-monitor
/// machine advertises exactly what it did before. Ids are xcap's own
/// enumeration ids: stable for this app run, and resolved back to the
/// same monitor by [`select_monitor`] when a route starts.
pub fn extra_screens() -> Vec<allmystuff_bridge::ScreenSource> {
    let Ok(monitors) = xcap::Monitor::all() else {
        return Vec::new();
    };
    // Mirror select_monitor's primary choice (first flagged, else first
    // listed) so the two views of "which one is the primary" can't drift.
    let primary = monitors
        .iter()
        .position(|m| m.is_primary().unwrap_or(false))
        .unwrap_or(0);
    monitors
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != primary)
        .filter_map(|(i, m)| {
            let id = m.id().ok()?;
            // Windows reports device paths (`\\.\DISPLAY2`); strip the
            // prefix so the tab reads as a name, not an escape sequence.
            let name = m.name().unwrap_or_default();
            let name = name.trim_start_matches(r"\\.\").trim();
            let label = if name.is_empty() {
                format!("Screen {}", i + 1)
            } else {
                format!("Screen — {name}")
            };
            Some(allmystuff_bridge::ScreenSource { id, label })
        })
        .collect()
}

/// The downscale + JPEG stage of one route's stream, with the
/// unchanged-frame gate: pixels identical to the last send (compared
/// post-downscale, where the buffer is small) return `None` instead of
/// burning an encode on a picture the viewer already has — refreshed every
/// [`STATIC_REFRESH`] regardless. Owns the stream's `seq`, which counts
/// *sent* frames (the receiver logs its first frame as `seq == 0`).
struct FrameEncoder {
    route_id: String,
    seq: u64,
    prev: Vec<u8>,
    prev_size: (u32, u32),
    last_sent: Option<Instant>,
    max_edge: u32,
    /// JPEG quality this stream encodes at — the Rate pill, mapped through
    /// [`Tune::jpeg_quality`] (MJPEG's stand-in for a bitrate).
    quality: u8,
    /// The route's one-shot "resend now" flag (a viewer asked).
    refresh: Arc<AtomicBool>,
    /// The receiver-driven resolution cap — read per frame, min'd with
    /// `max_edge` (see [`AutoAdapt`]).
    auto: Arc<AutoAdapt>,
}

impl FrameEncoder {
    fn new(route_id: &str, tune: Tune, refresh: Arc<AtomicBool>, auto: Arc<AutoAdapt>) -> Self {
        FrameEncoder {
            route_id: route_id.to_string(),
            seq: 0,
            prev: Vec::new(),
            prev_size: (0, 0),
            last_sent: None,
            max_edge: tune.mjpeg_edge(),
            quality: tune.jpeg_quality(),
            refresh,
            auto,
        }
    }

    fn encode(
        &mut self,
        rgba: Vec<u8>,
        sw: u32,
        sh: u32,
        stats: &mut StreamStats,
    ) -> Result<Option<VideoFrame>, String> {
        let edge = match self.auto.edge_cap() {
            Some(cap) => self.max_edge.min(cap),
            None => self.max_edge,
        };
        let (dw, dh) = fit_within(sw, sh, edge);
        let t0 = Instant::now();
        let scaled = if (dw, dh) == (sw, sh) {
            rgba
        } else {
            scale_rgba(&rgba, sw, sh, dw, dh)
        };
        stats.add_scale(t0.elapsed());
        (stats.out_w, stats.out_h) = (dw, dh);
        let refresh_due = self.refresh.swap(false, Ordering::SeqCst)
            || self
                .last_sent
                .is_none_or(|sent| sent.elapsed() >= STATIC_REFRESH);
        if !refresh_due && self.prev_size == (dw, dh) && self.prev == scaled {
            stats.static_skipped += 1;
            return Ok(None);
        }
        let t1 = Instant::now();
        let jpeg = encode_jpeg(&scaled, dw, dh, self.quality)?;
        stats.add_encode(t1.elapsed());
        stats.bytes += jpeg.len() as u64;
        stats.keyframes += 1; // every MJPEG frame is standalone
        self.prev = scaled;
        self.prev_size = (dw, dh);
        self.last_sent = Some(Instant::now());
        let frame = VideoFrame::new(&self.route_id, self.seq, dw, dh, sw, sh, jpeg);
        self.seq += 1;
        Ok(Some(frame))
    }

    /// Re-encode the retained last picture. The pump's rescue for a screen
    /// that has gone QUIET: change-driven capture delivers nothing, so
    /// `encode` — and with it [`STATIC_REFRESH`] and any viewer refresh
    /// ask — never runs. Consumes a pending ask; emits at most one frame.
    fn re_emit(&mut self, stats: &mut StreamStats) -> Result<Option<VideoFrame>, String> {
        if self.prev.is_empty() || self.prev_size == (0, 0) {
            return Ok(None);
        }
        self.refresh.store(false, Ordering::SeqCst);
        let (dw, dh) = self.prev_size;
        let t1 = Instant::now();
        let jpeg = encode_jpeg(&self.prev, dw, dh, self.quality)?;
        stats.add_encode(t1.elapsed());
        stats.bytes += jpeg.len() as u64;
        stats.keyframes += 1;
        self.last_sent = Some(Instant::now());
        let frame = VideoFrame::new(&self.route_id, self.seq, dw, dh, dw, dh, jpeg);
        self.seq += 1;
        Ok(Some(frame))
    }
}

/// How many encoder rebuilds a stream may spend inside one
/// [`REBUILD_WINDOW`] before the healer gives up and ends the route.
const REBUILD_MAX: u32 = 3;
/// The window rebuild attempts are counted in — long enough that a
/// once-in-a-while driver hiccup never exhausts the budget, short enough
/// that a truly dead encoder stops burning rebuild cycles.
const REBUILD_WINDOW: Duration = Duration::from_secs(120);
/// Minimum spacing between rebuilds. A GPU mid-TDR needs a beat before a
/// fresh session can succeed; the frames in between are skipped (not
/// blocked on), keeping the capture thread responsive to stop.
const REBUILD_SPACING: Duration = Duration::from_secs(2);

/// What to do about one encoder error — pure state, unit-testable.
struct RebuildPolicy {
    window_start: Option<Instant>,
    rebuilds_in_window: u32,
    last_rebuild: Option<Instant>,
}

enum PolicyVerdict {
    /// Rebuild the encoder now.
    Rebuild,
    /// Too soon since the last rebuild — skip this frame, retry next tick.
    SkipFrame,
    /// The budget is spent — the stream ends with the error.
    GiveUp,
}

impl RebuildPolicy {
    fn new() -> Self {
        RebuildPolicy {
            window_start: None,
            rebuilds_in_window: 0,
            last_rebuild: None,
        }
    }

    fn on_error(&mut self, now: Instant) -> PolicyVerdict {
        if self
            .window_start
            .is_none_or(|t| now.duration_since(t) > REBUILD_WINDOW)
        {
            self.window_start = Some(now);
            self.rebuilds_in_window = 0;
        }
        if self.rebuilds_in_window >= REBUILD_MAX {
            return PolicyVerdict::GiveUp;
        }
        if self
            .last_rebuild
            .is_some_and(|t| now.duration_since(t) < REBUILD_SPACING)
        {
            return PolicyVerdict::SkipFrame;
        }
        self.rebuilds_in_window += 1;
        self.last_rebuild = Some(now);
        PolicyVerdict::Rebuild
    }
}

/// [`StreamEncoder`] with self-healing: a backend error mid-stream rebuilds
/// the codec through the full ladder (falling to the MJPEG floor if H.264
/// can't come back) instead of ending — or worse, zombifying — the route.
/// The failure this buries: one transient MFT/driver error (a TDR under GPU
/// load, a driver update) used to demote the stream to per-frame GDI
/// screenshots fed into a permanently dead encoder, looping `GrabFailed`
/// forever while the viewer stared at a frozen frame. Rebuilds are budgeted
/// by [`RebuildPolicy`]; a stream that exhausts the budget ends with the
/// error, which the pumps surface to the viewer as a capture failure.
struct HealingEncoder {
    enc: StreamEncoder,
    route_id: String,
    mode: VideoMode,
    tune: Tune,
    refresh: Arc<AtomicBool>,
    idr_ms: Arc<AtomicU64>,
    auto: Arc<AutoAdapt>,
    policy: RebuildPolicy,
    /// Test seam: replaces the ladder rebuild so recovery is testable
    /// without hardware.
    #[cfg(test)]
    rebuild_override: Option<Box<dyn FnMut() -> Result<StreamEncoder, String> + Send>>,
}

impl HealingEncoder {
    #[allow(clippy::too_many_arguments)]
    fn new(
        route_id: &str,
        mode: VideoMode,
        source_hint: (u32, u32),
        tune: Tune,
        refresh: &Arc<AtomicBool>,
        idr_ms: &Arc<AtomicU64>,
        auto: &Arc<AutoAdapt>,
    ) -> Result<Self, String> {
        let enc = make_encoder(route_id, mode, source_hint, tune, refresh, idr_ms, auto)?;
        // Name the CPU pipeline for the effective-dials panel (the actual
        // encoder after any MJPEG-floor fallback). The GPU lane, when it
        // wins, set "GPU (hardware)" before this constructor was ever reached
        // on the fallback path.
        *route_live_cell(route_id).encoder.lock() = match enc.mode() {
            VideoMode::H264 => "H.264 (CPU)".to_string(),
            VideoMode::Mjpeg => "MJPEG (CPU)".to_string(),
        };
        Ok(HealingEncoder {
            enc,
            route_id: route_id.to_string(),
            mode,
            tune,
            refresh: refresh.clone(),
            idr_ms: idr_ms.clone(),
            auto: auto.clone(),
            policy: RebuildPolicy::new(),
            #[cfg(test)]
            rebuild_override: None,
        })
    }

    /// The transport the *current* encoder produces (a rebuild may have
    /// landed on the MJPEG floor).
    fn mode(&self) -> VideoMode {
        self.enc.mode()
    }

    fn wants_refresh(&self) -> bool {
        self.enc.wants_refresh()
    }

    fn encode(
        &mut self,
        rgba: Vec<u8>,
        sw: u32,
        sh: u32,
        stats: &mut StreamStats,
    ) -> Result<Vec<VideoPacket>, String> {
        match self.enc.encode(rgba, sw, sh, stats) {
            Ok(packets) => Ok(packets),
            Err(e) => {
                self.heal(&e, (sw, sh))?;
                Ok(Vec::new())
            }
        }
    }

    fn re_emit(
        &mut self,
        stats: &mut StreamStats,
        force_idr: bool,
    ) -> Result<Vec<VideoPacket>, String> {
        match self.enc.re_emit(stats, force_idr) {
            Ok(packets) => Ok(packets),
            Err(e) => {
                self.heal(&e, (0, 0))?;
                Ok(Vec::new())
            }
        }
    }

    /// [`StreamEncoder::encode_staged`] with the healing wrap — the
    /// pipelined pump's encode-side entry point.
    fn encode_staged(
        &mut self,
        staged: Staged,
        stats: &mut StreamStats,
        recycle: &mut dyn FnMut(Vec<u8>),
    ) -> Result<Vec<VideoPacket>, String> {
        let hint = (staged.sw, staged.sh);
        match self.enc.encode_staged(staged, stats, recycle) {
            Ok(packets) => Ok(packets),
            Err(e) => {
                self.heal(&e, hint)?;
                Ok(Vec::new())
            }
        }
    }

    /// See [`StreamEncoder::current_needs`] — republished by the encode
    /// side after every encode so the capture side tracks rebuilds.
    fn current_needs(&self) -> (Option<YuvFormat>, u32) {
        self.enc.current_needs()
    }

    /// One encoder error: rebuild, skip, or give up per the policy. `Ok(())`
    /// means the stream carries on (this frame is simply dropped); `Err`
    /// ends the route with the reason.
    fn heal(&mut self, err: &str, source_hint: (u32, u32)) -> Result<(), String> {
        match self.policy.on_error(Instant::now()) {
            PolicyVerdict::SkipFrame => {
                tracing::debug!(
                    "encoder for {} still failing ({err}); next rebuild not yet due",
                    self.route_id
                );
                Ok(())
            }
            PolicyVerdict::GiveUp => Err(format!(
                "encoder for {} failed permanently after {REBUILD_MAX} rebuilds: {err}",
                self.route_id
            )),
            PolicyVerdict::Rebuild => {
                tracing::warn!(
                    "encoder for {} failed ({err}); rebuilding through the ladder",
                    self.route_id
                );
                self.enc = self.rebuild(source_hint)?;
                // A fresh encoder's first unit is an IDR; arm the refresh
                // flag so the quiet path serves one promptly too.
                self.refresh.store(true, Ordering::SeqCst);
                Ok(())
            }
        }
    }

    fn rebuild(&mut self, source_hint: (u32, u32)) -> Result<StreamEncoder, String> {
        #[cfg(test)]
        if let Some(f) = &mut self.rebuild_override {
            return f();
        }
        // The negotiated transport is pinned for the route's life: a heal
        // rebuilds the SAME mode or fails the route (which then restarts
        // and renegotiates). The old path went through [`make_encoder`],
        // whose MJPEG floor could morph a healing H.264 route into
        // chunked MJPEG mid-stream — a quality lurch the viewer never
        // asked for, and (Chris's field read) chunk-loss artifacts that
        // present as tearing. The floor still applies at route START,
        // where the viewer can see what it's getting from frame one.
        StreamEncoder::new(
            &self.route_id,
            self.mode,
            source_hint,
            self.tune,
            &self.refresh,
            &self.idr_ms,
            &self.auto,
        )
    }
}

/// One route's encoder for the negotiated transport — with the rule that
/// an encoder that can't init (openh264 build/runtime trouble) must cost
/// quality, not the stream: fall back to MJPEG and say so.
#[allow(clippy::too_many_arguments)]
fn make_encoder(
    route_id: &str,
    mode: VideoMode,
    source_hint: (u32, u32),
    tune: Tune,
    refresh: &Arc<AtomicBool>,
    idr_ms: &Arc<AtomicU64>,
    auto: &Arc<AutoAdapt>,
) -> Result<StreamEncoder, String> {
    match StreamEncoder::new(route_id, mode, source_hint, tune, refresh, idr_ms, auto) {
        Ok(enc) => Ok(enc),
        Err(e) => {
            tracing::warn!("encoder for {route_id} unavailable ({e}); falling back to MJPEG");
            StreamEncoder::new(
                route_id,
                VideoMode::Mjpeg,
                source_hint,
                tune,
                refresh,
                idr_ms,
                auto,
            )
        }
    }
}

/// A capture-side-prepared frame headed for the encode stage: raw RGBA for
/// the MJPEG floor (which scales/encodes internally), or a 4:2:0 buffer
/// converted to the layout the encode side advertised via
/// [`HealingEncoder::current_needs`].
enum Prepared {
    Rgba(Vec<u8>),
    Yuv(YuvFormat, Vec<u8>),
}

/// One staged frame crossing the pipelined pump's channel — the seam that
/// lets the capture side convert frame N while the encode side encodes
/// frame N−1.
struct Staged {
    frame: Prepared,
    /// Prepared-buffer dims (fitted for `Yuv`; the raw capture dims for
    /// `Rgba`).
    dw: u32,
    dh: u32,
    /// Raw capture dims.
    sw: u32,
    sh: u32,
    /// Time the capture side spent converting — accounted into the encode
    /// side's stats, which own the dial-in log line.
    scale_spent: Duration,
}

/// The wire encoding of [`HealingEncoder::current_needs`]'s layout half for
/// the producer-visible atomic: 0 = raw RGBA, 1 = I420, 2 = NV12.
fn stage_need_to_u8(need: Option<YuvFormat>) -> u8 {
    match need {
        None => 0,
        Some(YuvFormat::I420) => 1,
        Some(YuvFormat::Nv12) => 2,
    }
}

fn stage_need_from_u8(v: u8) -> Option<YuvFormat> {
    match v {
        1 => Some(YuvFormat::I420),
        2 => Some(YuvFormat::Nv12),
        _ => None,
    }
}

/// The per-route encode stage, dispatching on the negotiated transport.
/// (The H.264 arm boxes openh264's chunky encoder state.)
enum StreamEncoder {
    Mjpeg(FrameEncoder),
    H264(Box<H264Stream>),
}

impl StreamEncoder {
    /// The transport this encoder actually produces (the negotiated one,
    /// or the MJPEG floor [`make_encoder`] fell back to).
    fn mode(&self) -> VideoMode {
        match self {
            StreamEncoder::Mjpeg(_) => VideoMode::Mjpeg,
            StreamEncoder::H264(_) => VideoMode::H264,
        }
    }

    /// `source_hint` is the capture source's expected resolution (the
    /// monitor's), which pre-budgets the H.264 bitrate; `(0, 0)` =
    /// unknown. The real budget locks to the first frame's true fitted
    /// size either way (logical-vs-physical monitor reports differ).
    #[allow(clippy::too_many_arguments)]
    fn new(
        route_id: &str,
        mode: VideoMode,
        source_hint: (u32, u32),
        tune: Tune,
        refresh: &Arc<AtomicBool>,
        idr_ms: &Arc<AtomicU64>,
        auto: &Arc<AutoAdapt>,
    ) -> Result<Self, String> {
        match mode {
            // MJPEG is stateless — no keyframes — so the adaptive IDR cadence
            // doesn't apply; only the H.264 stream reads `idr_ms`.
            VideoMode::Mjpeg => Ok(StreamEncoder::Mjpeg(FrameEncoder::new(
                route_id,
                tune,
                refresh.clone(),
                auto.clone(),
            ))),
            VideoMode::H264 => Ok(StreamEncoder::H264(Box::new(H264Stream::new(
                source_hint,
                tune,
                refresh.clone(),
                idr_ms.clone(),
                auto.clone(),
            )?))),
        }
    }

    fn encode(
        &mut self,
        rgba: Vec<u8>,
        sw: u32,
        sh: u32,
        stats: &mut StreamStats,
    ) -> Result<Vec<VideoPacket>, String> {
        // A backend that hands over fewer bytes than its stated dimensions
        // imply must cost an error, not an out-of-bounds panic downstream —
        // the release profile is `panic = "abort"`, so a panic on this
        // thread kills the whole node. The healer turns this into a skipped
        // frame / rebuild / clean route end instead.
        let need = (sw as usize) * (sh as usize) * 4;
        if rgba.len() < need {
            return Err(format!(
                "capture frame too short: {} bytes for {sw}x{sh} RGBA (need {need})",
                rgba.len()
            ));
        }
        match self {
            StreamEncoder::Mjpeg(enc) => Ok(enc
                .encode(rgba, sw, sh, stats)?
                .map(VideoPacket::Jpeg)
                .into_iter()
                .collect()),
            StreamEncoder::H264(enc) => enc.encode(rgba, sw, sh, stats),
        }
    }

    /// Whether a viewer's one-shot refresh ask is pending — peeked, not
    /// consumed (`encode`/`re_emit` consume it). Lets the pump serve an ask
    /// that lands while the screen is quiet and no frames reach `encode`.
    fn wants_refresh(&self) -> bool {
        match self {
            StreamEncoder::Mjpeg(enc) => enc.refresh.load(Ordering::SeqCst),
            StreamEncoder::H264(enc) => enc.refresh.load(Ordering::SeqCst),
        }
    }

    /// Re-emit the retained last picture — with `force_idr` the pump's
    /// convergence rescue for a quiet screen (a clean IDR on H.264, a plain
    /// resend on MJPEG), without it a post-quiesce quality-refinement pass
    /// (H.264 only — re-encoding identical pixels to JPEG produces the same
    /// bytes, so the MJPEG arm only answers the forced form).
    fn re_emit(
        &mut self,
        stats: &mut StreamStats,
        force_idr: bool,
    ) -> Result<Vec<VideoPacket>, String> {
        match self {
            StreamEncoder::Mjpeg(enc) => {
                if !force_idr {
                    return Ok(Vec::new());
                }
                Ok(enc
                    .re_emit(stats)?
                    .map(VideoPacket::Jpeg)
                    .into_iter()
                    .collect())
            }
            StreamEncoder::H264(enc) => enc.re_emit(stats, force_idr),
        }
    }

    /// Encode one capture-side-staged frame. A staging that doesn't match
    /// the current arm (a healing rebuild swapped H.264↔MJPEG, or a format
    /// change raced the producer) drops that one frame — the producer
    /// re-reads [`Self::current_needs`] and the next frame lands right.
    fn encode_staged(
        &mut self,
        staged: Staged,
        stats: &mut StreamStats,
        recycle: &mut dyn FnMut(Vec<u8>),
    ) -> Result<Vec<VideoPacket>, String> {
        match (self, staged.frame) {
            (StreamEncoder::Mjpeg(enc), Prepared::Rgba(rgba)) => {
                let need = (staged.sw as usize) * (staged.sh as usize) * 4;
                if rgba.len() < need {
                    return Err(format!(
                        "capture frame too short: {} bytes for {}x{} RGBA (need {need})",
                        rgba.len(),
                        staged.sw,
                        staged.sh
                    ));
                }
                Ok(enc
                    .encode(rgba, staged.sw, staged.sh, stats)?
                    .map(VideoPacket::Jpeg)
                    .into_iter()
                    .collect())
            }
            (StreamEncoder::H264(enc), Prepared::Yuv(format, yuv)) => {
                enc.encode_prepared(yuv, format, staged.dw, staged.dh, stats, recycle)
            }
            (_, Prepared::Yuv(_, yuv)) => {
                stats.dropped += 1;
                recycle(yuv);
                Ok(Vec::new())
            }
            _ => {
                stats.dropped += 1;
                Ok(Vec::new())
            }
        }
    }

    /// What the capture side should stage next: `(layout, fit edge)` —
    /// `None` = raw RGBA (the MJPEG floor), else the 4:2:0 layout the
    /// backend ingests plus the effective edge to fit to.
    fn current_needs(&self) -> (Option<YuvFormat>, u32) {
        match self {
            StreamEncoder::Mjpeg(_) => (None, 0),
            StreamEncoder::H264(enc) => (Some(enc.codec.input_format()), enc.effective_edge()),
        }
    }
}

/// The H.264 encode stage of one route's stream — openh264 in
/// screen-content mode, scaled to the [`h264_max_edge`] ceiling (even
/// dimensions for 4:2:0, native up to 4K by default), with the same
/// unchanged-frame gate as MJPEG and a forced IDR on an adaptive cadence
/// ([`adaptive_idr_ms`], floored at [`IDR_MS_TIGHT`]) so a viewer always has
/// a decode entry point within seconds. A resolution change (monitor swap)
/// re-initializes the encoder inside openh264; the next unit out is an IDR.
struct H264Stream {
    /// The active H.264 backend — a hardware encoder (Media Foundation's GPU
    /// H.264 MFT on Windows; NVENC/AMF/QSV/VideoToolbox/VA-API via FFmpeg on
    /// Linux/macOS) when one passed the frame-send test, else software openh264.
    /// Rebuilt (re-laddered) on a resize.
    codec: Box<dyn H264Codec>,
    /// The fitted size the current encoder's bitrate was budgeted for.
    /// The first real frame (or a monitor swap) that fits to a different
    /// size rebuilds the encoder with a corrected budget — monitor
    /// *reports* are logical pixels on HiDPI while captures are physical,
    /// and a 4× pixel gap on the same budget is a mush stream.
    budget_size: (u32, u32),
    tune: Tune,
    fps: u32,
    prev: Vec<u8>,
    prev_size: (u32, u32),
    last_sent: Option<Instant>,
    last_idr: Option<Instant>,
    /// Wall-clock instant of the last *emitted* access unit. The RTP duration of
    /// the next unit is the real gap since this — not a nominal 1/fps — so the
    /// 90 kHz clock tracks wall-clock across static-skip gaps (a 2 s idle then
    /// motion gets a ~2 s duration, not 1/fps), instead of lagging and churning
    /// the viewer's jitter buffer on motion onset.
    last_emit: Option<Instant>,
    /// The route's one-shot "clean entry now" flag (a viewer asked).
    refresh: Arc<AtomicBool>,
    /// The current forced-IDR interval (ms), adapted from receiver feedback
    /// ([`VideoBridge::note_feedback`]). Read fresh each frame; default
    /// [`IDR_MS_TIGHT`].
    idr_ms: Arc<AtomicU64>,
    /// The receiver-driven resolution cap ([`AutoAdapt`]), min'd with the
    /// tuned edge per frame — a changed fit re-inits the encoder through the
    /// budget-size rebuild below, no capture restart.
    auto: Arc<AutoAdapt>,
}

impl H264Stream {
    fn new(
        source_hint: (u32, u32),
        tune: Tune,
        refresh: Arc<AtomicBool>,
        idr_ms: Arc<AtomicU64>,
        auto: Arc<AutoAdapt>,
    ) -> Result<Self, String> {
        let fps = tune.fps();
        // Pre-budget from the monitor's report fitted to the edge
        // ceiling (unknown → 1080p, the old fixed default's density);
        // the first real frame corrects it if the capture's true size
        // differs.
        let auto_capped_edge = match auto.edge_cap() {
            Some(cap) => tune.h264_edge().min(cap),
            None => tune.h264_edge(),
        };
        let (bw, bh) = if source_hint.0 == 0 || source_hint.1 == 0 {
            (1920, 1080)
        } else {
            fit_within_even(source_hint.0, source_hint.1, auto_capped_edge)
        };
        let codec = make_h264_codec(bw, bh, fps, tune)?;
        Ok(H264Stream {
            codec,
            budget_size: (bw, bh),
            tune,
            fps,
            prev: Vec::new(),
            prev_size: (0, 0),
            last_sent: None,
            last_idr: None,
            last_emit: None,
            refresh,
            idr_ms,
            auto,
        })
    }

    /// The edge this frame fits to: the tuned ceiling, capped by the
    /// receiver-driven auto-adapt when it's active.
    fn effective_edge(&self) -> u32 {
        effective_h264_edge(self.tune, &self.auto)
    }

    fn encode(
        &mut self,
        rgba: Vec<u8>,
        sw: u32,
        sh: u32,
        stats: &mut StreamStats,
    ) -> Result<Vec<VideoPacket>, String> {
        if sw == 0 || sh == 0 {
            return Ok(Vec::new());
        }
        let (dw, dh) = fit_within_even(sw, sh, self.effective_edge());
        // Downscale and convert straight to the backend's native 4:2:0
        // layout in one fused pass — no RGB intermediate, no encoder-side
        // re-interleave (hardware MFTs ingest NV12 directly). The
        // unchanged-frame compare also runs on the small 1.5-byte/pixel
        // buffer instead of 3-byte RGB.
        let format = self.codec.input_format();
        let t0 = Instant::now();
        let yuv = match format {
            YuvFormat::I420 => scale_rgba_to_i420(&rgba, sw, sh, dw, dh),
            YuvFormat::Nv12 => scale_rgba_to_nv12(&rgba, sw, sh, dw, dh),
        };
        stats.add_scale(t0.elapsed());
        self.encode_prepared(yuv, format, dw, dh, stats, &mut |_spent| {})
    }

    /// The post-conversion half of [`Self::encode`]: budget rebuild,
    /// unchanged-frame gate, IDR cadence, the backend call, packetize. The
    /// pipelined pump converts on the capture side and feeds this directly,
    /// overlapping conversion with the previous frame's encode; the
    /// single-threaded paths go through [`Self::encode`]. `format` names
    /// the layout `yuv` was converted for — if a rebuild inside this call
    /// (or a producer race) leaves the backend wanting a different layout,
    /// the frame is dropped rather than fed as the wrong chroma order.
    /// Every buffer this call is finished with — a skipped frame, or the
    /// previous retained picture a consumed frame displaces — goes to
    /// `recycle`, which the pipelined pump routes back to the capture side
    /// so steady state converts into reused pages instead of allocating
    /// megabytes per frame.
    fn encode_prepared(
        &mut self,
        yuv: Vec<u8>,
        format: YuvFormat,
        dw: u32,
        dh: u32,
        stats: &mut StreamStats,
        recycle: &mut dyn FnMut(Vec<u8>),
    ) -> Result<Vec<VideoPacket>, String> {
        if dw == 0 || dh == 0 {
            recycle(yuv);
            return Ok(Vec::new());
        }
        // The real fitted size is known now — if it differs from what the
        // bitrate was budgeted for (HiDPI monitors *report* logical pixels
        // but *capture* physical ones), rebuild the encoder on a corrected
        // budget. Its first unit out is an IDR. The retained compare buffer
        // dies with the old codec: the fresh one may ingest a different
        // layout, and stale bytes must never masquerade as "already sent".
        if (dw, dh) != self.budget_size {
            self.codec = make_h264_codec(dw, dh, self.fps, self.tune)?;
            self.budget_size = (dw, dh);
            self.last_idr = None;
            self.prev = Vec::new();
            self.prev_size = (0, 0);
        }
        if format != self.codec.input_format() {
            // Converted for a backend the rebuild just replaced (or the
            // pipelined producer raced a needs change): one dropped frame;
            // the producer re-reads the needs and the next one lands right.
            stats.dropped += 1;
            recycle(yuv);
            return Ok(Vec::new());
        }
        (stats.out_w, stats.out_h) = (dw, dh);
        let refresh_asked = self.refresh.swap(false, Ordering::SeqCst);
        let refresh_due = refresh_asked
            || self
                .last_sent
                .is_none_or(|sent| sent.elapsed() >= STATIC_REFRESH);
        if !refresh_due && self.prev_size == (dw, dh) && self.prev == yuv {
            stats.static_skipped += 1;
            recycle(yuv);
            return Ok(Vec::new());
        }
        // The periodic-IDR interval is adaptive: the receiver's feedback
        // relaxes it on a healthy link and tightens it on a struggling one
        // (see `adaptive_idr_ms`). Default `IDR_MS_TIGHT` = the old fixed 2 s.
        let idr_every = Duration::from_millis(self.idr_ms.load(Ordering::Relaxed));
        let force_idr = refresh_asked || self.last_idr.is_none_or(|idr| idr.elapsed() >= idr_every);
        // Hand the frame to whichever backend the ladder selected — hardware
        // or software. It returns every access unit it had ready (a loaded
        // hardware pipeline may hand back a small backlog) and whether it
        // actually accepted this frame.
        let t1 = Instant::now();
        let outcome = self
            .codec
            .encode_yuv(&yuv, dw as usize, dh as usize, force_idr)?;
        stats.add_encode(t1.elapsed());
        if outcome.consumed {
            recycle(std::mem::replace(&mut self.prev, yuv));
            self.prev_size = (dw, dh);
            self.last_sent = Some(Instant::now());
        } else {
            recycle(yuv);
            if refresh_asked {
                // The frame never entered a stalled encoder, so the viewer's
                // ask wasn't served — re-arm it for the next tick instead of
                // silently eating it. (`prev` deliberately stays untouched:
                // the same content must not be static-skipped on the retry.)
                self.refresh.store(true, Ordering::SeqCst);
            }
        }
        Ok(self.packetize(outcome.units, stats))
    }

    /// Re-encode the retained last picture — a forced, clean IDR when
    /// `force_idr` (the pump's convergence rescue for a screen that went
    /// QUIET: change-driven capture delivers nothing while nothing moves, so
    /// `encode` — and with it [`STATIC_REFRESH`], the IDR cadence and any
    /// viewer refresh ask — never runs, and whatever the transport dropped at
    /// the tail of the last burst stays wrong on the viewer until the next
    /// motion), or a plain re-encode otherwise (the post-quiesce quality
    /// refinement: identical input lets rate control spend idle bandwidth
    /// sharpening what the burst left coarsely quantized). A pending viewer
    /// ask is consumed only by a *successful* IDR re-emit.
    fn re_emit(
        &mut self,
        stats: &mut StreamStats,
        force_idr: bool,
    ) -> Result<Vec<VideoPacket>, String> {
        if self.prev.is_empty() || self.prev_size == (0, 0) {
            return Ok(Vec::new());
        }
        let (dw, dh) = self.prev_size;
        let t1 = Instant::now();
        let outcome = self
            .codec
            .encode_yuv(&self.prev, dw as usize, dh as usize, force_idr)?;
        stats.add_encode(t1.elapsed());
        if outcome.consumed {
            self.last_sent = Some(Instant::now());
            if force_idr {
                self.refresh.store(false, Ordering::SeqCst);
            }
        }
        Ok(self.packetize(outcome.units, stats))
    }

    /// See [`packetize_units`] — bound to this stream's clock state.
    fn packetize(
        &mut self,
        units: Vec<(Vec<u8>, bool)>,
        stats: &mut StreamStats,
    ) -> Vec<VideoPacket> {
        packetize_units(
            units,
            self.fps,
            &mut self.last_emit,
            &mut self.last_idr,
            stats,
        )
    }
}

/// Stamp drained units into wire packets: keyframe/byte accounting, and
/// the RTP duration — the real wall-clock gap since the last emitted unit
/// (so the 90 kHz clock tracks wall-clock across static-skip gaps),
/// clamped to [1/2fps, 5 s] and split evenly across a drained backlog.
/// First-ever unit uses the nominal 1/fps. Free-standing because two
/// stream shapes share it: [`H264Stream`] (the CPU lane) and the GPU
/// lane's texture pump, each carrying its own `last_emit`/`last_idr`
/// clock.
fn packetize_units(
    units: Vec<(Vec<u8>, bool)>,
    fps: u32,
    last_emit: &mut Option<Instant>,
    last_idr: &mut Option<Instant>,
    stats: &mut StreamStats,
) -> Vec<VideoPacket> {
    let units: Vec<(Vec<u8>, bool)> = units.into_iter().filter(|(d, _)| !d.is_empty()).collect();
    if units.is_empty() {
        return Vec::new();
    }
    let now = Instant::now();
    let nominal = 1_000_000u64 / u64::from(fps.max(1));
    let total = match *last_emit {
        Some(prev) => (now.duration_since(prev).as_micros() as u64).clamp(nominal / 2, 5_000_000),
        None => nominal.saturating_mul(units.len() as u64),
    };
    let per_unit = (total / units.len() as u64).max(1);
    *last_emit = Some(now);
    units
        .into_iter()
        .map(|(data, key)| {
            if key {
                *last_idr = Some(now);
                stats.keyframes += 1;
            }
            stats.bytes += data.len() as u64;
            VideoPacket::H264 {
                data,
                key,
                duration_us: per_unit,
            }
        })
        .collect()
}

/// Byte cap for one paced burst — the slice pacer's grain. Encoders are
/// asked to cap slices at this (NVENC `sliceMode=1`, the MF codec API,
/// openh264 `max_slice_len`), and the send-side splitter groups NALs into
/// chunks of at most this many bytes. ≈20 RTP packets ≈ 0.25 ms of line
/// rate per burst — the shape a shallow bottleneck queue absorbs without
/// tail drops.
pub(crate) const PACE_SLICE_BYTES: usize = 24 * 1024;

/// Opt-in for the app-side slice pacer (`ALLMYSTUFF_PACED_SLICES=1`),
/// default OFF until soaked. When on: encoders emit byte-capped slices
/// and the mesh forwarder writes each slice group as its own track send
/// with a small gap — one keyframe's 200-packet wall becomes a handful
/// of spaced ~20-packet bursts, with zero MyOwnMesh involvement (its
/// reassembler emits per marker and its contiguity anchor spans the
/// split writes — verified against the daemon's `H264AuAssembler`).
/// GDR lanes register their wave-restart flag here, keyed by route — the
/// module-global seam between the encode lane and the feedback path,
/// which meet nowhere else. Entries are lane-lifetime (RAII-removed).
/// The value is the requested wave length in frames (0 = idle): the loss
/// chooser writes 3 for a fast heal on a lossy spell or the smooth
/// default on a one-off; the encode thread swaps it back to 0 as it arms.
pub(crate) fn wave_flags(
) -> &'static parking_lot::Mutex<std::collections::HashMap<String, std::sync::Arc<AtomicU32>>> {
    static FLAGS: std::sync::LazyLock<
        parking_lot::Mutex<std::collections::HashMap<String, std::sync::Arc<AtomicU32>>>,
    > = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
    &FLAGS
}

/// A direct-SDK heal onto Media Foundation loses rolling intra refresh. Clear
/// both the lane-local bit and the feedback registry atomically enough for the
/// control path: subsequent loss reports must request an IDR, never write a
/// stale wave flag that no encoder will consume.
#[cfg(windows)]
fn disable_gdr_route(route_id: &str, gdr: &mut bool, wave: &Arc<AtomicU32>) -> bool {
    let was_gdr = std::mem::replace(gdr, false);
    wave.store(0, Ordering::SeqCst);
    let mut flags = wave_flags().lock();
    let removed = if flags
        .get(route_id)
        .is_some_and(|registered| Arc::ptr_eq(registered, wave))
    {
        flags.remove(route_id);
        true
    } else {
        false
    };
    let changed = was_gdr || removed;
    if changed {
        tracing::info!(
            "video encoder fallback for {route_id}: GDR wave unregistered; IDR recovery restored"
        );
    }
    changed
}

/// The steady-state wave length for `fps` — the same shape init
/// configures: the refresh period spread over a fifth, floored at 3.
pub(crate) fn default_wave_frames(fps: u32) -> u32 {
    ((fps / 2).max(15) / 5).max(3)
}

/// Per-route rate state shared between the encode lane (which applies the
/// target via in-place reconfigure and never restarts the stream), the
/// feedback path (the AIMD controller below), and the mesh forwarder (the
/// pacer's drain model) — the same module-global seam pattern as
/// [`wave_flags`], and like it, entries are lane-lifetime (RAII-removed).
pub(crate) struct RouteRate {
    /// The current target (bps). The encode thread reads it every frame
    /// and re-aims the rate controller when it moves; the mesh pacer
    /// reads it to spread bursts at a rate the link is actually being
    /// asked to carry.
    pub target: AtomicU32,
    /// The posture lane's full budget — where AIMD climbs back to.
    pub ceiling: AtomicU32,
    adapt: Mutex<RateAdaptState>,
}

#[derive(Default)]
struct RateAdaptState {
    bad: u32,
    good: u32,
    last_step: Option<Instant>,
}

pub(crate) fn route_rates(
) -> &'static parking_lot::Mutex<std::collections::HashMap<String, std::sync::Arc<RouteRate>>> {
    static RATES: std::sync::LazyLock<
        parking_lot::Mutex<std::collections::HashMap<String, std::sync::Arc<RouteRate>>>,
    > = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
    &RATES
}

/// Live, pollable per-route encode facts for the GUI's "effective reality"
/// panel — the counterpart to [`route_rates`] for the dials the rate
/// controller doesn't own. Published cheaply by the encode lanes through
/// [`StreamStats::maybe_log`] (output dims + codec, every frame) and by
/// [`run_capture`] / [`HealingEncoder::new`] (the encoder rung label, once
/// per lane), and read by [`VideoBridge::route_dials`]. Lane-lifetime like
/// [`route_rates`]; [`VideoBridge::stop`] reaps the entry.
#[derive(Default)]
pub(crate) struct RouteLive {
    /// Actual encoded output width/height (0 until the first frame lands).
    out_w: AtomicU32,
    out_h: AtomicU32,
    /// Wire codec: 0 unknown, 1 H.264, 2 MJPEG.
    codec: AtomicU8,
    /// Human encoder-rung label ("GPU (hardware)", "H.264 (CPU)", …).
    encoder: Mutex<String>,
}

pub(crate) fn route_live(
) -> &'static parking_lot::Mutex<std::collections::HashMap<String, std::sync::Arc<RouteLive>>> {
    static LIVE: std::sync::LazyLock<
        parking_lot::Mutex<std::collections::HashMap<String, std::sync::Arc<RouteLive>>>,
    > = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
    &LIVE
}

/// Get-or-create the live cell for `route_id` — one per route, shared by
/// whichever lane (GPU or CPU) is currently encoding it.
fn route_live_cell(route_id: &str) -> std::sync::Arc<RouteLive> {
    route_live()
        .lock()
        .entry(route_id.to_string())
        .or_default()
        .clone()
}

/// The effective encode dials for one locally-encoded route — the values the
/// viewer's panel shows beside the requested [`Tune`]. Assembled from
/// [`Tune`] (posture / fps target / edge cap), [`route_rates`] (the AIMD
/// bitrate target + its ceiling), and [`route_live`] (actual dims, codec,
/// rung). Built by [`VideoBridge::route_dials`].
#[derive(Debug, Clone)]
pub struct RouteDials {
    pub posture: &'static str,
    pub encoder_label: String,
    pub codec: &'static str,
    pub target_bitrate_bps: u32,
    pub ceiling_bps: u32,
    pub fps_target: u32,
    pub edge_cap: u32,
    pub out_w: u32,
    pub out_h: u32,
}

/// Where the closed-loop bitrate may act. The rule: an automatic rate
/// changer runs by default ONLY where it is beneficial in every case
/// for that mode's use case. **Game qualifies** — its identity is
/// smoothness/latency over quality, so a congestion cut is always the
/// right trade there and even a false positive costs little (the climb
/// restores). **Balanced and Studio don't**: their deal is the picked
/// quality, and a false-positive cut (viewer-side CPU hiccup reading as
/// congestion) would silently soften a healthy stream — the same
/// native-quality contract that keeps [`auto_adapt_enabled`] opt-in.
/// `ALLMYSTUFF_RATE_ADAPT=1` opts every lossy posture in (field A/B);
/// `=0` kills it everywhere; unset = game-only.
#[derive(Clone, Copy, PartialEq)]
enum RateAdaptMode {
    Off,
    GameOnly,
    All,
}

fn rate_adapt_mode() -> RateAdaptMode {
    static MODE: std::sync::LazyLock<RateAdaptMode> = std::sync::LazyLock::new(|| {
        match std::env::var("ALLMYSTUFF_RATE_ADAPT")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "0" | "off" | "false" => {
                tracing::info!("ALLMYSTUFF_RATE_ADAPT=0 — closed-loop bitrate disabled");
                RateAdaptMode::Off
            }
            "1" | "on" | "true" | "all" => {
                tracing::info!(
                    "ALLMYSTUFF_RATE_ADAPT on: closed-loop bitrate for every lossy posture"
                );
                RateAdaptMode::All
            }
            _ => RateAdaptMode::GameOnly,
        }
    });
    *MODE
}

/// The gate itself, pure for the tests.
fn rate_adapt_allowed(game: bool, mode: RateAdaptMode) -> bool {
    match mode {
        RateAdaptMode::Off => false,
        RateAdaptMode::GameOnly => game,
        RateAdaptMode::All => true,
    }
}

/// Consecutive struggling reports (~2 s apart) before a cut — one report
/// can be a scheduler hiccup; two is a trend at this cadence.
const RATE_BAD_STREAK: u32 = 2;
/// Consecutive clean reports before a climb — slow up, mirroring the
/// auto-adapt discipline (stepping up too eagerly re-breaks the viewer).
const RATE_GOOD_STREAK: u32 = 5;
/// Settle time after any step before the next move in either direction:
/// the reconfigure and the viewer's queue need a beat to show up in the
/// feedback before it's evidence about the new rate.
const RATE_HOLD: Duration = Duration::from_secs(6);
/// The floor AIMD never cuts through — the existing stream budget floor.
const RATE_FLOOR: u32 = 8_000_000;

/// One AIMD step from one feedback report: multiplicative down on
/// congestion evidence, additive up on sustained health, `None` to hold.
/// Pure — the unit tests drive it with synthetic reports.
fn rate_adapt_step(
    state: &mut RateAdaptState,
    fb: &RecvFeedback,
    target_fps: u32,
    current: u32,
    ceiling: u32,
    now: Instant,
) -> Option<u32> {
    // Congestion evidence, most-direct first: the viewer's decode queue
    // backing up, decode failures (loss), the arrival-rate estimate
    // sagging below what we send, a sustained one-way-delay ramp, or the
    // rendered cadence collapsing versus target.
    let est_sagging = fb.est_kbps > 0 && (fb.est_kbps as u64 * 1000) < (current as u64 * 85 / 100);
    let delay_ramping = fb.delay_trend_us_per_s > 20_000; // +20 ms of queue per second
    let struggling = fb.queue_depth > 8
        || fb.decode_fails > 0
        || est_sagging
        || delay_ramping
        || (target_fps > 0
            && fb.recv_fps > 0
            && fb.recv_fps.saturating_mul(10) < target_fps.saturating_mul(7));
    let held = state
        .last_step
        .is_some_and(|t| now.duration_since(t) < RATE_HOLD);
    if struggling {
        state.good = 0;
        state.bad += 1;
        if state.bad >= RATE_BAD_STREAK && !held {
            // Cut toward the evidence: 0.7×, or straight to just under
            // the measured arrival rate when the estimate is the witness
            // (converges in one step instead of several blind ones).
            let mut next = (current as u64 * 7 / 10) as u32;
            if fb.est_kbps > 0 {
                // Bound the peer-supplied estimate in u64 before the cast:
                // a huge `est_kbps` would otherwise truncate to garbage.
                let est_bps = (fb.est_kbps as u64 * 1000 * 85 / 100).min(u64::from(ceiling)) as u32;
                next = next.min(est_bps);
            }
            // `RATE_FLOOR.min(ceiling)` keeps the clamp self-consistent:
            // `Ord::clamp` panics (→ abort under `panic = "abort"`) when
            // min > max, which a route ceiling configured below the 8 Mbps
            // floor (`ALLMYSTUFF_VIDEO_BITRATE` bypasses the floor) makes
            // reachable via two peer congestion reports. When the ceiling
            // is under the floor we simply don't cut below it.
            let next = next.clamp(RATE_FLOOR.min(ceiling), ceiling);
            if next < current {
                state.bad = 0;
                state.last_step = Some(now);
                return Some(next);
            }
        }
        return None;
    }
    state.bad = 0;
    if current >= ceiling {
        state.good = 0;
        return None;
    }
    state.good += 1;
    if state.good >= RATE_GOOD_STREAK && !held {
        // Additive climb: 8% of the ceiling per step — reaches full rate
        // from half in ~6 clean cycles (~a minute) without overshooting.
        let next = current
            .saturating_add((ceiling / 12).max(500_000))
            .min(ceiling);
        state.good = 0;
        state.last_step = Some(now);
        return Some(next);
    }
    None
}

pub(crate) fn paced_slices_enabled() -> bool {
    // Production is deliberately locked safe-off for the upstream cut. The
    // env-controlled form remains in test builds so the fork can exercise its
    // sender/receiver prototype without making one-sided field configuration
    // or version skew capable of fragmenting pictures for WebCodecs/NVDEC.
    #[cfg(not(test))]
    {
        false
    }
    #[cfg(test)]
    {
        static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
            // Experimental opt-in only. A paced picture becomes several media
            // samples with one RTP timestamp; WebCodecs and NVDEC require a whole
            // access-unit/finality contract that shipped ingress does not carry.
            // Defaulting this on caused partial-frame decode regressions.
            let on = std::env::var("ALLMYSTUFF_PACED_SLICES")
                .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "on" | "true"))
                .unwrap_or(false);
            if on {
                tracing::warn!(
                    "ALLMYSTUFF_PACED_SLICES=1 — experimental multi-sample AU pacing enabled"
                );
            }
            on
        });
        *ON
    }
}

/// Split one Annex-B access unit into paced chunks of at most `max_chunk`
/// bytes, cutting **only** at slice-NAL boundaries and gluing any
/// parameter-set/SEI run to the slice that follows it — a chunk never
/// strands a slice from its headers. The ranges concatenate back to one
/// complete access unit; they are not generally independent pictures
/// (WebCodecs/NVDEC require receiver-side AU completion). Speaks both codecs:
/// an AU carrying HEVC parameter sets
/// (VPS/SPS/PPS — byte values no H.264 key unit leads with) cuts at HEVC
/// VCL NALs; anything else cuts at H.264 slice types 1/5, which for a
/// paramless HEVC delta AU means "no cut" — correct, just unpaced, and
/// the keyframe walls the pacer exists for always carry their parameter
/// sets. Returns contiguous ranges that concatenate back to `data`
/// byte-identically; a unit whose single slice exceeds the cap stays
/// whole (slice granularity is the floor — the encoder-side slice count
/// is what makes real splits exist).
///
/// **AV1 (future):** AV1 has no Annex-B start codes — it's OBUs with
/// leb128 sizes — so this walk finds no cut points and returns the whole
/// AU as one chunk. That is the correct SAFE fallback (AV1 rides unpaced,
/// exactly like a paramless HEVC delta), so nothing breaks when AV1
/// lands; but the pacer's burst-shaping is then off for AV1. The AV1-
/// aware seam is here: an `obu_split` branch that cuts at tile-group /
/// frame OBU boundaries (the AV1 analog of slice NALs) — walk OBU
/// headers via leb128 like `sniff_av1_obu` in `video_decode.rs`, group
/// to `max_chunk`.
pub(crate) fn split_annexb_paced(data: &[u8], max_chunk: usize) -> Vec<std::ops::Range<usize>> {
    // Walk the start codes (00 00 01 and 00 00 00 01), recording each
    // NAL's offset and header byte.
    // SIMD-anchored: memchr sweeps for 0x01 at cache speed and the
    // look-behind confirms the 00 00 prefix — the pacer runs this over
    // every AU (a lossless IDR is ~1.4 MB), and the old byte-stepping
    // loop paid a branch per byte. Equivalent to the forward scan: a
    // start code's own bytes can never satisfy another match's [0,0]
    // look-behind, and a run of ≥3 zeros anchors at p−3 exactly where
    // the forward scan entered its 4-byte arm.
    let mut nals: Vec<(usize, u8)> = Vec::new();
    for p in memchr::memchr_iter(1, data) {
        if p < 2 || data[p - 1] != 0 || data[p - 2] != 0 {
            continue;
        }
        let i = if p >= 3 && data[p - 3] == 0 {
            p - 3
        } else {
            p - 2
        };
        let hdr = p + 1;
        if hdr < data.len() {
            nals.push((i, data[hdr]));
        }
    }
    // Exact parameter-set bytes (0x40/0x42/0x44 = VPS/SPS/PPS, layer 0)
    // — a masked type test would also match 0x41, the H.264 referenced
    // P-slice byte, and misread whole H.264 streams as HEVC.
    let hevc = nals.iter().any(|&(_, b)| matches!(b, 0x40 | 0x42 | 0x44));
    let is_slice = |b: u8| {
        if hevc {
            b & 0x80 == 0 && ((b >> 1) & 0x3F) <= 21 // any VCL NAL
        } else {
            matches!(b & 0x1F, 1 | 5)
        }
    };
    // One decodable unit per slice NAL, absorbing the non-slice run
    // before it; anything before the first slice belongs to the first
    // unit, anything after the last slice's data to the last.
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
    // Greedy pack: extend the current chunk unit by unit; cut when the
    // next extension would overflow the cap (an oversized single unit
    // still ships whole).
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

/// The route's effective H.264 bitrate: the viewer's explicit Rate pill,
/// else the pixel budget — floored to the Studio fidelity budget when
/// that posture is active — clamped into the posture's lane. Studio may
/// spend up to 250 Mbps on the LAN it's gated to; every other posture
/// stays under the 80 Mbps stability ceiling.
fn tuned_bitrate(tune: Tune, w: u32, h: u32, fps: u32) -> u32 {
    let auto = h264_bitrate_for(w, h, fps, tune.link);
    // Posture sets the auto budget's floor and the ceiling the viewer's
    // Rate pill can reach. Both Studio and Game uncork well past the
    // Balanced stability ceiling:
    //  - Game, because its single-frame VBV (see the NVENC kernel) turns
    //    extra bits into constant-per-frame motion quality — crisp fast
    //    pans — at NO latency cost, since every frame stays one frame
    //    interval's size regardless of the budget.
    //  - Studio, because it's the deliberate spend-the-pipe fidelity mode
    //    (150 Mbps auto floor); the viewer's warning gates it, then the
    //    user owns the wire.
    let (floor, ceiling) = match tune.posture() {
        // Lossless has no rate control — this number only steers the
        // lossy-Studio fallback when the HEVC rung can't open (and the
        // noise guard's landing spot), so it mirrors Studio.
        Posture::Studio | Posture::StudioLossless => (auto.max(150_000_000), 500_000_000),
        Posture::Game => (auto, 200_000_000),
        Posture::Balanced => (auto, 80_000_000),
    };
    tune.bitrate.unwrap_or(floor).clamp(250_000, ceiling)
}

/// The H.264 edge a frame fits to right now: the tuned ceiling, capped by
/// the receiver-driven auto-adapt when it's active. Shared by
/// [`H264Stream`] and the GPU lane (which re-fits — and rebuilds — when
/// this changes under it).
fn effective_h264_edge(tune: Tune, auto: &AutoAdapt) -> u32 {
    match auto.edge_cap() {
        Some(cap) => tune.h264_edge().min(cap),
        None => tune.h264_edge(),
    }
}

/// A borrowing view over a contiguous I420 buffer (Y, then U, then V) that
/// satisfies openh264's `YUVSource` — lets [`H264Stream::encode`] hand the
/// planes straight to the encoder with no copy and no RGB→YUV step.
struct I420Frame<'a> {
    buf: &'a [u8],
    w: usize,
    h: usize,
}

impl openh264::formats::YUVSource for I420Frame<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.w, self.h)
    }
    fn strides(&self) -> (usize, usize, usize) {
        (self.w, self.w / 2, self.w / 2)
    }
    fn y(&self) -> &[u8] {
        &self.buf[..self.w * self.h]
    }
    fn u(&self) -> &[u8] {
        let ys = self.w * self.h;
        let cs = (self.w / 2) * (self.h / 2);
        &self.buf[ys..ys + cs]
    }
    fn v(&self) -> &[u8] {
        let ys = self.w * self.h;
        let cs = (self.w / 2) * (self.h / 2);
        &self.buf[ys + cs..ys + 2 * cs]
    }
}

/// One encode call's outcome from a backend: every access unit it had ready
/// (**oldest first** — a loaded hardware encoder may hand back a small
/// backlog), and whether THIS call's input frame actually entered the
/// encoder.
///
/// The two fields exist because hardware pipelines decouple input from
/// output. Returning *all* drained units is what keeps the P-frame chain
/// intact — the old single-unit seam made a backlogged pump overwrite (and
/// so silently drop) reference frames, which the viewer experienced as
/// smearing until the next IDR. `consumed: false` means a stalled input
/// queue accepted nothing: the caller must not treat the frame's content as
/// sent (or a pending refresh ask as served) — the next capture re-offers
/// the same pixels.
pub(crate) struct EncodeOutcome {
    /// Drained Annex-B access units + their keyframe flags, oldest first.
    /// Empty while the encoder is buffering.
    pub units: Vec<(Vec<u8>, bool)>,
    /// Whether the input frame entered the encoder this call.
    pub consumed: bool,
    /// The encoder's own input timestamp for the frame consumed this call
    /// — the key `nvEncInvalidateRefFrames` takes. The lane pairs it with
    /// the frame's presentation time so a viewer's loss report (a wire
    /// timestamp) maps back to the exact reference to invalidate. 0 on
    /// backends without invalidation (MF, openh264).
    #[cfg_attr(not(windows), allow(dead_code))]
    pub input_ts: u64,
}

impl EncodeOutcome {
    /// A backend that synchronously consumed the frame and produced at most
    /// one unit (openh264's shape).
    pub(crate) fn consumed(unit: Option<(Vec<u8>, bool)>) -> Self {
        EncodeOutcome {
            units: unit.into_iter().filter(|(d, _)| !d.is_empty()).collect(),
            consumed: true,
            input_ts: 0,
        }
    }
}

/// The 4:2:0 layout a backend ingests. The stream converts captured RGBA
/// straight to this in one fused pass — producing the backend's native
/// layout directly deletes a whole per-frame chroma re-interleave.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum YuvFormat {
    /// Y plane, then U, then V — openh264's shape.
    I420,
    /// Y plane, then interleaved U/V — what hardware H.264 MFTs ingest.
    Nv12,
}

/// A pluggable H.264 backend: one fitted 4:2:0 frame in, Annex-B access
/// units out. The ladder ([`make_h264_codec`]) selects the implementation;
/// everything around it (scaling, the static-frame skip, the adaptive IDR
/// cadence, stats) stays in [`H264Stream`]. `Send` so the whole stream can
/// live on the route's capture/encode thread.
trait H264Codec: Send {
    /// The layout [`Self::encode_yuv`] expects; the stream converts capture
    /// output straight to it.
    fn input_format(&self) -> YuvFormat {
        YuvFormat::I420
    }
    /// Encode one contiguous 4:2:0 frame laid out per
    /// [`Self::input_format`]. `force_idr` requests a keyframe. Returns
    /// every unit the backend had ready plus whether the input was accepted
    /// — see [`EncodeOutcome`].
    fn encode_yuv(
        &mut self,
        yuv: &[u8],
        w: usize,
        h: usize,
        force_idr: bool,
    ) -> Result<EncodeOutcome, String>;
    /// Human label for logs ("openh264 (software)", "h264_nvenc", …).
    fn label(&self) -> &str;
}

/// Software openh264 — the guaranteed floor of the ladder.
struct OpenH264Codec(openh264::encoder::Encoder);

impl H264Codec for OpenH264Codec {
    fn encode_yuv(
        &mut self,
        yuv: &[u8],
        w: usize,
        h: usize,
        force_idr: bool,
    ) -> Result<EncodeOutcome, String> {
        if force_idr {
            self.0.force_intra_frame();
        }
        let stream = self
            .0
            .encode(&I420Frame { buf: yuv, w, h })
            .map_err(|e| format!("openh264 encode: {e}"))?;
        let key = matches!(
            stream.frame_type(),
            openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
        );
        let data = stream.to_vec();
        Ok(EncodeOutcome::consumed(Some((data, key))))
    }
    fn label(&self) -> &str {
        "openh264 (software)"
    }
}

/// Media Foundation hardware backend (NVENC/QuickSync/AMD via the OS's H.264
/// MFT) — the Windows hardware path, no FFmpeg toolchain. Thin wrapper so the
/// inherent methods on `mediafoundation::MediaFoundationH264` don't clash with
/// the trait.
#[cfg(windows)]
struct MfCodec(crate::mediafoundation::MediaFoundationH264);

#[cfg(windows)]
impl H264Codec for MfCodec {
    /// NV12 straight from the fused scaler — the MFT's native layout, no
    /// encoder-side re-interleave.
    fn input_format(&self) -> YuvFormat {
        YuvFormat::Nv12
    }
    fn encode_yuv(
        &mut self,
        yuv: &[u8],
        _w: usize,
        _h: usize,
        force_idr: bool,
    ) -> Result<EncodeOutcome, String> {
        // The MFT is fixed to the size it was opened at — the same (dw, dh) the
        // ladder built it for; `H264Stream` rebuilds on resize.
        self.0.encode_nv12(yuv, force_idr)
    }
    fn label(&self) -> &str {
        self.0.label()
    }
}

/// VideoToolbox hardware backend — the Mac's media engine, no FFmpeg
/// toolchain (see `videotoolbox.rs`). Thin wrapper so the inherent methods
/// don't clash with the trait.
#[cfg(target_os = "macos")]
struct VtCodec(crate::videotoolbox::VideoToolboxH264);

#[cfg(target_os = "macos")]
impl H264Codec for VtCodec {
    fn encode_yuv(
        &mut self,
        yuv: &[u8],
        _w: usize,
        _h: usize,
        force_idr: bool,
    ) -> Result<EncodeOutcome, String> {
        // The session is fixed to the size it was opened at — the same
        // (dw, dh) the ladder built it for; `H264Stream` rebuilds on resize.
        // Input stays I420 (the default `input_format`): the CVPixelBuffer
        // is planar y420.
        self.0.encode_i420(yuv, force_idr)
    }
    fn label(&self) -> &str {
        self.0.label()
    }
}

/// FFmpeg hardware backend (NVENC/AMF/QSV/VideoToolbox/VA-API), behind the
/// `hwenc` feature. Thin wrapper so the inherent methods on `hwenc::FfmpegH264`
/// don't clash with the trait.
#[cfg(feature = "hwenc")]
struct FfmpegCodec(crate::hwenc::FfmpegH264);

#[cfg(feature = "hwenc")]
impl H264Codec for FfmpegCodec {
    fn encode_yuv(
        &mut self,
        yuv: &[u8],
        _w: usize,
        _h: usize,
        force_idr: bool,
    ) -> Result<EncodeOutcome, String> {
        // The FFmpeg encoder is fixed to the size it was opened at — the same
        // (dw, dh) the ladder built it for; `H264Stream` rebuilds on resize.
        // Input stays I420 (the default `input_format`): the AVFrame planes
        // are planar YUV420P.
        self.0.encode_i420(yuv, force_idr)
    }
    fn label(&self) -> &str {
        self.0.label()
    }
}

/// Build the best H.264 backend for `bw`×`bh` at `fps`: walk the platform's
/// hardware candidates (NVENC first), open each and **frame-send-test** it —
/// the first that actually emits an access unit wins. Anything that won't open
/// or won't produce a frame (no GPU, driver/permission/session-cap trouble) is
/// stepped over, down to software openh264, which is the guaranteed floor.
///
/// The hardware rung is platform-split: Windows uses **Media Foundation** (the
/// GPU's own H.264 MFT, no FFmpeg toolchain); Linux/macOS use the **FFmpeg**
/// vendor encoders behind the `hwenc` feature.
fn make_h264_codec(bw: u32, bh: u32, fps: u32, tune: Tune) -> Result<Box<dyn H264Codec>, String> {
    // Windows: the GPU's hardware H.264 MFT via Media Foundation. Enumerated
    // best-first by the OS; each is opened and frame-send-tested, stepping down
    // to the next (then to software) on any failure. No extra build toolchain.
    #[cfg(windows)]
    {
        let bitrate = tuned_bitrate(tune, bw, bh, fps);
        let game = tune.game();
        for hw in crate::mediafoundation::hardware_h264_mfts() {
            match hw.open_for_route(bw, bh, fps, bitrate, game) {
                Ok(enc) => {
                    let mut probe = MfCodec(enc);
                    let label = probe.label().to_string();
                    if codec_emits_frame(&mut probe, bw, bh) {
                        // The probe encoded up to ten forced-grey IDRs. Drop
                        // it, clear IMFActivate's cached transform, and open a
                        // genuinely fresh live encoder so none of those bytes
                        // or references can enter the route.
                        drop(probe);
                        if let Err(e) = hw.shutdown_probe_instance() {
                            tracing::debug!(
                                "Media Foundation H.264 probe reset failed for {label}: {e}"
                            );
                            continue;
                        }
                        match hw.open_for_route(bw, bh, fps, bitrate, game) {
                            Ok(enc) => {
                                let codec = MfCodec(enc);
                                tracing::info!(
                                    "H.264 hardware encoder: {} · {bw}×{bh} · {:.1} Mbps @ {fps} fps (Media Foundation)",
                                    codec.label(),
                                    bitrate as f64 / 1e6
                                );
                                return Ok(Box::new(codec));
                            }
                            Err(e) => tracing::debug!(
                                "Media Foundation H.264 live reopen failed after probe for {label}: {e}"
                            ),
                        }
                    } else {
                        tracing::info!(
                            "H.264 encoder {label} opened but produced no frame in the send test — stepping down"
                        );
                    }
                }
                Err(e) => tracing::debug!("Media Foundation H.264 MFT unavailable: {e}"),
            }
        }
    }
    // macOS: the media engine via VideoToolbox — hardware required (its
    // software fallback would just be a slower openh264), frame-send-tested
    // like every rung. This is what takes a Retina Mac host off software
    // openh264, the encoder the viewer experienced as a slideshow.
    #[cfg(target_os = "macos")]
    {
        let bitrate = tuned_bitrate(tune, bw, bh, fps);
        match crate::videotoolbox::VideoToolboxH264::open(bw, bh, fps, bitrate) {
            Ok(enc) => {
                let mut probe = VtCodec(enc);
                if codec_emits_frame(&mut probe, bw, bh) {
                    // A VT session owns its callback queue/reference state;
                    // invalidating the probe before reopening keeps its grey
                    // units out of the live stream.
                    drop(probe);
                    match crate::videotoolbox::VideoToolboxH264::open(bw, bh, fps, bitrate) {
                        Ok(enc) => {
                            let codec = VtCodec(enc);
                            tracing::info!(
                                "H.264 hardware encoder: {} · {bw}×{bh} · {:.1} Mbps @ {fps} fps",
                                codec.label(),
                                bitrate as f64 / 1e6
                            );
                            return Ok(Box::new(codec));
                        }
                        Err(e) => tracing::debug!(
                            "VideoToolbox live reopen failed after successful probe: {e}"
                        ),
                    }
                } else {
                    tracing::info!(
                        "VideoToolbox opened but produced no frame in the send test — stepping down"
                    );
                }
            }
            Err(e) => tracing::debug!("VideoToolbox H.264 unavailable: {e}"),
        }
    }
    #[cfg(feature = "hwenc")]
    {
        let bitrate = tuned_bitrate(tune, bw, bh, fps);
        let game = tune.game();
        for &name in crate::hwenc::candidates() {
            match crate::hwenc::FfmpegH264::open(name, bw, bh, fps, bitrate, game) {
                Ok(enc) => {
                    let mut probe = FfmpegCodec(enc);
                    if codec_emits_frame(&mut probe, bw, bh) {
                        // Discard the probed AVCodecContext entirely. A fresh
                        // context starts the route at PTS zero with no queued
                        // grey packets or reference history.
                        drop(probe);
                        match crate::hwenc::FfmpegH264::open(
                            name, bw, bh, fps, bitrate, game,
                        ) {
                            Ok(enc) => {
                                let codec = FfmpegCodec(enc);
                                tracing::info!(
                                    "H.264 hardware encoder: {} · {bw}×{bh} · {:.1} Mbps @ {fps} fps",
                                    codec.label(),
                                    bitrate as f64 / 1e6
                                );
                                return Ok(Box::new(codec));
                            }
                            Err(e) => tracing::debug!(
                                "H.264 encoder {name} live reopen failed after successful probe: {e}"
                            ),
                        }
                    } else {
                        tracing::info!(
                            "H.264 encoder {name} opened but produced no frame in the send test — stepping down"
                        );
                    }
                }
                Err(e) => tracing::debug!("H.264 encoder {name} unavailable: {e}"),
            }
        }
    }
    let codec: Box<dyn H264Codec> = Box::new(OpenH264Codec(make_h264_encoder(bw, bh, fps, tune)?));
    tracing::info!(
        "H.264 software encoder: {} · {bw}×{bh} · {:.1} Mbps @ {fps} fps",
        codec.label(),
        tuned_bitrate(tune, bw, bh, fps) as f64 / 1e6
    );
    Ok(codec)
}

/// The frame-send test that drives the step-down: feed one neutral-grey I420
/// and confirm the encoder emits an access unit within a few frames. A backend
/// that opens but won't actually produce frames is stepped over.
#[cfg(any(feature = "hwenc", windows, target_os = "macos"))]
fn codec_emits_frame(codec: &mut dyn H264Codec, w: u32, h: u32) -> bool {
    let (w, h) = (w as usize, h as usize);
    let grey = vec![128u8; w * h + 2 * ((w / 2) * (h / 2))];
    // Generous attempt count: each call waits only the short async grace, so
    // on a GPU that's busy with other work (the very situation the stream is
    // for) output can lag several calls behind. The probe runs once per
    // stream start — a few hundred slow-path ms here is cheap next to
    // wrongly demoting a loaded-but-working hardware encoder to software.
    for _ in 0..10 {
        // All-128 bytes are a valid neutral-grey frame in both 4:2:0
        // layouts, so the probe needn't care which the backend ingests.
        match codec.encode_yuv(&grey, w, h, true) {
            Ok(o) if o.units.iter().any(|(d, _)| !d.is_empty()) => return true,
            Ok(_) => continue, // buffering — try another frame
            Err(_) => return false,
        }
    }
    false
}

/// One openh264 encoder, budgeted for the given fitted size: the tune's
/// explicit bitrate when the viewer set one, the pixel budget otherwise.
fn make_h264_encoder(
    bw: u32,
    bh: u32,
    fps: u32,
    tune: Tune,
) -> Result<openh264::encoder::Encoder, String> {
    use openh264::encoder::{
        BitRate, Encoder, EncoderConfig, FrameRate, RateControlMode, UsageType,
    };
    let bitrate = tuned_bitrate(tune, bw, bh, fps);
    let mut config = EncoderConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(bitrate))
        .max_frame_rate(FrameRate::from_hz(fps as f32));
    if paced_slices_enabled() {
        // Byte-capped slices give the send-side pacer real cut points —
        // a keyframe becomes several independently-decodable slices
        // instead of one wall (see [`split_annexb_paced`]).
        config = config.max_slice_len(PACE_SLICE_BYTES as u32);
    }
    Encoder::with_api_config(openh264::OpenH264API::from_source(), config)
        .map_err(|e| format!("openh264 init: {e}"))
}

/// [`fit_within`], then force both edges even (4:2:0 chroma subsampling
/// needs it; a 1-pixel crop is invisible at these sizes). `pub(crate)`:
/// the GPU lane fits its blt output with the same rule
/// (`win_capture::start_gpu`).
pub(crate) fn fit_within_even(w: u32, h: u32, max_edge: u32) -> (u32, u32) {
    let (w, h) = fit_within(w, h, max_edge);
    ((w & !1).max(2), (h & !1).max(2))
}

fn encode_jpeg(rgba: &[u8], w: u32, h: u32, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(64 * 1024);
    let encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    encoder
        .encode(
            rgba,
            w.try_into().map_err(|_| "frame too wide")?,
            h.try_into().map_err(|_| "frame too tall")?,
            jpeg_encoder::ColorType::Rgba,
        )
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// Scale `(w, h)` down (never up) so the longest edge fits `max_edge`,
/// preserving aspect.
fn fit_within(w: u32, h: u32, max_edge: u32) -> (u32, u32) {
    let edge = w.max(h);
    if edge <= max_edge || edge == 0 {
        return (w, h);
    }
    let num = max_edge as u64;
    let den = edge as u64;
    (
        ((w as u64 * num / den) as u32).max(1),
        ((h as u64 * num / den) as u32).max(1),
    )
}

// The scalers live in `allmystuff-pixels` (path crate) purely so dev
// builds run them at opt-level 3 — at opt 0 they alone cap the stream at
// single-digit fps on a Retina/4K source.
use allmystuff_pixels::{
    scale_rgba, scale_rgba_to_i420, scale_rgba_to_i420_into, scale_rgba_to_nv12,
    scale_rgba_to_nv12_into,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn experimental_gpu_paths_require_an_explicit_boolean_value() {
        assert_eq!(explicit_bool_choice(None), None);
        assert_eq!(explicit_bool_choice(Some("")), None);
        assert_eq!(explicit_bool_choice(Some("unexpected")), None);
        for value in ["1", "on", "TRUE", " true "] {
            assert_eq!(explicit_bool_choice(Some(value)), Some(true), "{value}");
        }
        for value in ["0", "off", "FALSE", " false "] {
            assert_eq!(explicit_bool_choice(Some(value)), Some(false), "{value}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn gpu_lane_policy_quarantines_every_posture_unless_explicitly_enabled() {
        for posture in [
            Posture::Balanced,
            Posture::Game,
            Posture::Studio,
            Posture::StudioLossless,
        ] {
            assert!(!gpu_lane_policy(posture, None));
            assert!(gpu_lane_policy(posture, Some(true)));
            assert!(!gpu_lane_policy(posture, Some(false)));
        }
    }

    #[cfg(windows)]
    #[test]
    fn only_invalid_hmonitor_errors_trigger_screenshot_reacquisition() {
        assert!(invalid_monitor_handle(
            "Invalid monitor handle. (0x800705B5)"
        ));
        assert!(invalid_monitor_handle("invalid monitor handle"));
        assert!(!invalid_monitor_handle("Access is denied. (0x80070005)"));
        assert!(!invalid_monitor_handle(
            "screen recording permission denied"
        ));
    }

    #[cfg(windows)]
    #[test]
    fn exact_monitor_reacquisition_never_uses_a_stale_numeric_handle() {
        assert_eq!(
            monitor_reacquire_key(None, None),
            Ok(MonitorReacquireKey::Primary)
        );
        assert_eq!(
            monitor_reacquire_key(Some(0x1234), Some(r"\\.\DISPLAY2")),
            Ok(MonitorReacquireKey::DeviceName(r"\\.\DISPLAY2".into()))
        );
        assert!(monitor_reacquire_key(Some(0x1234), None)
            .unwrap_err()
            .contains("refusing stale-handle fallback"));
    }

    #[cfg(windows)]
    #[test]
    fn mf_fallback_unregisters_the_stale_gdr_wave() {
        let route = "test:gdr-to-mf";
        let wave = Arc::new(AtomicU32::new(7));
        wave_flags().lock().insert(route.into(), wave.clone());
        let mut gdr = true;
        assert!(disable_gdr_route(route, &mut gdr, &wave));
        assert!(!gdr);
        assert_eq!(wave.load(Ordering::SeqCst), 0);
        assert!(!wave_flags().lock().contains_key(route));
    }

    #[cfg(windows)]
    #[test]
    fn balanced_fallback_does_not_claim_a_gdr_downgrade() {
        let route = "test:balanced-to-mf";
        wave_flags().lock().remove(route);
        let wave = Arc::new(AtomicU32::new(0));
        let mut gdr = false;
        assert!(!disable_gdr_route(route, &mut gdr, &wave));
        assert!(!gdr);
        assert!(!wave_flags().lock().contains_key(route));
    }

    /// One synthetic Annex-B NAL: start code + header byte (type in the
    /// low 5 bits) + a body that can't fake a start code.
    fn nal(ty: u8, len: usize, four_byte_sc: bool) -> Vec<u8> {
        let mut v = if four_byte_sc {
            vec![0, 0, 0, 1]
        } else {
            vec![0, 0, 1]
        };
        v.push(ty);
        v.extend(std::iter::repeat_n(0xAB, len));
        v
    }

    /// The slice splitter's contract on a synthetic AU: cuts land only
    /// before slice NALs, parameter sets and SEI travel with the slice
    /// that follows them, the ranges partition the input byte-exactly,
    /// and an unsplittable unit ships whole.
    #[test]
    fn splitter_cuts_only_at_slices_and_partitions_exactly() {
        let sps = nal(7, 10, true);
        let pps = nal(8, 4, true);
        let s1 = nal(5, 900, false);
        let s2 = nal(5, 900, false);
        let sei = nal(6, 8, false);
        let s3 = nal(1, 900, false);
        let au: Vec<u8> = [sps.clone(), pps.clone(), s1.clone(), s2, sei.clone(), s3].concat();
        let chunks = split_annexb_paced(&au, 1000);
        let rebuilt: Vec<u8> = chunks
            .iter()
            .flat_map(|r| au[r.clone()].iter().copied())
            .collect();
        assert_eq!(rebuilt, au, "chunks partition the unit byte-exactly");
        assert_eq!(chunks.len(), 3, "three slice-anchored chunks: {chunks:?}");
        assert!(
            chunks[0].len() >= sps.len() + pps.len() + s1.len(),
            "SPS/PPS glued to the first slice"
        );
        let c2 = &au[chunks[2].clone()];
        assert_eq!(&c2[..sei.len()], &sei[..], "SEI travels with its slice");
        for r in &chunks {
            let c = &au[r.clone()];
            assert!(
                c.starts_with(&[0, 0, 1]) || c.starts_with(&[0, 0, 0, 1]),
                "every chunk begins at a start code"
            );
        }
        // A unit with one oversized slice can't split below slice
        // granularity — it ships whole.
        let solo = [nal(7, 10, true), nal(8, 4, true), nal(5, 5000, false)].concat();
        let whole = split_annexb_paced(&solo, 1000);
        assert_eq!(whole.len(), 1);
        assert_eq!(whole[0], 0..solo.len());
        // Degenerate inputs never panic and never split.
        assert_eq!(split_annexb_paced(&[], 1000).len(), 1);
        assert_eq!(split_annexb_paced(&[0, 0], 1000)[0], 0..2);
    }

    /// The OpenH264-specific incremental-feed property on a REAL bitstream: an encoder
    /// with a slice cap emits multi-slice units, the splitter cuts them,
    /// and an OpenH264 decoder fed **chunk by chunk** still decode every
    /// picture cleanly. This does not generalize to WebCodecs or NVDEC, which
    /// require complete access units; pacing remains experimental until the
    /// receiver has an explicit completion contract.
    #[test]
    fn openh264_accepts_paced_slice_chunks_incrementally() {
        use openh264::encoder::{
            BitRate, Encoder, EncoderConfig, FrameRate, RateControlMode, UsageType,
        };
        let (w, h) = (640usize, 480usize);
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(8_000_000))
            .max_frame_rate(FrameRate::from_hz(30.0))
            .max_slice_len(4 * 1024);
        let mut enc =
            Encoder::with_api_config(openh264::OpenH264API::from_source(), config).expect("enc");
        let mut dec = openh264::decoder::Decoder::with_api_config(
            openh264::OpenH264API::from_source(),
            openh264::decoder::DecoderConfig::new(),
        )
        .expect("dec");
        let mut yuv = vec![128u8; w * h + 2 * ((w / 2) * (h / 2))];
        let mut saw_multi = false;
        let mut decoded = 0u32;
        let mut on_last_chunk = 0u32;
        for i in 0..10u32 {
            // Encodable-but-busy content (marching stripes + texture) so
            // rate control never frame-skips, while slices still fill to
            // the cap. (Pure noise at this bitrate makes openh264 skip
            // alternate frames entirely — a rate-control artifact that
            // says nothing about chunked feeding.)
            for (j, v) in yuv[..w * h].iter_mut().enumerate() {
                let row = j / w;
                let stripe = (row as u32 + i * 3) % 32 < 16;
                let texture = ((j as u32).wrapping_mul(31) >> 3) % 48;
                *v = if stripe {
                    170 + (texture as u8 / 2)
                } else {
                    40 + texture as u8
                };
            }
            let au = enc
                .encode(&I420Frame { buf: &yuv, w, h })
                .expect("encode")
                .to_vec();
            let chunks = split_annexb_paced(&au, 4 * 1024);
            let rebuilt: Vec<u8> = chunks
                .iter()
                .flat_map(|r| au[r.clone()].iter().copied())
                .collect();
            assert_eq!(rebuilt, au, "real-bitstream partition is byte-exact");
            if chunks.len() > 1 {
                saw_multi = true;
            }
            let last = chunks.len() - 1;
            for (ci, r) in chunks.into_iter().enumerate() {
                if dec
                    .decode(&au[r])
                    .expect("each paced chunk decodes cleanly")
                    .is_some()
                {
                    decoded += 1;
                    if ci == last {
                        on_last_chunk += 1;
                    }
                }
            }
        }
        assert!(saw_multi, "the slice cap produced multi-chunk units");
        assert!(
            decoded >= 9,
            "pictures completed across chunk-by-chunk feeding ({decoded}/10)"
        );
        // The zero-added-latency property: openh264 completes a picture by
        // macroblock accounting, so it surfaces on the SAME frame's final
        // chunk — never held for the next AU. This is what lets the live
        // viewer decode paced chunks as they arrive, no coalescing stage.
        assert!(
            on_last_chunk >= decoded.saturating_sub(1),
            "pictures surface on their own frame's last chunk ({on_last_chunk}/{decoded})"
        );
    }

    fn fb(recv_fps: u32, decode_fails: u32, queue_depth: u32) -> RecvFeedback {
        RecvFeedback {
            recv_fps,
            decode_fails,
            queue_depth,
            est_kbps: 0,
            delay_trend_us_per_s: 0,
            at: Instant::now(),
        }
    }

    /// The AIMD contract: two congested reports cut multiplicatively
    /// (jumping to just under a measured estimate when one exists), the
    /// hold window damps oscillation, and recovery climbs additively
    /// only after a sustained clean streak.
    #[test]
    fn rate_adapt_cuts_fast_climbs_slow_and_respects_the_estimate() {
        let ceiling = 40_000_000u32;
        let congested = RecvFeedback {
            queue_depth: 12,
            ..fb(60, 0, 12)
        };
        let mut st = RateAdaptState::default();
        let t0 = Instant::now();
        // One bad report is a hiccup, not a verdict.
        assert_eq!(
            rate_adapt_step(&mut st, &congested, 60, ceiling, ceiling, t0),
            None
        );
        // The second cuts ×0.7.
        assert_eq!(
            rate_adapt_step(&mut st, &congested, 60, ceiling, ceiling, t0),
            Some(28_000_000)
        );
        // Inside the hold window nothing moves, evidence or not.
        assert_eq!(
            rate_adapt_step(&mut st, &congested, 60, 28_000_000, ceiling, t0),
            None
        );
        assert_eq!(
            rate_adapt_step(&mut st, &congested, 60, 28_000_000, ceiling, t0),
            None
        );
        // Past the hold, with a measured estimate as the witness, the cut
        // lands just under it instead of feeling its way down in steps.
        let est = RecvFeedback {
            est_kbps: 10_000,
            ..fb(60, 0, 12)
        };
        // The bad streak carried through the hold, so the first post-hold
        // report with congestion evidence steps immediately.
        let t1 = t0 + RATE_HOLD + Duration::from_secs(1);
        let cut = rate_adapt_step(&mut st, &est, 60, 28_000_000, ceiling, t1)
            .expect("estimate-guided cut");
        assert_eq!(cut, 8_500_000, "85% of the measured 10 Mbps");
        // Clean reports climb additively — and only after the streak.
        let clean = fb(60, 0, 0);
        let t2 = t1 + RATE_HOLD + Duration::from_secs(1);
        let mut up = None;
        for _ in 0..RATE_GOOD_STREAK {
            up = rate_adapt_step(&mut st, &clean, 60, cut, ceiling, t2);
        }
        let up = up.expect("climb after the streak");
        assert_eq!(up, cut + (ceiling / 12).max(500_000));
        assert!(up < ceiling, "climb is additive, not a jump home");
        // A delay ramp alone (queue growing before loss) counts as
        // congestion evidence.
        let ramping = RecvFeedback {
            delay_trend_us_per_s: 30_000,
            ..fb(60, 0, 0)
        };
        let mut st2 = RateAdaptState::default();
        let t3 = t2 + RATE_HOLD + Duration::from_secs(1);
        assert_eq!(
            rate_adapt_step(&mut st2, &ramping, 60, ceiling, ceiling, t3),
            None
        );
        assert_eq!(
            rate_adapt_step(&mut st2, &ramping, 60, ceiling, ceiling, t3),
            Some(28_000_000)
        );
    }

    /// The reservation rule: automatic bitrate changes act only where
    /// they are beneficial in every case for the mode's use case — Game
    /// by default; everything else opt-in, off means off.
    #[test]
    fn rate_adapt_is_reserved_to_game_by_default() {
        assert!(rate_adapt_allowed(true, RateAdaptMode::GameOnly));
        assert!(
            !rate_adapt_allowed(false, RateAdaptMode::GameOnly),
            "balanced/studio keep the picked quality unless opted in"
        );
        assert!(rate_adapt_allowed(false, RateAdaptMode::All));
        assert!(!rate_adapt_allowed(true, RateAdaptMode::Off));
    }

    /// The wave chooser: a first loss heals with the smooth default; a
    /// second within the spell window shortens the heal to 3 frames; the
    /// flag write itself restarts an in-flight wave.
    #[test]
    fn wave_length_shortens_in_a_lossy_spell() {
        let vb = VideoBridge::new();
        let flag = std::sync::Arc::new(AtomicU32::new(0));
        wave_flags()
            .lock()
            .insert("wave-spell-test".into(), flag.clone());
        vb.route_wave_or_refresh("wave-spell-test");
        assert_eq!(
            flag.load(Ordering::SeqCst),
            default_wave_frames(60),
            "one-off loss keeps the smooth default"
        );
        vb.route_wave_or_refresh("wave-spell-test");
        assert_eq!(
            flag.load(Ordering::SeqCst),
            3,
            "a second loss inside the window shortens the heal"
        );
        wave_flags().lock().remove("wave-spell-test");
    }

    #[test]
    fn auto_adapt_steps_down_after_a_sustained_stall_and_holds_between_steps() {
        // The field failure: a 60 fps encode arriving at 0–6 fps for tens of
        // seconds while the sender kept pushing full resolution.
        let auto = AutoAdapt::new();
        let t0 = Instant::now();
        // Two struggling reports: not yet a verdict.
        assert_eq!(auto.observe(&fb(2, 2, 0), 60, t0), None);
        assert_eq!(auto.observe(&fb(0, 1, 0), 60, t0), None);
        assert!(auto.edge_cap().is_none());
        // Third consecutive: step down one rung.
        assert_eq!(auto.observe(&fb(4, 0, 0), 60, t0), Some((0, 2560)));
        assert_eq!(auto.edge_cap(), Some(2560));
        // Still struggling immediately after — held by the settle window,
        // no second step until it has had time to show up in feedback.
        for _ in 0..5 {
            assert_eq!(auto.observe(&fb(0, 0, 0), 60, t0), None);
        }
        // Past the hold, the sustained stall steps again.
        let t1 = t0 + AUTO_DOWN_HOLD;
        assert_eq!(auto.observe(&fb(0, 0, 0), 60, t1), Some((2560, 1920)));
        // A healthy report in a bad streak resets it — no flappy verdicts.
        let t2 = t1 + AUTO_DOWN_HOLD;
        assert_eq!(auto.observe(&fb(1, 0, 0), 60, t2), None);
        assert_eq!(auto.observe(&fb(1, 0, 0), 60, t2), None);
        assert_eq!(auto.observe(&fb(55, 0, 0), 60, t2), None); // healthy
        assert_eq!(auto.observe(&fb(1, 0, 0), 60, t2), None);
        assert_eq!(auto.observe(&fb(1, 0, 0), 60, t2), None);
        assert_eq!(auto.edge_cap(), Some(1920), "reset streak must not step");
    }

    #[test]
    fn auto_adapt_recovers_slowly_and_stops_at_the_floor() {
        let auto = AutoAdapt::new();
        let t0 = Instant::now();
        // Drive it to the floor.
        let mut t = t0;
        for _ in 0..AUTO_EDGES.len() {
            t += AUTO_DOWN_HOLD;
            for _ in 0..AUTO_BAD_STREAK {
                auto.observe(&fb(0, 0, 40), 60, t);
            }
        }
        assert_eq!(auto.edge_cap(), Some(*AUTO_EDGES.last().unwrap()));
        // Still bad at the floor: nothing further to give, no churn.
        t += AUTO_DOWN_HOLD;
        for _ in 0..10 {
            assert_eq!(auto.observe(&fb(0, 0, 40), 60, t), None);
        }
        // Sustained health past the up-hold steps back up exactly one rung
        // per streak — slow up, so the picture never oscillates.
        t += AUTO_UP_HOLD;
        let mut stepped = None;
        for _ in 0..AUTO_GOOD_STREAK {
            stepped = auto.observe(&fb(58, 0, 0), 60, t);
        }
        assert_eq!(stepped, Some((960, 1280)));
        // And from the top rung, recovery lifts the cap entirely.
        let auto = AutoAdapt::new();
        let t1 = Instant::now();
        for _ in 0..AUTO_BAD_STREAK {
            auto.observe(&fb(0, 0, 0), 60, t1);
        }
        assert_eq!(auto.edge_cap(), Some(2560));
        let t2 = t1 + AUTO_UP_HOLD;
        let mut lifted = None;
        for _ in 0..AUTO_GOOD_STREAK {
            lifted = auto.observe(&fb(58, 0, 0), 60, t2);
        }
        assert_eq!(lifted, Some((2560, 0)));
        assert!(auto.edge_cap().is_none());
    }

    #[test]
    fn h264_ladder_picks_a_backend_that_emits_a_frame() {
        // Runs the real step-down ladder: it tries each hardware encoder
        // (NVENC/VA-API/…) and frame-send-tests it; on a box without a GPU
        // those open or test-fail and it lands on software openh264. Either
        // way the returned backend must actually encode a frame — that's the
        // contract the whole ladder exists to guarantee.
        let (w, h) = (640u32, 480u32);
        let mut codec = make_h264_codec(w, h, 30, Tune::default()).expect("a working backend");
        let grey = vec![128u8; (w * h) as usize + 2 * ((w / 2 * h / 2) as usize)];
        // Forced as an IDR — a keyframe unit must come out within a bounded
        // run of calls. An async hardware backend delivers on a later call's
        // drain by design, and under parallel test load (another test holding
        // the same GPU encoder) output can lag several grace windows.
        let mut key_unit = false;
        for _ in 0..10 {
            let out = codec
                .encode_yuv(&grey, w as usize, h as usize, true)
                .expect("encode");
            key_unit |= out.units.iter().any(|(d, key)| !d.is_empty() && *key);
            if key_unit {
                break;
            }
        }
        assert!(
            key_unit,
            "ladder backend ({}) must emit a keyframe",
            codec.label()
        );
    }

    #[test]
    fn fit_within_caps_the_long_edge_and_keeps_aspect() {
        assert_eq!(fit_within(3840, 2160, 1280), (1280, 720));
        assert_eq!(fit_within(1080, 1920, 1280), (720, 1280));
        // Already small → untouched (never upscaled).
        assert_eq!(fit_within(800, 600, 1280), (800, 600));
        assert_eq!(fit_within(0, 0, 1280), (0, 0));
    }

    // A buffer whose every byte is distinct, so any reorder (rotation) is
    // detectable and a pass-through is provably identical.
    fn distinct_rgba(w: u32, h: u32) -> Vec<u8> {
        (0..(w * h * 4)).map(|i| i as u8).collect()
    }

    #[test]
    fn orient_passes_through_at_zero_degrees() {
        // Every presentation backend (and DXGI at IDENTITY) reports 0: pixels
        // and dims are returned untouched, no rotation applied.
        let buf = distinct_rgba(4, 2);
        let (out, ow, oh) = orient_to_monitor(buf.clone(), 4, 2, 0);
        assert_eq!((ow, oh), (4, 2));
        assert_eq!(out, buf);
    }

    #[test]
    fn orient_rotates_90_and_270_clockwise_swapping_dims() {
        // Raw DXGI landscape scan-out (4×2) of a portrait panel: a 90 or 270
        // report drives one/three clockwise quarter-turns → 2×4 upright.
        let buf = distinct_rgba(4, 2);

        let (cw90, ow, oh) = orient_to_monitor(buf.clone(), 4, 2, 90);
        assert_eq!((ow, oh), (2, 4), "90: dims swap");
        assert_eq!(cw90, allmystuff_pixels::rotate_rgba(&buf, 4, 2, 1).0);

        let (cw270, ow, oh) = orient_to_monitor(buf.clone(), 4, 2, 270);
        assert_eq!((ow, oh), (2, 4), "270: dims swap");
        assert_eq!(cw270, allmystuff_pixels::rotate_rgba(&buf, 4, 2, 3).0);

        // 90 and 270 are distinct rotations (direction matters).
        assert_ne!(cw90, cw270);
    }

    #[test]
    fn orient_flips_180_preserving_dims() {
        // 180 is dimensionally invisible but reorders every pixel.
        let buf = distinct_rgba(4, 2);
        let (out, ow, oh) = orient_to_monitor(buf.clone(), 4, 2, 180);
        assert_eq!((ow, oh), (4, 2), "180: dims unchanged");
        assert_eq!(out, allmystuff_pixels::rotate_rgba(&buf, 4, 2, 2).0);
        assert_ne!(out, buf, "180: pixels reordered");
    }

    #[test]
    fn orient_maps_degrees_to_clockwise_turns() {
        // The committed DXGI mapping, end to end: turns = (deg/90) mod 4,
        // clockwise. 90→1, 180→2, 270→3 — the inverse of the display's CCW
        // rotation, matching the canonical Desktop Duplication sample.
        let buf = distinct_rgba(4, 2);
        for (deg, turns) in [(0u32, 0u8), (90, 1), (180, 2), (270, 3)] {
            let got = orient_to_monitor(buf.clone(), 4, 2, deg).0;
            let want = allmystuff_pixels::rotate_rgba(&buf, 4, 2, turns).0;
            assert_eq!(got, want, "deg={deg} must equal {turns} CW turns");
        }
    }

    #[test]
    fn jpeg_encoder_produces_a_jpeg() {
        let rgba = vec![128u8; 8 * 8 * 4];
        let jpeg = encode_jpeg(&rgba, 8, 8, JPEG_QUALITY).expect("encode");
        // SOI marker.
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    }

    fn test_frame_encoder() -> FrameEncoder {
        FrameEncoder::new(
            "r",
            Tune::default(),
            Arc::new(AtomicBool::new(false)),
            AutoAdapt::new(),
        )
    }

    #[test]
    fn encoder_skips_unchanged_frames_and_keeps_seq_for_sent_ones() {
        let mut stats = StreamStats::new("r", VideoMode::Mjpeg);
        let mut enc = test_frame_encoder();
        let a = vec![10u8; 8 * 8 * 4];
        let first = enc
            .encode(a.clone(), 8, 8, &mut stats)
            .unwrap()
            .expect("first sends");
        assert_eq!(first.seq, 0);
        // Same pixels again, inside the refresh window → skipped.
        assert!(enc.encode(a.clone(), 8, 8, &mut stats).unwrap().is_none());
        assert_eq!(stats.static_skipped, 1, "the skip is counted");
        // Changed pixels → sent, with the next seq (skips don't burn one).
        let b = vec![200u8; 8 * 8 * 4];
        let second = enc
            .encode(b, 8, 8, &mut stats)
            .unwrap()
            .expect("change sends");
        assert_eq!(second.seq, 1);
        assert!(stats.bytes > 0, "emitted bytes are counted");
    }

    #[test]
    fn encoder_resends_after_the_refresh_interval() {
        let mut stats = StreamStats::new("r", VideoMode::Mjpeg);
        let mut enc = test_frame_encoder();
        let a = vec![10u8; 8 * 8 * 4];
        enc.encode(a.clone(), 8, 8, &mut stats)
            .unwrap()
            .expect("first sends");
        enc.last_sent = Some(Instant::now() - STATIC_REFRESH);
        let refreshed = enc.encode(a, 8, 8, &mut stats).unwrap();
        assert_eq!(refreshed.expect("refresh resends").seq, 1);
    }

    #[test]
    fn a_refresh_ask_resends_an_unchanged_frame_immediately() {
        let mut stats = StreamStats::new("r", VideoMode::Mjpeg);
        let refresh = Arc::new(AtomicBool::new(false));
        let mut enc = FrameEncoder::new("r", Tune::default(), refresh.clone(), AutoAdapt::new());
        let a = vec![10u8; 8 * 8 * 4];
        enc.encode(a.clone(), 8, 8, &mut stats)
            .unwrap()
            .expect("first sends");
        assert!(
            enc.encode(a.clone(), 8, 8, &mut stats).unwrap().is_none(),
            "unchanged inside the window is skipped"
        );
        refresh.store(true, Ordering::SeqCst);
        assert!(
            enc.encode(a.clone(), 8, 8, &mut stats).unwrap().is_some(),
            "the viewer's refresh ask overrides the unchanged gate"
        );
        assert!(!refresh.load(Ordering::SeqCst), "the ask is one-shot");
    }

    #[test]
    fn tune_clamps_each_dial_into_its_lane() {
        let t = Tune {
            max_edge: Some(800),
            bitrate: Some(12_000_000),
            fps: Some(60),
            link: LinkClass::default(),
            game: false,
            mode: None,
        };
        assert_eq!(t.fps(), 60);
        assert_eq!(t.h264_edge(), 800);
        // Both encodings honour the Res pick now (parity), each to the 4K
        // hard cap.
        assert_eq!(t.mjpeg_edge(), 800);
        // The Rate pick maps to a JPEG quality for MJPEG.
        assert_eq!(t.jpeg_quality(), mjpeg_quality_for(12_000_000));
        let big = Tune {
            max_edge: Some(9999),
            ..Tune::default()
        };
        assert_eq!(big.h264_edge(), 3840);
        assert_eq!(big.mjpeg_edge(), 3840);
        let auto = Tune::default();
        assert_eq!(auto.fps(), target_fps_for(LinkClass::Unknown, false));
        assert_eq!(auto.h264_edge(), h264_max_edge());
        // Untuned MJPEG defaults to HD, and untuned quality is neutral.
        assert_eq!(auto.mjpeg_edge(), mjpeg_max_edge());
        assert_eq!(auto.jpeg_quality(), JPEG_QUALITY);
    }

    #[test]
    fn lan_gate_raises_only_the_automatic_dials() {
        // A LAN link earns the 60 fps / 80 Mbps automatic dials; off-LAN
        // (and unknown, i.e. ICE not settled or an old daemon) stays at
        // the conservative 30 / 40 M — the open-loop-transport rule.
        let lan = Tune {
            link: LinkClass::Lan,
            ..Tune::default()
        };
        assert_eq!(lan.fps(), 60);
        assert_eq!(Tune::default().fps(), 30);
        assert_eq!(h264_bitrate_for(3840, 2160, 60, LinkClass::Lan), 79_626_240);
        assert_eq!(
            h264_bitrate_for(3840, 2160, 60, LinkClass::Unknown),
            40_000_000
        );
        assert_eq!(h264_bitrate_for(3840, 2160, 60, LinkClass::Wan), 40_000_000);
        // An explicit viewer Tune bypasses the gate on any link.
        let pinned = Tune {
            fps: Some(48),
            bitrate: Some(60_000_000),
            link: LinkClass::Wan,
            ..Tune::default()
        };
        assert_eq!(pinned.fps(), 48);
        assert_eq!(pinned.bitrate, Some(60_000_000));
    }

    #[test]
    fn rate_pill_maps_to_a_monotonic_jpeg_quality() {
        // The console's Rate pills, softest → sharpest, never decreasing.
        let q: Vec<u8> = [4, 8, 15, 25, 40]
            .iter()
            .map(|m| mjpeg_quality_for(m * 1_000_000))
            .collect();
        assert!(q.windows(2).all(|w| w[0] <= w[1]), "monotonic: {q:?}");
        assert!(*q.first().unwrap() < JPEG_QUALITY); // "Speed" softer than neutral
        assert!(*q.last().unwrap() > JPEG_QUALITY); // "Quality" sharper
    }

    #[test]
    fn game_mode_dials_raise_fps_and_tighten_bursts() {
        // The pure halves of the game-mode preset (the env read is a
        // process-wide LazyLock; tests exercise the logic directly).
        assert_eq!(auto_fps(LinkClass::Lan, false), 60);
        assert_eq!(auto_fps(LinkClass::Wan, false), 30);
        assert_eq!(auto_fps(LinkClass::Unknown, false), 30);
        assert_eq!(
            auto_fps(LinkClass::Wan, true),
            60,
            "game mode floors 60 off-LAN"
        );
        assert_eq!(auto_fps(LinkClass::Unknown, true), 60);
        // Standard posture: 2× peak over ~1 s; game mode: 1.5× over ~½ s.
        assert_eq!(burst_bounds(40_000_000, false), (80_000_000, 40_000_000));
        assert_eq!(burst_bounds(40_000_000, true), (60_000_000, 20_000_000));
    }

    #[test]
    fn adaptive_idr_relaxes_only_on_confirmed_health() {
        let fresh = |decode_fails, queue_depth| {
            Some(RecvFeedback {
                recv_fps: 30,
                decode_fails,
                queue_depth,
                est_kbps: 0,
                delay_trend_us_per_s: 0,
                at: Instant::now(),
            })
        };
        // No feedback at all → the tight floor (today's behaviour).
        assert_eq!(adaptive_idr_ms(None), IDR_MS_TIGHT);
        // Clean + draining → relax.
        assert_eq!(adaptive_idr_ms(fresh(0, 0)), IDR_MS_RELAXED);
        assert_eq!(adaptive_idr_ms(fresh(0, 8)), IDR_MS_RELAXED);
        // Any decode failure → tighten.
        assert_eq!(adaptive_idr_ms(fresh(1, 0)), IDR_MS_TIGHT);
        // A backed-up queue → tighten.
        assert_eq!(adaptive_idr_ms(fresh(0, 9)), IDR_MS_TIGHT);
        // Stale feedback (viewer went quiet) → never holds it relaxed.
        let stale = Some(RecvFeedback {
            recv_fps: 30,
            decode_fails: 0,
            queue_depth: 0,
            est_kbps: 0,
            delay_trend_us_per_s: 0,
            at: Instant::now() - (FEEDBACK_FRESH + Duration::from_secs(1)),
        });
        assert_eq!(adaptive_idr_ms(stale), IDR_MS_TIGHT);
    }

    #[test]
    fn receiver_feedback_is_recorded_latest_wins_and_clears_with_the_route() {
        let vb = VideoBridge::new();
        assert!(vb.latest_feedback("r1").is_none());
        vb.note_feedback("r1", 28, 3, 1, 0, 0);
        let fb = vb.latest_feedback("r1").expect("recorded");
        assert_eq!((fb.recv_fps, fb.decode_fails, fb.queue_depth), (28, 3, 1));
        // A fresher report replaces the old one.
        vb.note_feedback("r1", 30, 0, 0, 0, 0);
        assert_eq!(vb.latest_feedback("r1").map(|f| f.decode_fails), Some(0));
        // Tearing the route down drops its feedback (no unbounded growth).
        vb.stop("r1");
        assert!(vb.latest_feedback("r1").is_none());
    }

    #[test]
    fn existing_capture_worker_is_joined_before_successor_reuses_route_id() {
        fn fake_route(exited: Arc<AtomicBool>) -> RouteVideo {
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = stop.clone();
            let thread = std::thread::spawn(move || {
                while !thread_stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                exited.store(true, Ordering::SeqCst);
            });
            RouteVideo {
                stop,
                thread: Some(thread),
                mode: VideoMode::Mjpeg,
                source: VideoSource::Screen(None),
                on_packet: Arc::new(|_| true),
                on_status: Arc::new(|_, _| {}),
                refresh: Arc::new(AtomicBool::new(false)),
                idr_ms: Arc::new(AtomicU64::new(IDR_MS_TIGHT)),
                tune: Tune::default(),
                auto: AutoAdapt::new(),
            }
        }

        let bridge = VideoBridge::new();
        let old_exited = Arc::new(AtomicBool::new(false));
        bridge
            .routes
            .lock()
            .insert("same-id".into(), fake_route(old_exited.clone()));

        let displaced = bridge
            .take_existing_capture("same-id")
            .expect("predecessor registry entry");
        assert!(!bridge.routes.lock().contains_key("same-id"));
        drop(displaced);
        assert!(
            old_exited.load(Ordering::SeqCst),
            "predecessor is stopped and joined before successor insertion"
        );

        let successor_exited = Arc::new(AtomicBool::new(false));
        bridge
            .routes
            .lock()
            .insert("same-id".into(), fake_route(successor_exited.clone()));
        assert!(bridge.routes.lock().contains_key("same-id"));
        bridge.stop("same-id");
        assert!(successor_exited.load(Ordering::SeqCst));
    }

    #[test]
    fn fit_within_even_forces_even_edges() {
        assert_eq!(fit_within_even(3024, 1964, 1920), (1920, 1246));
        assert_eq!(fit_within_even(1919, 1081, 1920), (1918, 1080));
        assert_eq!(fit_within_even(1, 1, 1920), (2, 2));
    }

    #[test]
    fn a_frameless_session_pump_errors_at_the_deadline() {
        let stop = AtomicBool::new(false);
        // Sender stays alive and silent: the pump must give up on the
        // deadline, not on a disconnect.
        let (_tx, rx) = mpsc::channel::<(Vec<u8>, u32, u32)>();
        let mut stats = StreamStats::new("r", VideoMode::Mjpeg);
        let mut enc = healing_with(StreamEncoder::Mjpeg(test_frame_encoder()));
        let mut reporter = StatusReporter::new(Arc::new(|_, _| {}));
        let err = pump_frames_with_stall(
            &stop,
            30,
            &rx,
            |f: (Vec<u8>, u32, u32)| f,
            &|_| true,
            &mut enc,
            &mut stats,
            &mut reporter,
            VideoStatusState::CameraFailed,
            Some(Duration::from_millis(300)),
            None,
        )
        .expect_err("a frameless session past the deadline must end");
        assert!(err.contains("no frame within"), "{err}");
    }

    #[test]
    fn h264_stream_emits_annexb_with_a_leading_idr() {
        let mut stats = StreamStats::new("r", VideoMode::H264);
        let mut enc = H264Stream::new(
            (64, 64),
            Tune::default(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(IDR_MS_TIGHT)),
            AutoAdapt::new(),
        )
        .expect("openh264 init");
        // A 64×64 solid frame → first unit out must be a key (IDR + SPS/PPS
        // in-band), Annex-B framed.
        let rgba = vec![128u8; 64 * 64 * 4];
        let packet = enc
            .encode(rgba, 64, 64, &mut stats)
            .expect("encode")
            .into_iter()
            .next()
            .expect("first frame emits");
        let VideoPacket::H264 { data, .. } = packet else {
            panic!("h264 stream emitted a jpeg");
        };
        assert!(data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1]));
        assert_eq!(stats.keyframes, 1, "first unit is a key");
        assert!(stats.bytes > 0);
    }

    /// A test-only backend that scripts its outcomes (including errors), for
    /// exercising the [`H264Stream`] drain/consumed logic and the
    /// [`HealingEncoder`] recovery without hardware. Off-script calls
    /// consume-and-emit-nothing (a healthy buffering encoder).
    struct ScriptedCodec {
        script: std::collections::VecDeque<Result<EncodeOutcome, String>>,
        calls: Arc<AtomicU32>,
        /// Every call's `force_idr` flag, in order — lets tests assert which
        /// re-encodes were IDRs and which were refinement passes.
        forced: Arc<Mutex<Vec<bool>>>,
    }

    impl ScriptedCodec {
        fn new(script: Vec<Result<EncodeOutcome, String>>) -> Self {
            ScriptedCodec {
                script: script.into(),
                calls: Arc::new(AtomicU32::new(0)),
                forced: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl H264Codec for ScriptedCodec {
        fn encode_yuv(
            &mut self,
            _yuv: &[u8],
            _w: usize,
            _h: usize,
            force_idr: bool,
        ) -> Result<EncodeOutcome, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.forced.lock().push(force_idr);
            self.script.pop_front().unwrap_or(Ok(EncodeOutcome {
                units: Vec::new(),
                consumed: true,
                input_ts: 0,
            }))
        }
        fn label(&self) -> &str {
            "scripted (test)"
        }
    }

    /// An [`H264Stream`] around an injected codec, budget-sized so a 64×64
    /// test frame never triggers the real-ladder rebuild.
    fn h264_stream_with(codec: Box<dyn H264Codec>) -> H264Stream {
        H264Stream {
            codec,
            budget_size: (64, 64),
            tune: Tune::default(),
            fps: 30,
            prev: Vec::new(),
            prev_size: (0, 0),
            last_sent: None,
            last_idr: None,
            last_emit: None,
            refresh: Arc::new(AtomicBool::new(false)),
            idr_ms: Arc::new(AtomicU64::new(IDR_MS_TIGHT)),
            auto: AutoAdapt::new(),
        }
    }

    #[test]
    fn h264_stream_emits_every_drained_unit_in_order() {
        // A loaded hardware encoder can hand back a small backlog in one
        // call. Every unit must reach the wire, in order — the old seam kept
        // only the newest, which snapped the viewer's reference chain.
        let mut stats = StreamStats::new("r", VideoMode::H264);
        let backlog = Ok(EncodeOutcome {
            units: vec![(vec![1, 1], false), (vec![2, 2], true), (vec![3, 3], false)],
            consumed: true,
            input_ts: 0,
        });
        let mut enc = h264_stream_with(Box::new(ScriptedCodec::new(vec![backlog])));
        let rgba = vec![128u8; 64 * 64 * 4];
        let packets = enc.encode(rgba, 64, 64, &mut stats).expect("encode");
        let datas: Vec<Vec<u8>> = packets
            .iter()
            .map(|p| match p {
                VideoPacket::H264 { data, .. } => data.clone(),
                VideoPacket::Jpeg(_) => panic!("jpeg from h264 stream"),
            })
            .collect();
        assert_eq!(datas, vec![vec![1, 1], vec![2, 2], vec![3, 3]]);
        assert_eq!(stats.keyframes, 1, "the drained key is counted");
        assert_eq!(stats.bytes, 6);
    }

    #[test]
    fn h264_stream_does_not_mark_unconsumed_frames_as_sent() {
        // A stalled hardware input queue refuses a frame. Its content must
        // NOT be remembered as sent — the same pixels next tick have to reach
        // the encoder instead of being static-skipped into a stale viewer.
        let mut stats = StreamStats::new("r", VideoMode::H264);
        let outcomes = vec![
            Ok(EncodeOutcome {
                units: vec![(vec![1], true)],
                consumed: true,
                input_ts: 0,
            }),
            Ok(EncodeOutcome {
                units: Vec::new(),
                consumed: false,
                input_ts: 0,
            }),
            Ok(EncodeOutcome {
                units: vec![(vec![2], false)],
                consumed: true,
                input_ts: 0,
            }),
        ];
        let codec = ScriptedCodec::new(outcomes);
        let calls = codec.calls.clone();
        let mut enc = h264_stream_with(Box::new(codec));
        let a = vec![128u8; 64 * 64 * 4];
        let b = vec![200u8; 64 * 64 * 4];
        assert_eq!(enc.encode(a, 64, 64, &mut stats).unwrap().len(), 1);
        assert!(enc
            .encode(b.clone(), 64, 64, &mut stats)
            .unwrap()
            .is_empty());
        let retried = enc.encode(b, 64, 64, &mut stats).unwrap();
        assert_eq!(retried.len(), 1, "retry reached the encoder and emitted");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "no static skip ate the retry"
        );
        assert_eq!(stats.static_skipped, 0);
    }

    /// A [`HealingEncoder`] around an injected [`StreamEncoder`], policy
    /// fresh, no rebuild override.
    fn healing_with(enc: StreamEncoder) -> HealingEncoder {
        let mode = enc.mode();
        HealingEncoder {
            enc,
            route_id: "r".into(),
            mode,
            tune: Tune::default(),
            refresh: Arc::new(AtomicBool::new(false)),
            idr_ms: Arc::new(AtomicU64::new(IDR_MS_TIGHT)),
            auto: AutoAdapt::new(),
            policy: RebuildPolicy::new(),
            rebuild_override: None,
        }
    }

    fn erroring_h264(msg: &str) -> StreamEncoder {
        StreamEncoder::H264(Box::new(h264_stream_with(Box::new(ScriptedCodec::new(
            vec![Err(msg.to_string())],
        )))))
    }

    #[test]
    fn a_short_capture_buffer_is_an_error_not_a_panic() {
        // panic = "abort" in release: an out-of-bounds slice on the capture
        // thread would kill the whole node. Short buffers must surface as
        // stream errors the healer can absorb.
        let mut stats = StreamStats::new("r", VideoMode::Mjpeg);
        let mut enc = StreamEncoder::Mjpeg(test_frame_encoder());
        let err = enc
            .encode(vec![0u8; 16], 64, 64, &mut stats)
            .expect_err("short buffer is rejected");
        assert!(err.contains("too short"), "{err}");
        // The rotator sidesteps a short buffer instead of indexing past it.
        let (out, w, h) = orient_to_monitor(vec![0u8; 16], 64, 64, 90);
        assert_eq!((out.len(), w, h), (16, 64, 64), "unrotated pass-through");
    }

    #[test]
    fn rebuild_policy_spaces_attempts_and_gives_up_after_the_budget() {
        let ms = Duration::from_millis;
        let mut p = RebuildPolicy::new();
        let t0 = Instant::now();
        assert!(
            matches!(p.on_error(t0), PolicyVerdict::Rebuild),
            "first error rebuilds"
        );
        assert!(
            matches!(p.on_error(t0 + ms(100)), PolicyVerdict::SkipFrame),
            "inside the spacing the frame is skipped, not blocked on"
        );
        assert!(matches!(
            p.on_error(t0 + REBUILD_SPACING),
            PolicyVerdict::Rebuild
        ));
        assert!(matches!(
            p.on_error(t0 + REBUILD_SPACING * 2),
            PolicyVerdict::Rebuild
        ));
        assert!(
            matches!(p.on_error(t0 + REBUILD_SPACING * 3), PolicyVerdict::GiveUp),
            "the budget is {REBUILD_MAX} rebuilds per window"
        );
        // A fresh window resets the budget.
        let t1 = t0 + REBUILD_WINDOW + REBUILD_SPACING * 3 + ms(1);
        assert!(matches!(p.on_error(t1), PolicyVerdict::Rebuild));
    }

    #[test]
    fn healing_encoder_rebuilds_and_the_stream_continues() {
        let mut stats = StreamStats::new("r", VideoMode::H264);
        let mut he = healing_with(erroring_h264("driver reset"));
        he.rebuild_override = Some(Box::new(|| {
            Ok(StreamEncoder::H264(Box::new(h264_stream_with(Box::new(
                ScriptedCodec::new(vec![Ok(EncodeOutcome {
                    units: vec![(vec![9], true)],
                    consumed: true,
                    input_ts: 0,
                })]),
            )))))
        }));
        let rgba = vec![128u8; 64 * 64 * 4];
        // The erroring frame is absorbed: no packets, no stream death, and
        // the refresh flag is armed so the viewer gets a clean entry.
        let healed = he.encode(rgba.clone(), 64, 64, &mut stats).expect("healed");
        assert!(healed.is_empty());
        assert!(he.refresh.load(Ordering::SeqCst), "rebuild arms a refresh");
        // The next frame rides the rebuilt encoder.
        let packets = he.encode(rgba, 64, 64, &mut stats).expect("encodes again");
        assert_eq!(packets.len(), 1, "the stream continues after the rebuild");
    }

    #[test]
    fn healing_encoder_gives_up_when_the_budget_is_spent() {
        let mut stats = StreamStats::new("r", VideoMode::H264);
        let mut he = healing_with(erroring_h264("device removed"));
        he.policy.window_start = Some(Instant::now());
        he.policy.rebuilds_in_window = REBUILD_MAX;
        let rgba = vec![128u8; 64 * 64 * 4];
        let err = he
            .encode(rgba, 64, 64, &mut stats)
            .expect_err("budget spent → the stream ends with the reason");
        assert!(err.contains("failed permanently"), "{err}");
    }

    #[test]
    fn a_quiet_h264_stream_gets_an_idr_then_refinement_passes() {
        // One real frame, then silence. The pump must emit the convergence
        // IDR (~250 ms in) and then REFINE_PASSES *non-IDR* refinements —
        // the post-transition sharpening — and then go fully quiet.
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<(Vec<u8>, u32, u32)>();
        let mut stats = StreamStats::new("r", VideoMode::H264);
        let codec = ScriptedCodec::new(vec![
            Ok(EncodeOutcome {
                units: vec![(vec![1], true)],
                consumed: true,
                input_ts: 0,
            }),
            Ok(EncodeOutcome {
                units: vec![(vec![2], true)],
                consumed: true,
                input_ts: 0,
            }),
            Ok(EncodeOutcome {
                units: vec![(vec![3], false)],
                consumed: true,
                input_ts: 0,
            }),
            Ok(EncodeOutcome {
                units: vec![(vec![4], false)],
                consumed: true,
                input_ts: 0,
            }),
        ]);
        let forced = codec.forced.clone();
        let mut enc = healing_with(StreamEncoder::H264(Box::new(h264_stream_with(Box::new(
            codec,
        )))));
        let sent = Arc::new(Mutex::new(0u32));
        let sent_cb = sent.clone();
        let mut reporter = StatusReporter::new(Arc::new(|_, _| {}));
        tx.send((vec![128u8; 64 * 64 * 4], 64, 64)).unwrap();
        let stopper = stop.clone();
        let killer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2600));
            stopper.store(true, Ordering::SeqCst);
        });
        pump_frames_with_stall(
            &stop,
            30,
            &rx,
            |f: (Vec<u8>, u32, u32)| f,
            &move |_p| {
                *sent_cb.lock() += 1;
                true
            },
            &mut enc,
            &mut stats,
            &mut reporter,
            VideoStatusState::DisplayAsleep,
            None,
            None,
        )
        .expect("pump runs until stopped");
        killer.join().unwrap();
        assert_eq!(
            *sent.lock(),
            2 + u32::from(REFINE_PASSES),
            "frame + convergence IDR + {REFINE_PASSES} refinements"
        );
        assert_eq!(
            forced.lock().as_slice(),
            &[true, true, false, false],
            "refinements are non-IDR re-encodes"
        );
    }

    #[test]
    fn a_quiet_mjpeg_stream_resends_once_and_never_refines() {
        // MJPEG's quiesce resend stays a single frame — re-encoding
        // identical pixels to JPEG is byte-identical, so refinement is
        // H.264-only by design.
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<(Vec<u8>, u32, u32)>();
        let mut stats = StreamStats::new("r", VideoMode::Mjpeg);
        let mut enc = healing_with(StreamEncoder::Mjpeg(test_frame_encoder()));
        let sent = Arc::new(Mutex::new(0u32));
        let sent_cb = sent.clone();
        let mut reporter = StatusReporter::new(Arc::new(|_, _| {}));
        tx.send((vec![128u8; 8 * 8 * 4], 8, 8)).unwrap();
        let stopper = stop.clone();
        let killer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1400));
            stopper.store(true, Ordering::SeqCst);
        });
        pump_frames_with_stall(
            &stop,
            30,
            &rx,
            |f: (Vec<u8>, u32, u32)| f,
            &move |_p| {
                *sent_cb.lock() += 1;
                true
            },
            &mut enc,
            &mut stats,
            &mut reporter,
            VideoStatusState::DisplayAsleep,
            None,
            None,
        )
        .expect("pump runs until stopped");
        killer.join().unwrap();
        assert_eq!(*sent.lock(), 2, "the real frame + one quiesce resend");
    }

    #[test]
    fn h264_stream_rearms_a_refresh_ask_the_encoder_did_not_take() {
        // A viewer's refresh ask must survive a stalled encoder: the flag was
        // consumed on the way in, so an unconsumed frame has to re-arm it.
        let mut stats = StreamStats::new("r", VideoMode::H264);
        let mut enc = h264_stream_with(Box::new(ScriptedCodec::new(vec![Ok(EncodeOutcome {
            units: Vec::new(),
            consumed: false,
            input_ts: 0,
        })])));
        enc.refresh.store(true, Ordering::SeqCst);
        let rgba = vec![128u8; 64 * 64 * 4];
        let _ = enc.encode(rgba, 64, 64, &mut stats).unwrap();
        assert!(
            enc.refresh.load(Ordering::SeqCst),
            "the ask survives an unconsumed frame"
        );
    }
}
