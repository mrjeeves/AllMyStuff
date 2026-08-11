//! The audio media plane: capture and playback with `cpal` (plus the
//! platform loopback paths), so an active audio route actually moves sound
//! across the mesh.
//!
//! cpal's `Stream` is `!Send`, so each route runs on its own dedicated
//! thread that builds the stream, keeps it alive, and drops it on stop. The
//! mesh layer never touches a stream directly — it calls [`AudioBridge`]:
//!
//!  * **source side** — [`AudioBridge::start_capture`] records what the
//!    routed capability names ([`CaptureSource`]): the default microphone
//!    for a scanned input device, or — for the synthetic `system-audio`
//!    capability, "what this machine is playing" — the OS loopback of the
//!    default output (WASAPI loopback on Windows, the pulse server's
//!    monitor source on Linux; macOS has no OS loopback API and degrades
//!    to the default input, loudly). Buffers are down-mixed to mono i16
//!    and handed to a callback the mesh forwards as an [`AudioFrame`].
//!  * **sink side** — [`AudioBridge::start_playback`] opens the default
//!    output and drains a ring buffer that [`AudioBridge::feed`] fills from
//!    inbound frames (linear-resampled to the device rate).
//!
//! Because "routes active, no sound" is invisible from the route handshake,
//! both ends are deliberately loud about the things that go silently wrong
//! in the field: each stream logs **which device** it opened (the default
//! device may not be the one the user is thinking of), a mic capture that
//! produces nothing but zeros for its first seconds names the OS
//! microphone permission (macOS TCC and Windows privacy both deliver
//! silence rather than an error — a silent *system* capture is just a
//! quiet desktop, so that source never warns), and a playback that never
//! gets fed says so once instead of playing silence forever. The
//! flowing-state counters (`audio out/in` lines) ride the same
//! `ALLMYSTUFF_VIDEO_STATS` dial-in switch the video pipeline uses, at
//! debug otherwise.
//!
//! v1 simplifications, called out honestly: capture uses the category's
//! *default* device (mapping a specific scanned device to a cpal device by
//! name is a follow-up), and transport is mono. Both are noted in the
//! README.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;

use allmystuff_session::AudioFrame;

/// What a sourcing audio route records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSource {
    /// The default input device — a scanned microphone capability.
    Mic,
    /// This machine's own playback (the synthetic `system-audio`
    /// capability): the loopback of the default output.
    System,
}

/// Playout depth the inbound ring aims for — enough to ride normal
/// network jitter without underruns, small enough that audio stays in
/// step with the video stream (which drops stale frames and runs ~a
/// frame or two behind live). Lip-sync offsets under ~100 ms read as
/// "in sync".
const TARGET_DEPTH_MS: usize = 80;
/// Depth at which [`AudioBridge::feed`] trims the ring back to
/// [`TARGET_DEPTH_MS`]. The gap between the two is hysteresis: a burst
/// must overshoot meaningfully before a trim (an audible skip) is paid.
const MAX_DEPTH_MS: usize = 200;

// ---- the mesh's Opus audio lane (encode side) --------------------------

/// The audio lane's clock: Opus always runs a 48 kHz RTP clock.
pub(crate) const OPUS_RATE: u32 = 48_000;
/// 20 ms at 48 kHz — the canonical Opus frame.
pub(crate) const OPUS_FRAME_SAMPLES: usize = 960;
/// The same 20 ms, as the lane's RTP pacing value.
pub(crate) const OPUS_FRAME_US: u64 = 20_000;

/// Chops a capture stream (any rate, mono, buffers at the device's whim)
/// into the canonical 20 ms frames the mesh's Opus lane carries, encoding
/// each. Opus only accepts exact frame sizes, cpal promises nothing about
/// buffer cadence — this is the shim between them.
pub(crate) struct OpusStream {
    enc: opus::Encoder,
    /// 48 kHz mono samples awaiting a full frame.
    buf: Vec<i16>,
}

impl OpusStream {
    pub(crate) fn new() -> Result<Self, String> {
        let mut enc = opus::Encoder::new(OPUS_RATE, opus::Channels::Mono, opus::Application::Audio)
            .map_err(|e| e.to_string())?;
        // System audio is music as often as speech — give the codec room
        // (96 kbps mono is transparent for almost everything; still ~10×
        // smaller than the PCM frames the channel fallback ships).
        enc.set_bitrate(opus::Bitrate::Bits(96_000))
            .map_err(|e| e.to_string())?;
        Ok(OpusStream {
            enc,
            buf: Vec::with_capacity(OPUS_FRAME_SAMPLES * 2),
        })
    }

    /// Push one captured buffer; `emit` is called once per completed
    /// 20 ms frame with the encoded packet.
    pub(crate) fn push(&mut self, pcm: &[i16], rate: u32, mut emit: impl FnMut(Vec<u8>)) {
        let resampled;
        let samples = if rate == OPUS_RATE || rate == 0 {
            pcm
        } else {
            resampled = resample_linear(pcm, rate, OPUS_RATE);
            &resampled[..]
        };
        self.buf.extend_from_slice(samples);
        let mut off = 0;
        while self.buf.len() - off >= OPUS_FRAME_SAMPLES {
            let frame = &self.buf[off..off + OPUS_FRAME_SAMPLES];
            match self.enc.encode_vec(frame, 4000) {
                Ok(pkt) => emit(pkt),
                // A failed frame costs 20 ms of sound, never the stream.
                Err(e) => tracing::debug!("opus encode failed: {e}"),
            }
            off += OPUS_FRAME_SAMPLES;
        }
        self.buf.drain(..off);
    }
}

/// One running route's audio resources (a capture or a playback thread).
struct RouteAudio {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Present for playback routes: the ring the output stream drains and
    /// the device's output sample rate that `feed` resamples to.
    playback: Option<Playback>,
}

struct Playback {
    ring: Arc<Mutex<VecDeque<i16>>>,
    out_rate: Arc<AtomicU32>,
    /// Frames `feed` has accepted — the playback thread warns once when
    /// this is still zero seconds after the route went live.
    fed: Arc<AtomicU64>,
    /// Receive-side dial-in counters.
    stats: Mutex<LevelStats>,
}

impl Drop for RouteAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Windowed frames/peak counters shared by the capture and feed sides —
/// the audio equivalent of the video pipeline's dial-in lines.
struct LevelStats {
    since: Instant,
    frames: u32,
    peak: i16,
    /// Whether the all-zeros warning fired (once per stream).
    warned_silent: bool,
}

impl LevelStats {
    fn new() -> Self {
        LevelStats {
            since: Instant::now(),
            frames: 0,
            peak: 0,
            warned_silent: false,
        }
    }

    /// Count one buffer; every ~5 s emit the dial-in line via `log` and
    /// return whether the window was pure silence (for the caller's
    /// one-shot permission warning).
    fn note(&mut self, pcm: &[i16], log: impl FnOnce(String)) -> bool {
        const EVERY: Duration = Duration::from_secs(5);
        self.frames += 1;
        let peak = pcm.iter().map(|s| s.saturating_abs()).max().unwrap_or(0);
        self.peak = self.peak.max(peak);
        if self.since.elapsed() < EVERY {
            return false;
        }
        let secs = self.since.elapsed().as_secs_f64();
        log(format!(
            "{:.0} buffers/s · peak {:.0}%",
            self.frames as f64 / secs,
            self.peak as f64 * 100.0 / i16::MAX as f64,
        ));
        let silent = self.peak == 0;
        self.since = Instant::now();
        self.frames = 0;
        self.peak = 0;
        silent
    }
}

#[derive(Default)]
pub struct AudioBridge {
    // Capture and playback live in separate maps so a loopback route (this
    // machine's mic to its own speakers) can run both under one route id —
    // one map made the second insert silently stop the first's thread.
    captures: Mutex<HashMap<String, RouteAudio>>,
    playbacks: Mutex<HashMap<String, RouteAudio>>,
}

impl AudioBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin capturing `source` for `route_id`. `on_frame(mono_pcm,
    /// sample_rate)` is called for every captured buffer; the mesh wraps it
    /// into an [`AudioFrame`] and sends it to the peer.
    pub fn start_capture<F>(&self, route_id: String, source: CaptureSource, on_frame: F)
    where
        F: Fn(Vec<i16>, u32) + Send + Sync + 'static,
    {
        // Exactly one capture pump per route (the video bridge's discipline):
        // a duplicate StartMedia (the daemon redelivers an Offer once per
        // shared network) must not double-start audio capture — and with the
        // release profile's `panic = "abort"` a panic on a stray capture
        // thread aborts the host. Keyed on the captures map only, so a
        // loopback route (capture + playback under one id) is unaffected.
        if self.captures.lock().contains_key(&route_id) {
            tracing::debug!(
                "audio capture already running for {route_id}; ignoring duplicate start"
            );
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let id = route_id.clone();
        let thread = std::thread::spawn(move || {
            if let Err(e) = run_capture(&stop_thread, &id, source, on_frame) {
                tracing::warn!("audio capture for {id} stopped: {e}");
            }
        });
        // Insert, then join any displaced thread off the lock (RouteAudio::drop
        // joins) — never under the captures lock.
        let displaced = {
            let mut captures = self.captures.lock();
            captures.insert(
                route_id,
                RouteAudio {
                    stop,
                    thread: Some(thread),
                    playback: None,
                },
            )
        };
        drop(displaced);
    }

    /// Begin playing inbound audio for `route_id` on the default output.
    pub fn start_playback(&self, route_id: String) {
        // One playback pump per route, for the same reason as `start_capture`;
        // keyed on the playbacks map so a loopback route still runs both.
        if self.playbacks.lock().contains_key(&route_id) {
            tracing::debug!(
                "audio playback already running for {route_id}; ignoring duplicate start"
            );
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let ring = Arc::new(Mutex::new(VecDeque::<i16>::new()));
        let ring_thread = ring.clone();
        // 48 kHz is the near-universal default; the thread overwrites it
        // with the real device rate once the stream is built.
        let out_rate = Arc::new(AtomicU32::new(48_000));
        let out_rate_thread = out_rate.clone();
        let fed = Arc::new(AtomicU64::new(0));
        let fed_thread = fed.clone();
        let id = route_id.clone();
        let thread = std::thread::spawn(move || {
            if let Err(e) =
                run_playback(&stop_thread, &id, ring_thread, out_rate_thread, fed_thread)
            {
                tracing::warn!("audio playback for {id} stopped: {e}");
            }
        });
        let displaced = {
            let mut playbacks = self.playbacks.lock();
            playbacks.insert(
                route_id,
                RouteAudio {
                    stop,
                    thread: Some(thread),
                    playback: Some(Playback {
                        ring,
                        out_rate,
                        fed,
                        stats: Mutex::new(LevelStats::new()),
                    }),
                },
            )
        };
        // Join any displaced playback thread off the lock (RouteAudio::drop).
        drop(displaced);
    }

    /// Push an inbound frame into a playback route's ring (mono, resampled
    /// to the device rate). No-op if the route isn't a playback route.
    pub fn feed(&self, route_id: &str, frame: &AudioFrame) {
        let routes = self.playbacks.lock();
        let Some(pb) = routes.get(route_id).and_then(|r| r.playback.as_ref()) else {
            return;
        };
        if pb.fed.fetch_add(1, Ordering::Relaxed) == 0 {
            tracing::info!(
                "first audio frame for {route_id} ({} Hz, {} ch)",
                frame.sample_rate,
                frame.channels
            );
        }
        let mono = downmix(&frame.pcm, frame.channels);
        {
            let mut stats = pb.stats.lock();
            let id = route_id.to_string();
            stats.note(&mono, move |line| {
                stats_log(format!("audio in {id}: {line}"))
            });
        }
        let out_rate = pb.out_rate.load(Ordering::Relaxed);
        let samples = if frame.sample_rate == out_rate || frame.sample_rate == 0 {
            mono
        } else {
            resample_linear(&mono, frame.sample_rate, out_rate)
        };
        let mut ring = pb.ring.lock();
        // Keep playout close behind the live edge. The ring is a jitter
        // buffer, not a reservoir: whatever piles up beyond MAX_DEPTH_MS —
        // the device-open transient at session start, a network burst,
        // slow clock drift between the two ends — becomes *permanent* lag
        // behind the video stream (which drops stale frames) unless it's
        // cut. Trim the *oldest* samples back to TARGET_DEPTH_MS: one
        // small audible skip, and the audio is current again.
        if let Some(held_ms) = buffer_playback_samples(&mut ring, samples, out_rate) {
            tracing::debug!(
                "audio ring for {route_id} trimmed to {TARGET_DEPTH_MS} ms (held {} ms)",
                held_ms
            );
        }
    }

    pub fn stop(&self, route_id: &str) {
        // Drop runs the thread join + stream teardown — bind the removed
        // values so that join happens after each lock guard is released, never
        // under it (an unbound `remove(..);` drops while the guard is held).
        let removed_capture = self.captures.lock().remove(route_id);
        let removed_playback = self.playbacks.lock().remove(route_id);
        drop(removed_capture);
        drop(removed_playback);
    }

    #[allow(dead_code)]
    pub fn stop_all(&self) {
        self.captures.lock().clear();
        self.playbacks.lock().clear();
    }

    #[allow(dead_code)]
    pub fn is_running(&self, route_id: &str) -> bool {
        self.captures.lock().contains_key(route_id) || self.playbacks.lock().contains_key(route_id)
    }
}

/// Append one decoded buffer and keep playback close to the live edge.
///
/// The ring is a jitter buffer, not a reservoir: whatever piles up beyond
/// `MAX_DEPTH_MS` becomes permanent lag behind video (which drops stale
/// frames), so trim the oldest samples back to `TARGET_DEPTH_MS`. Keeping this
/// operation independent of the OS output stream also lets its boundary
/// behavior be tested without opening a real audio device.
fn buffer_playback_samples(
    ring: &mut VecDeque<i16>,
    samples: impl IntoIterator<Item = i16>,
    out_rate: u32,
) -> Option<usize> {
    ring.extend(samples);
    let samples_per_ms = out_rate as usize / 1000;
    let max = samples_per_ms * MAX_DEPTH_MS;
    let target = samples_per_ms * TARGET_DEPTH_MS;
    if ring.len() <= max.max(1) {
        return None;
    }
    let held_ms = ring.len() / samples_per_ms.max(1);
    let excess = ring.len().saturating_sub(target);
    ring.drain(..excess);
    Some(held_ms)
}

/// Route a dial-in line through the same switch as the video pipeline's:
/// info while `ALLMYSTUFF_VIDEO_STATS` is set, debug otherwise.
fn stats_log(line: String) {
    if crate::video::stats_to_info() {
        tracing::info!("{line}");
    } else {
        tracing::debug!("{line}");
    }
}

// ---- capture ----------------------------------------------------------

fn run_capture<F>(
    stop: &AtomicBool,
    route_id: &str,
    source: CaptureSource,
    on_frame: F,
) -> Result<(), String>
where
    F: Fn(Vec<i16>, u32) + Send + Sync + 'static,
{
    // The meter wraps every path; the `Arc` is so a failed loopback can
    // hand the same consumer to its fallback (cpal consumes the callback
    // even when the stream build fails).
    let on_frame = Arc::new(metered(route_id, source, on_frame));
    match source {
        CaptureSource::Mic => run_capture_cpal(stop, route_id, false, on_frame),
        CaptureSource::System => run_system_capture(stop, route_id, on_frame),
    }
}

/// Wrap a capture consumer with the level meter: the ~5 s dial-in line,
/// plus the one-shot pure-silence warning. The warning names the OS
/// microphone permission and only fires for [`CaptureSource::Mic`] —
/// macOS TCC / Windows privacy deliver silence rather than an error,
/// while a silent *system* capture is just a quiet desktop.
fn metered<F>(
    route_id: &str,
    source: CaptureSource,
    on_frame: F,
) -> impl Fn(Vec<i16>, u32) + Send + Sync + 'static
where
    F: Fn(Vec<i16>, u32) + Send + Sync + 'static,
{
    let stats = Mutex::new(LevelStats::new());
    let id = route_id.to_string();
    move |pcm: Vec<i16>, rate: u32| {
        {
            let mut stats = stats.lock();
            let line_id = id.clone();
            let silent = stats.note(&pcm, move |line| {
                stats_log(format!("audio out {line_id}: {line}"));
            });
            if silent && source == CaptureSource::Mic && !stats.warned_silent {
                stats.warned_silent = true;
                tracing::warn!(
                    "audio capture for {id} is producing pure silence — check the OS \
                     microphone permission for this app (and the default input device)"
                );
            }
        }
        on_frame(pcm, rate);
    }
}

/// System-audio capture on Windows: WASAPI loopback. An *input* stream
/// built on the default *output* device captures the machine's playback
/// mix (cpal raises `AUDCLNT_STREAMFLAGS_LOOPBACK` for render-device
/// input streams). One quirk to know when reading logs: loopback delivers
/// buffers only while something renders, so a fully silent desktop
/// produces no frames at all — that's normal, not a stall.
#[cfg(windows)]
fn run_system_capture<F>(stop: &AtomicBool, route_id: &str, on_frame: Arc<F>) -> Result<(), String>
where
    F: Fn(Vec<i16>, u32) + Send + Sync + 'static,
{
    match run_capture_cpal(stop, route_id, true, on_frame.clone()) {
        Err(e) => {
            tracing::warn!(
                "system-audio loopback for {route_id} failed ({e}) — capturing the default \
                 input instead"
            );
            run_capture_cpal(stop, route_id, false, on_frame)
        }
        done => done,
    }
}

/// System-audio capture on Linux: the pulse server's monitor of the
/// default sink (PulseAudio natively, PipeWire via pipewire-pulse — i.e.
/// effectively every desktop). A box without a pulse server (bare ALSA)
/// degrades to the default input, loudly.
#[cfg(target_os = "linux")]
fn run_system_capture<F>(stop: &AtomicBool, route_id: &str, on_frame: Arc<F>) -> Result<(), String>
where
    F: Fn(Vec<i16>, u32) + Send + Sync + 'static,
{
    match pulse_monitor::Monitor::open(route_id) {
        Ok(monitor) => {
            tracing::info!(
                "audio capture for {route_id}: system audio (monitor of the default output, \
                 via the pulse server)"
            );
            monitor.pump(stop, &*on_frame)
        }
        Err(e) => {
            tracing::warn!(
                "system-audio capture for {route_id} unavailable ({e}) — capturing the \
                 default input instead"
            );
            run_capture_cpal(stop, route_id, false, on_frame)
        }
    }
}

/// System-audio capture on macOS: there is no OS loopback API — capturing
/// the played mix needs a virtual output device (BlackHole et al.). Until
/// that's wired, the honest degradation is the default input, named in
/// the log.
#[cfg(target_os = "macos")]
fn run_system_capture<F>(stop: &AtomicBool, route_id: &str, on_frame: Arc<F>) -> Result<(), String>
where
    F: Fn(Vec<i16>, u32) + Send + Sync + 'static,
{
    tracing::warn!(
        "system-audio capture isn't available on macOS without a virtual loopback device — \
         capturing the default input for {route_id} instead"
    );
    run_capture_cpal(stop, route_id, false, on_frame)
}

/// Everywhere else with the audio plane built (iOS today; any BSD
/// tomorrow): no loopback API at all, so the same honest degradation as
/// macOS — the default input, named in the log.
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn run_system_capture<F>(stop: &AtomicBool, route_id: &str, on_frame: Arc<F>) -> Result<(), String>
where
    F: Fn(Vec<i16>, u32) + Send + Sync + 'static,
{
    tracing::warn!(
        "system-audio capture isn't available on this platform — capturing the default \
         input for {route_id} instead"
    );
    run_capture_cpal(stop, route_id, false, on_frame)
}

fn run_capture_cpal<F>(
    stop: &AtomicBool,
    route_id: &str,
    loopback: bool,
    on_frame: Arc<F>,
) -> Result<(), String>
where
    F: Fn(Vec<i16>, u32) + Send + Sync + 'static,
{
    let host = cpal::default_host();
    // Loopback (Windows only — see `run_system_capture`) records the
    // default *output* device with its render mix format; everything else
    // records the default input.
    let (device, supported, what) = if loopback {
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default output device to capture".to_string())?;
        let supported = device.default_output_config().map_err(|e| e.to_string())?;
        (device, supported, "system audio (loopback)")
    } else {
        let device = host
            .default_input_device()
            .ok_or_else(|| "no default input device".to_string())?;
        let supported = device.default_input_config().map_err(|e| e.to_string())?;
        (device, supported, "default input")
    };
    let sample_rate = supported.sample_rate().0;
    let channels = supported.channels();
    let config: cpal::StreamConfig = supported.config();
    let fmt = supported.sample_format();
    // Name the device: "no sound" is regularly "the default device isn't
    // the one you think it is".
    tracing::info!(
        "audio capture for {route_id}: {what} — device \"{}\", {sample_rate} Hz, {channels} ch, {fmt:?}",
        device.name().unwrap_or_else(|_| "unknown".into()),
    );
    let err = |e| tracing::warn!("input stream error: {e}");

    // Only one arm runs; each gets its own handle on the shared consumer.
    let stream = match fmt {
        cpal::SampleFormat::F32 => {
            let on_frame = on_frame.clone();
            device.build_input_stream(
                &config,
                move |data: &[f32], _: &_| {
                    let pcm: Vec<i16> = downmix(
                        &data.iter().map(|&f| f32_to_i16(f)).collect::<Vec<_>>(),
                        channels,
                    );
                    on_frame(pcm, sample_rate);
                },
                err,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let on_frame = on_frame.clone();
            device.build_input_stream(
                &config,
                move |data: &[i16], _: &_| on_frame(downmix(data, channels), sample_rate),
                err,
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let on_frame = on_frame.clone();
            device.build_input_stream(
                &config,
                move |data: &[u16], _: &_| {
                    let pcm: Vec<i16> = downmix(
                        &data
                            .iter()
                            .map(|&u| (u as i32 - 32768) as i16)
                            .collect::<Vec<_>>(),
                        channels,
                    );
                    on_frame(pcm, sample_rate);
                },
                err,
                None,
            )
        }
        other => return Err(format!("unsupported input sample format: {other:?}")),
    }
    .map_err(|e| e.to_string())?;

    stream.play().map_err(|e| e.to_string())?;
    park_until_stopped(stop);
    Ok(())
}

/// Recording the pulse server's monitor source — Linux's "what this
/// machine is playing". Uses PulseAudio's **simple** API, dlopen'd at
/// runtime so machines without `libpulse` (a bare-ALSA box, a container)
/// degrade to the caller's fallback instead of failing to load the app.
/// The server side resamples and downmixes to the requested spec, so this
/// asks for the route's wire shape (48 kHz mono S16) directly.
#[cfg(target_os = "linux")]
mod pulse_monitor {
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// `pa_sample_spec` — pulse/sample.h.
    #[repr(C)]
    struct SampleSpec {
        format: c_int,
        rate: u32,
        channels: u8,
    }

    /// `pa_buffer_attr` — pulse/def.h. `u32::MAX` means "server default".
    #[repr(C)]
    struct BufferAttr {
        maxlength: u32,
        tlength: u32,
        prebuf: u32,
        minreq: u32,
        fragsize: u32,
    }

    const PA_SAMPLE_S16LE: c_int = 3;
    const PA_STREAM_RECORD: c_int = 2;
    const RATE: u32 = 48_000;
    /// 20 ms per blocking read — the same order as a cpal capture buffer.
    const SAMPLES_PER_READ: usize = RATE as usize / 50;

    type PaSimpleNew = unsafe extern "C" fn(
        *const c_char, // server (null = default)
        *const c_char, // application name
        c_int,         // direction
        *const c_char, // device
        *const c_char, // stream name
        *const SampleSpec,
        *const c_void, // channel map (null = default)
        *const BufferAttr,
        *mut c_int, // error out
    ) -> *mut c_void;
    type PaSimpleRead = unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_int) -> c_int;
    type PaSimpleFree = unsafe extern "C" fn(*mut c_void);

    struct Api {
        new: PaSimpleNew,
        read: PaSimpleRead,
        free: PaSimpleFree,
    }

    /// dlopen once per process. The library handle is deliberately leaked
    /// so the extracted function pointers stay valid for the app's life.
    fn api() -> Option<&'static Api> {
        static API: std::sync::OnceLock<Option<Api>> = std::sync::OnceLock::new();
        API.get_or_init(|| unsafe {
            let lib = ["libpulse-simple.so.0", "libpulse-simple.so"]
                .iter()
                .find_map(|name| libloading::Library::new(name).ok())?;
            let api = {
                let new: libloading::Symbol<PaSimpleNew> = lib.get(b"pa_simple_new\0").ok()?;
                let read: libloading::Symbol<PaSimpleRead> = lib.get(b"pa_simple_read\0").ok()?;
                let free: libloading::Symbol<PaSimpleFree> = lib.get(b"pa_simple_free\0").ok()?;
                Api {
                    new: *new,
                    read: *read,
                    free: *free,
                }
            };
            std::mem::forget(lib);
            Some(api)
        })
        .as_ref()
    }

    pub(super) struct Monitor {
        api: &'static Api,
        stream: *mut c_void,
    }

    impl Monitor {
        /// Open a recording stream on the monitor of the default sink.
        /// `@DEFAULT_MONITOR@` is resolved server-side, so it lands on
        /// whatever the default output is at open time.
        pub(super) fn open(route_id: &str) -> Result<Self, String> {
            let api = api().ok_or("libpulse-simple isn't available")?;
            let spec = SampleSpec {
                format: PA_SAMPLE_S16LE,
                rate: RATE,
                channels: 1,
            };
            let attr = BufferAttr {
                maxlength: u32::MAX,
                tlength: u32::MAX,
                prebuf: u32::MAX,
                minreq: u32::MAX,
                // Small fragments keep the route's added latency at ~one
                // read instead of the server's roomy record default.
                fragsize: (SAMPLES_PER_READ * std::mem::size_of::<i16>()) as u32,
            };
            let app = CString::new("AllMyStuff").expect("static name");
            let stream_name =
                CString::new(route_id).map_err(|_| "route id contains a NUL".to_string())?;
            let device = CString::new("@DEFAULT_MONITOR@").expect("static name");
            let mut error: c_int = 0;
            let stream = unsafe {
                (api.new)(
                    std::ptr::null(),
                    app.as_ptr(),
                    PA_STREAM_RECORD,
                    device.as_ptr(),
                    stream_name.as_ptr(),
                    &spec,
                    std::ptr::null(),
                    &attr,
                    &mut error,
                )
            };
            if stream.is_null() {
                return Err(format!("pa_simple_new failed (error {error})"));
            }
            Ok(Monitor { api, stream })
        }

        /// Blocking read loop: hand each ~20 ms of mono PCM to `on_frame`
        /// until `stop` flips. A monitor source streams continuously
        /// (zeros while the desktop is silent), so the stop flag is seen
        /// within one read.
        pub(super) fn pump(
            &self,
            stop: &AtomicBool,
            on_frame: &(impl Fn(Vec<i16>, u32) + Send + Sync),
        ) -> Result<(), String> {
            let mut buf = vec![0i16; SAMPLES_PER_READ];
            while !stop.load(Ordering::SeqCst) {
                let mut error: c_int = 0;
                let rc = unsafe {
                    (self.api.read)(
                        self.stream,
                        buf.as_mut_ptr().cast(),
                        buf.len() * std::mem::size_of::<i16>(),
                        &mut error,
                    )
                };
                if rc < 0 {
                    return Err(format!("pa_simple_read failed (error {error})"));
                }
                on_frame(buf.clone(), RATE);
            }
            Ok(())
        }
    }

    impl Drop for Monitor {
        fn drop(&mut self) {
            unsafe { (self.api.free)(self.stream) };
        }
    }
}

// ---- playback ---------------------------------------------------------

fn run_playback(
    stop: &AtomicBool,
    route_id: &str,
    ring: Arc<Mutex<VecDeque<i16>>>,
    out_rate: Arc<AtomicU32>,
    fed: Arc<AtomicU64>,
) -> Result<(), String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "no default output device".to_string())?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    out_rate.store(supported.sample_rate().0, Ordering::Relaxed);
    let channels = supported.channels() as usize;
    let config: cpal::StreamConfig = supported.config();
    let fmt = supported.sample_format();
    tracing::info!(
        "audio playback for {route_id}: device \"{}\", {} Hz, {channels} ch, {fmt:?}",
        device.name().unwrap_or_else(|_| "unknown".into()),
        supported.sample_rate().0,
    );
    let err = |e| tracing::warn!("output stream error: {e}");

    // Each output frame is `channels` interleaved samples; we hold mono in
    // the ring and write the same sample to every channel.
    macro_rules! fill {
        ($data:expr, $conv:expr) => {{
            let mut guard = ring.lock();
            for frame in $data.chunks_mut(channels) {
                let s = guard.pop_front().unwrap_or(0);
                for slot in frame.iter_mut() {
                    *slot = $conv(s);
                }
            }
        }};
    }

    let stream = match fmt {
        cpal::SampleFormat::F32 => device.build_output_stream(
            &config,
            move |data: &mut [f32], _: &_| fill!(data, i16_to_f32),
            err,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_output_stream(
            &config,
            move |data: &mut [i16], _: &_| fill!(data, |s| s),
            err,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_output_stream(
            &config,
            move |data: &mut [u16], _: &_| fill!(data, |s: i16| (s as i32 + 32768) as u16),
            err,
            None,
        ),
        other => return Err(format!("unsupported output sample format: {other:?}")),
    }
    .map_err(|e| e.to_string())?;

    stream.play().map_err(|e| e.to_string())?;
    // The route is live and the speaker is waiting: if nothing has been
    // fed after a few seconds the problem is upstream (peer capturing
    // silence, frames not arriving) — name it once, so "no sound" is
    // attributable from this side's log alone.
    let mut starve_check = Some(Instant::now());
    while !stop.load(Ordering::SeqCst) {
        if let Some(started) = starve_check {
            if started.elapsed() >= Duration::from_secs(5) {
                starve_check = None;
                if fed.load(Ordering::Relaxed) == 0 {
                    tracing::warn!(
                        "audio playback for {route_id} has received no frames after 5s — \
                         the sending side is likely capturing silence or not capturing at all"
                    );
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn park_until_stopped(stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---- sample helpers (pure) -------------------------------------------

fn f32_to_i16(f: f32) -> i16 {
    (f.clamp(-1.0, 1.0) * 32767.0) as i16
}

fn i16_to_f32(s: i16) -> f32 {
    s as f32 / 32768.0
}

/// Average `channels` interleaved samples down to one mono sample each.
fn downmix(samples: &[i16], channels: u16) -> Vec<i16> {
    let ch = channels.max(1) as usize;
    if ch == 1 {
        return samples.to_vec();
    }
    samples
        .chunks(ch)
        .map(|c| (c.iter().map(|&s| s as i32).sum::<i32>() / c.len() as i32) as i16)
        .collect()
}

/// Linear-interpolating resampler from `from_rate` to `to_rate` (mono).
pub(crate) fn resample_linear(samples: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    if samples.is_empty() || from_rate == 0 || to_rate == 0 || from_rate == to_rate {
        return samples.to_vec();
    }
    let ratio = to_rate as f64 / from_rate as f64;
    let out_len = ((samples.len() as f64) * ratio) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 / ratio;
        let i0 = src.floor() as usize;
        let frac = src - i0 as f64;
        let a = samples.get(i0).copied().unwrap_or(0) as f64;
        let b = samples.get(i0 + 1).copied().unwrap_or(a as i16) as f64;
        out.push((a + (b - a) * frac) as i16);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_stereo_to_mono_averages() {
        assert_eq!(downmix(&[10, 20, 30, 40], 2), vec![15, 35]);
        assert_eq!(downmix(&[1, 2, 3], 1), vec![1, 2, 3]);
    }

    #[test]
    fn resample_upsamples_length() {
        let up = resample_linear(&[0, 100, 200, 300], 24_000, 48_000);
        assert_eq!(up.len(), 8);
        assert_eq!(up[0], 0);
        // Same rate is a passthrough.
        assert_eq!(resample_linear(&[1, 2, 3], 48_000, 48_000), vec![1, 2, 3]);
    }

    #[test]
    fn sample_conversions_round_trip_near_unity() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert!(f32_to_i16(1.0) >= 32760);
        assert!((i16_to_f32(32767) - 1.0).abs() < 0.01);
    }

    #[test]
    fn level_stats_flag_a_pure_silence_window() {
        let mut stats = LevelStats::new();
        assert!(!stats.note(&[0, 0, 0], |_| {}), "window not elapsed yet");
        stats.since = Instant::now() - Duration::from_secs(6);
        assert!(stats.note(&[0, 0, 0], |_| {}), "all-zeros window is silent");

        let mut loud = LevelStats::new();
        loud.since = Instant::now() - Duration::from_secs(6);
        assert!(!loud.note(&[0, 900, 0], |_| {}), "signal present");
    }

    #[test]
    fn opus_stream_frames_and_survives_a_roundtrip() {
        // Arbitrary capture cadence in (here: 30 ms buffers at 24 kHz),
        // canonical 20 ms Opus packets out — and what was encoded
        // decodes again, which is the whole contract with the far side.
        let mut stream = OpusStream::new().expect("encoder");
        let mut packets = Vec::new();
        // 3 × 30 ms at 24 kHz = 90 ms → resampled to 48 kHz → 4 full
        // frames (80 ms) plus a 10 ms remainder held in the buffer.
        let buf: Vec<i16> = (0..720).map(|i| ((i * 37) % 1000) as i16).collect();
        for _ in 0..3 {
            stream.push(&buf, 24_000, |pkt| packets.push(pkt));
        }
        assert_eq!(packets.len(), 4, "90 ms yields 4 complete frames");
        assert!(
            stream.buf.len() < OPUS_FRAME_SAMPLES,
            "remainder stays buffered"
        );

        let mut dec = opus::Decoder::new(OPUS_RATE, opus::Channels::Mono).expect("decoder");
        let mut out = vec![0i16; OPUS_FRAME_SAMPLES * 6];
        let n = dec.decode(&packets[0], &mut out, false).expect("decode");
        assert_eq!(n, OPUS_FRAME_SAMPLES, "one packet = one 20 ms frame");
    }

    #[test]
    fn playback_ring_trims_back_to_the_target_depth() {
        // The ring is a jitter buffer: anything beyond MAX_DEPTH_MS is
        // cut back to TARGET_DEPTH_MS (oldest samples dropped) so audio
        // can't fall permanently behind the video stream. Exercise the
        // buffer operation directly: opening a native output device would
        // test CPAL/WASAPI instead and make this depend on CI hardware.
        let mut ring = VecDeque::new();
        // Default device rate 48 kHz → target 3840 samples, max 9600.
        for _ in 0..5 {
            buffer_playback_samples(&mut ring, vec![1i16; 4800], 48_000); // 100 ms
        }
        // 500 ms went in; the trims must have pulled it back to target.
        assert_eq!(ring.len(), (48_000 / 1000) * TARGET_DEPTH_MS);
    }

    #[test]
    fn capture_and_playback_for_one_route_coexist() {
        // A loopback route starts both; stopping must kill both. (The
        // threads themselves bail instantly in CI — no audio devices —
        // which is fine: this exercises the bookkeeping, not cpal.)
        let bridge = AudioBridge::new();
        bridge.start_capture("r".into(), CaptureSource::Mic, |_, _| {});
        bridge.start_playback("r".into());
        assert!(bridge.captures.lock().contains_key("r"));
        assert!(bridge.playbacks.lock().contains_key("r"));
        bridge.stop("r");
        assert!(!bridge.is_running("r"));
    }
}
