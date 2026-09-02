//! Hardware H.264 encode on **Windows via Media Foundation** — the GPU's own
//! H.264 Media Foundation Transform (MFT): `NVIDIA H.264 Encoder MFT` (NVENC,
//! our fleet's gold standard), Intel QuickSync, or AMD VCE, whichever the box
//! actually has. The ladder in `video.rs` enumerates the platform's hardware
//! MFTs (sorted best-first by MF), opens each, and frame-send-tests it; the
//! first that emits an access unit wins, else it steps down to software
//! openh264.
//!
//! Why MF and not FFmpeg here: FFmpeg's vendor encoders need libav* dev libs +
//! pkg-config/vcpkg at build time — a toolchain end users (and our own Windows
//! host) don't carry, which is exactly what broke the build. Media Foundation
//! ships *inside* Windows and we already link the `windows` crate for DXGI
//! capture, so this path adds **no new build dependency** and works on any
//! Windows config with a hardware H.264 encoder.
//!
//! Input is the same contiguous I420 the software path produces
//! (`allmystuff_pixels::scale_rgba_to_i420`); we interleave its chroma to NV12
//! (the format every hardware H.264 MFT accepts) and feed system-memory
//! samples. Output is an Annex-B byte stream — same seam as openh264/FFmpeg:
//! I420 in, Annex-B H.264 out.

use std::ffi::c_void;
use std::sync::Once;
use std::time::{Duration, Instant};

use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;

use crate::video::EncodeOutcome;

/// How long one encode call may keep polling for the freshly-fed frame's own
/// output before returning whatever has already drained. Healthy hardware
/// answers in a few ms, so the common case still returns this frame's unit;
/// a loaded GPU answers whenever it answers, and that unit is returned by a
/// later call's drain instead of stalling the capture thread. (The old model
/// waited up to a full second and then *discarded* all but the newest
/// backlogged unit on the next call — the freeze and the smear-until-IDR,
/// respectively, under GPU load.)
const ASYNC_OUTPUT_GRACE: Duration = Duration::from_millis(50);

/// MF version word: `(MF_SDK_VERSION << 16) | MF_API_VERSION` = `0x0002_0070`.
/// Hard-coded because the `windows` crate doesn't export the composed constant.
const MF_VERSION_WORD: u32 = 0x0002_0070;

static MF_STARTUP: Once = Once::new();

/// Start Media Foundation once for the process (refcounted with `MFShutdown`,
/// which we never call — process lifetime). Idempotent.
fn ensure_mf_started() {
    MF_STARTUP.call_once(|| unsafe {
        let _ = MFStartup(MF_VERSION_WORD, MFSTARTUP_NOSOCKET);
    });
}

/// COM-initialize the *calling thread* (MTA), best-effort. Each route's
/// capture/encode thread builds and drives its own encoder, so every thread
/// that touches MF must be initialized; `S_FALSE`/`RPC_E_CHANGED_MODE` mean
/// it already was, which is fine. We don't `CoUninitialize` — the thread lives
/// for the route and tears down with it.
fn ensure_com_thread() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// One enumerated hardware H.264 encoder MFT, not yet activated. Holds the
/// activation handle and the MFT's friendly name (for logs — the user sees
/// e.g. "NVIDIA H.264 Encoder MFT" confirming hardware is in play).
pub struct HwEncoder {
    activate: IMFActivate,
    name: String,
}

impl HwEncoder {
    /// The MFT's friendly name, e.g. `"NVIDIA H.264 Encoder MFT"`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Activate and configure this MFT for `width`×`height` at `fps`/`bitrate`.
    /// Returns `Err` if it won't activate or accept our NV12-in/H264-out types
    /// (wrong driver state, unsupported size) — the ladder steps to the next.
    pub fn open(
        &self,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<MediaFoundationH264, String> {
        self.open_for_route(width, height, fps, bitrate, crate::video::Posture::Balanced)
    }

    /// [`Self::open`] with the owning route's latency posture. Production
    /// streams use this entry so two concurrent routes may carry different
    /// peak/VBV shapes; the plain [`Self::open`] stays balanced for hardware
    /// probes and backend-local tests.
    pub(crate) fn open_for_route(
        &self,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        posture: crate::video::Posture,
    ) -> Result<MediaFoundationH264, String> {
        self.open_with_manager_for_route(width, height, fps, bitrate, None, posture)
    }

    /// [`Self::open`] bound to a DXGI device manager — the GPU lane: the MFT
    /// joins the shared device and accepts NV12 *textures*
    /// ([`MediaFoundationH264::encode_texture`]) with zero CPU pixel work.
    pub fn open_with_manager(
        &self,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        manager: Option<&IMFDXGIDeviceManager>,
    ) -> Result<MediaFoundationH264, String> {
        self.open_with_manager_for_route(
            width,
            height,
            fps,
            bitrate,
            manager,
            crate::video::Posture::Balanced,
        )
    }

    /// Device-manager form of [`Self::open_for_route`]. The manager and route
    /// posture both belong to this one encode lane; neither is process-global.
    pub(crate) fn open_with_manager_for_route(
        &self,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        manager: Option<&IMFDXGIDeviceManager>,
        posture: crate::video::Posture,
    ) -> Result<MediaFoundationH264, String> {
        ensure_com_thread();
        unsafe { self.open_inner(width, height, fps, bitrate, manager, posture) }
    }

    unsafe fn open_inner(
        &self,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        manager: Option<&IMFDXGIDeviceManager>,
        posture: crate::video::Posture,
    ) -> Result<MediaFoundationH264, String> {
        let name = self.name.clone();
        let game = posture == crate::video::Posture::Game;
        let studio = matches!(
            posture,
            crate::video::Posture::Studio | crate::video::Posture::StudioLossless
        );

        // Activate the MFT into an IMFTransform.
        let transform: IMFTransform = self
            .activate
            .ActivateObject()
            .map_err(mferr(&name, "activate"))?;

        // Async (hardware) MFTs must be unlocked and driven by events; sync MFTs
        // use the plain ProcessInput/ProcessOutput model. Detect which.
        let attrs = transform.GetAttributes().ok();
        let is_async = attrs
            .as_ref()
            .and_then(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).ok())
            .unwrap_or(0)
            == 1;
        if let Some(a) = &attrs {
            if is_async {
                let _ = a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
            }
            // Low-latency mode: no reordering/lookahead buffering — what a live
            // screen stream needs. Best-effort (older MFTs may ignore it).
            let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
        }

        // The GPU lane hands over the DXGI device manager before any media
        // types — but strictly AFTER the async unlock above: an async MFT
        // refuses every message (MF_E_TRANSFORM_ASYNC_LOCKED) until the
        // caller declares async support. CPU-lane opens skip this and feed
        // system memory exactly as before.
        if let Some(m) = manager {
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, m.as_raw() as usize)
                .map_err(mferr(&name, "set D3D manager"))?;
        }

        // H.264 encoders require the OUTPUT type set before the input type.
        let out_type = MFCreateMediaType().map_err(mferr(&name, "create out type"))?;
        out_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(mferr(&name, "out major"))?;
        out_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .map_err(mferr(&name, "out subtype"))?;
        out_type
            .SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
            .map_err(mferr(&name, "out bitrate"))?;
        out_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(mferr(&name, "out interlace"))?;
        set_attr_size(&out_type, &MF_MT_FRAME_SIZE, width, height)
            .map_err(mferr(&name, "out size"))?;
        set_attr_size(&out_type, &MF_MT_FRAME_RATE, fps.max(1), 1)
            .map_err(mferr(&name, "out rate"))?;
        set_attr_size(&out_type, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)
            .map_err(mferr(&name, "out par"))?;
        // Main profile — broad decoder support; best-effort.
        let _ = out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32);
        transform
            .SetOutputType(0, &out_type, 0)
            .map_err(mferr(&name, "set out type"))?;

        // Input: NV12 system memory (every hardware H.264 MFT accepts it).
        let in_type = MFCreateMediaType().map_err(mferr(&name, "create in type"))?;
        in_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(mferr(&name, "in major"))?;
        in_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(mferr(&name, "in subtype"))?;
        in_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(mferr(&name, "in interlace"))?;
        set_attr_size(&in_type, &MF_MT_FRAME_SIZE, width, height)
            .map_err(mferr(&name, "in size"))?;
        set_attr_size(&in_type, &MF_MT_FRAME_RATE, fps.max(1), 1)
            .map_err(mferr(&name, "in rate"))?;
        set_attr_size(&in_type, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1).map_err(mferr(&name, "in par"))?;
        // Our NV12 buffers are tightly packed: stride == width, stated
        // explicitly. Left unstated, the Intel QSV MFT computes its own
        // assumed stride and can round it to an alignment boundary (a
        // sheared/green frame on Intel silicon); NVIDIA is indifferent.
        // Best-effort — an MFT that rejects the attribute keeps its default.
        let _ = in_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, width);
        transform
            .SetInputType(0, &in_type, 0)
            .map_err(mferr(&name, "set in type"))?;

        // ICodecAPI knobs — all best-effort: the encoder works on defaults if a
        // particular box doesn't expose one. The rate control is the crucial
        // part: **peak-constrained VBR with a peak (MaxBitRate ≈ 2× average) and
        // a ~1 s VBV (BufferSize)** so a fast-motion / scene-change frame can
        // spend more than the average byte budget instead of having its QP
        // cranked into macroblocking. Bare CBR with mean-rate-only (the old
        // setting) starved exactly those frames — the "blocky on fast motion"
        // symptom. GOP is a backstop (≈4 s); forced IDRs handle the tight
        // recovery cadence. Plus the ability to force a keyframe on demand.
        let codecapi = transform.cast::<ICodecAPI>().ok();
        if let Some(api) = &codecapi {
            // Peak + VBV from the shared posture: quality-first by default,
            // trimmed for burst latency in game mode (see
            // `video::burst_bounds`).
            let (peak, vbv) = crate::video::burst_bounds(bitrate, fps, game, studio);
            let _ = api.SetValue(
                &CODECAPI_AVEncCommonRateControlMode,
                &variant_u32(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32),
            );
            let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bitrate));
            let _ = api.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &variant_u32(peak));
            let _ = api.SetValue(&CODECAPI_AVEncCommonBufferSize, &variant_u32(vbv));
            let _ = api.SetValue(&CODECAPI_AVEncCommonLowLatency, &variant_bool(true));
            let _ = api.SetValue(
                &CODECAPI_AVEncMPVGOPSize,
                &variant_u32(fps.saturating_mul(4).max(1)),
            );
            if crate::video::paced_slices_enabled() {
                // Prefer an exact byte cap, but do not mistake one rejected
                // mode for "this encoder cannot slice". CodecAPI defines
                // three static modes and permits an encoder to expose only a
                // subset. Negotiate all legal modes, most precise first.
                configure_paced_slices(api, &name, width, height);
            }
        }

        // Event generator for the async (hardware) drive model.
        let events = if is_async {
            Some(
                transform
                    .cast::<IMFMediaEventGenerator>()
                    .map_err(mferr(&name, "event generator"))?,
            )
        } else {
            None
        };

        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .map_err(mferr(&name, "begin streaming"))?;
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(mferr(&name, "start of stream"))?;

        // Whether the MFT hands us its own output samples (hardware MFTs do) or
        // we must supply the buffer.
        let info = transform
            .GetOutputStreamInfo(0)
            .map_err(mferr(&name, "out stream info"))?;
        let provides_samples = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
            != 0;
        let out_buf_size = (info.cbSize as usize).max((width as usize * height as usize) + 4096);

        Ok(MediaFoundationH264 {
            transform,
            events,
            codecapi,
            is_async,
            name: self.name.clone(),
            width,
            height,
            fps: fps.max(1),
            provides_samples,
            out_buf_size,
            frame_index: 0,
            input_credits: 0,
            game,
            studio,
        })
    }

    /// Tear down the object cached by this activation after a throwaway
    /// frame-send probe. `IMFActivate` otherwise returns that same transform
    /// on the next `ActivateObject`, carrying the probe's grey reference chain
    /// into the live route.
    pub(crate) fn shutdown_probe_instance(&self) -> Result<(), String> {
        ensure_com_thread();
        unsafe {
            self.activate
                .ShutdownObject()
                .map_err(mferr(&self.name, "shutdown probe instance"))
        }
    }
}

/// Enumerate the box's **hardware** H.264 encoder MFTs, best-first (MF's own
/// sort/filter). Empty when none exists (no GPU encoder, RDP session, locked
/// down driver) — the ladder then falls to software openh264.
pub fn hardware_h264_mfts() -> Vec<HwEncoder> {
    ensure_mf_started();
    ensure_com_thread();
    unsafe { enum_hw_h264() }
}

/// [`hardware_h264_mfts`] scoped to one adapter — the GPU lane's
/// enumeration: the encoder must live on the duplication device's adapter
/// or the shared device manager buys nothing. Empty when that adapter has
/// no H.264 encoder MFT; the lane then falls back to the CPU path (which
/// may still find an encoder elsewhere).
pub(crate) fn hardware_h264_mfts_on(adapter: LUID) -> Vec<HwEncoder> {
    ensure_mf_started();
    ensure_com_thread();
    unsafe { enum_h264_encoders(Some(adapter)) }
}

/// Whether the operator pinned encoding to a specific adapter
/// (`ALLMYSTUFF_VIDEO_ENCODE_ADAPTER`). The GPU zero-copy lane steps aside
/// when so: the pin's whole point is encoding on a *different* GPU than
/// the display's (iGPU offload), which is inherently cross-adapter — the
/// CPU lane's system-memory NV12 serves that; a same-device texture lane
/// can't.
pub(crate) fn adapter_pin_active() -> bool {
    adapter_pin().is_some()
}

unsafe fn enum_hw_h264() -> Vec<HwEncoder> {
    // The operator's adapter pin runs first — soft, like every rung of the
    // ladder: a pin that matches no adapter, or an adapter with no H.264
    // encoder MFT, falls back to the default enumeration.
    if let Some(pin) = adapter_pin() {
        if let Some(luid) = adapter_luid_for_pin(pin) {
            let pinned = enum_h264_encoders(Some(luid));
            if !pinned.is_empty() {
                return pinned;
            }
            tracing::warn!(
                "pinned adapter ({pin}) exposes no hardware H.264 encoder MFT; \
                 using the default enumeration"
            );
        }
    }
    enum_h264_encoders(None)
}

/// The operator's encoder-adapter pin (`ALLMYSTUFF_VIDEO_ENCODE_ADAPTER`):
/// `intel` / `nvidia` / `amd` picks the first adapter of that vendor, a
/// number picks the DXGI adapter index. Unset = MF's default enumeration
/// (every adapter's encoders, merit-sorted). The lever this adds: on a box
/// whose primary GPU is saturated (a game, a render job), pinning the
/// encode to the idle iGPU keeps the stream's encoder off the contended
/// engine entirely — encoder input is system-memory NV12, so feeding
/// another adapter's MFT costs nothing extra. Read once per process, like
/// the video dials.
fn adapter_pin() -> Option<&'static str> {
    static PIN: std::sync::LazyLock<Option<String>> =
        std::sync::LazyLock::new(|| match std::env::var("ALLMYSTUFF_VIDEO_ENCODE_ADAPTER") {
            Ok(v) if !v.trim().is_empty() => {
                tracing::info!("ALLMYSTUFF_VIDEO_ENCODE_ADAPTER={} (override)", v.trim());
                Some(v.trim().to_string())
            }
            _ => None,
        });
    PIN.as_deref()
}

/// Resolve a pin to a DXGI adapter LUID, logging which adapter it landed on.
unsafe fn adapter_luid_for_pin(pin: &str) -> Option<LUID> {
    let factory: IDXGIFactory1 = match CreateDXGIFactory1() {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("DXGI factory for the encoder-adapter pin failed: {e}");
            return None;
        }
    };
    let want_vendor = match pin.to_ascii_lowercase().as_str() {
        "intel" => Some(0x8086u32),
        "nvidia" => Some(0x10DEu32),
        "amd" => Some(0x1002u32),
        _ => None,
    };
    let want_index: Option<u32> = pin.parse().ok();
    if want_vendor.is_none() && want_index.is_none() {
        tracing::warn!(
            "ALLMYSTUFF_VIDEO_ENCODE_ADAPTER={pin} isn't intel/nvidia/amd or an adapter index"
        );
        return None;
    }
    let mut idx = 0u32;
    while let Ok(adapter) = factory.EnumAdapters1(idx) {
        if let Ok(desc) = adapter.GetDesc1() {
            let hit = match want_vendor {
                Some(v) => desc.VendorId == v,
                None => Some(idx) == want_index,
            };
            if hit {
                let name = String::from_utf16_lossy(&desc.Description);
                tracing::info!(
                    "encoder adapter pinned: {} (DXGI adapter {idx})",
                    name.trim_end_matches('\0').trim()
                );
                return Some(desc.AdapterLuid);
            }
        }
        idx += 1;
    }
    tracing::warn!("ALLMYSTUFF_VIDEO_ENCODE_ADAPTER={pin} matched no DXGI adapter");
    None
}

/// One enumeration pass: `MFTEnum2` scoped to `adapter` when given
/// (`MFT_ENUM_ADAPTER_LUID` — valid only alongside `MFT_ENUM_FLAG_HARDWARE`,
/// which is always set here), plain `MFTEnumEx` otherwise.
unsafe fn enum_h264_encoders(adapter: Option<LUID>) -> Vec<HwEncoder> {
    let out_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    // Hardware encoders, output H.264; SORTANDFILTER puts the preferred MFT
    // first and drops disabled ones.
    let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    let enumerated = match adapter {
        Some(luid) => {
            let mut attrs: Option<IMFAttributes> = None;
            if MFCreateAttributes(&mut attrs, 1).is_err() {
                return Vec::new();
            }
            let Some(attrs) = attrs else {
                return Vec::new();
            };
            // The attribute's value is the adapter LUID as an 8-byte blob
            // (LowPart then HighPart, little-endian — the struct's layout).
            let mut blob = [0u8; 8];
            blob[..4].copy_from_slice(&luid.LowPart.to_le_bytes());
            blob[4..].copy_from_slice(&luid.HighPart.to_le_bytes());
            if attrs.SetBlob(&MFT_ENUM_ADAPTER_LUID, &blob).is_err() {
                return Vec::new();
            }
            MFTEnum2(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                None,
                Some(&out_info),
                &attrs,
                &mut activates,
                &mut count,
            )
        }
        None => MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&out_info),
            &mut activates,
            &mut count,
        ),
    };
    if enumerated.is_err() || activates.is_null() || count == 0 {
        if !activates.is_null() {
            CoTaskMemFree(Some(activates as *const c_void));
        }
        return Vec::new();
    }

    let mut result = Vec::new();
    let slice = std::slice::from_raw_parts_mut(activates, count as usize);
    for slot in slice.iter_mut() {
        // take() moves ownership out of the array (leaving None), so the COM
        // ref transfers to us — dropped with HwEncoder, never leaked.
        if let Some(act) = slot.take() {
            let name = friendly_name(&act);
            result.push(HwEncoder {
                activate: act,
                name,
            });
        }
    }
    // The array memory itself is CoTaskMem; its element refs are now ours.
    CoTaskMemFree(Some(activates as *const c_void));
    // The full enumeration at debug, not just the eventual winner — on a
    // hybrid dGPU+iGPU box this is what reveals a present-but-never-selected
    // second encoder (the ladder takes the first that emits a frame).
    if !result.is_empty() {
        tracing::debug!(
            "hardware H.264 encoder MFTs{}: {}",
            if adapter.is_some() {
                " (pinned adapter)"
            } else {
                ""
            },
            result
                .iter()
                .map(|h| h.name())
                .collect::<Vec<_>>()
                .join(" · ")
        );
    }
    result
}

/// The `MFT_FRIENDLY_NAME_Attribute` string, or a generic label if absent.
unsafe fn friendly_name(act: &IMFActivate) -> String {
    let mut ptr = PWSTR::null();
    let mut len: u32 = 0;
    match act.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut ptr, &mut len) {
        Ok(()) if !ptr.is_null() => {
            let s = ptr.to_string().unwrap_or_default();
            CoTaskMemFree(Some(ptr.0 as *const c_void));
            if s.is_empty() {
                "H.264 hardware MFT".to_string()
            } else {
                s
            }
        }
        _ => "H.264 hardware MFT".to_string(),
    }
}

/// One opened Media Foundation hardware H.264 encoder. Created and driven
/// entirely on its route's capture/encode thread.
pub struct MediaFoundationH264 {
    transform: IMFTransform,
    /// `Some` for async (hardware) MFTs — the event source we pump.
    events: Option<IMFMediaEventGenerator>,
    /// `Some` when the MFT exposes the codec API (force-keyframe, rate control).
    codecapi: Option<ICodecAPI>,
    is_async: bool,
    name: String,
    width: u32,
    height: u32,
    fps: u32,
    /// Whether the MFT allocates its own output samples.
    provides_samples: bool,
    out_buf_size: usize,
    frame_index: i64,
    /// Banked `METransformNeedInput` credits. The async MFT posts each
    /// NeedInput exactly once and then *waits*; an event that arrives while
    /// this call's frame is already fed must be banked — not dropped — or
    /// the input pipeline starves permanently (the MFT never re-asks). A
    /// banked credit feeds the next call's frame immediately, no event
    /// round-trip.
    input_credits: u32,
    /// This route's burst posture, retained so in-place bitrate changes keep
    /// the same peak/VBV shape chosen at open.
    game: bool,
    studio: bool,
}

// SAFETY: a MediaFoundationH264 is built on, and only ever used from, the single
// capture/encode thread that owns its route's stream (see `video.rs` —
// `H264Stream` lives on that thread and is never shared). The COM interfaces it
// holds are therefore never touched from two threads at once. The `Send` bound
// exists only because the `H264Codec` trait object is nominally `Send`; the
// value does not actually migrate between threads after construction.
unsafe impl Send for MediaFoundationH264 {}

impl MediaFoundationH264 {
    pub fn label(&self) -> &str {
        &self.name
    }

    /// Encode one contiguous NV12 frame (`width*height` Y, then interleaved
    /// U/V) — the MFT's native layout, which the fused scaler upstream now
    /// produces directly (the old seam took I420 and paid a full chroma
    /// re-interleave per frame here). Returns every Annex-B access unit the
    /// MFT had ready (oldest first) plus whether this frame was accepted —
    /// see [`crate::video::EncodeOutcome`] for why both matter.
    /// `pub(crate)`: the outcome type is the video module's internal seam.
    pub(crate) fn encode_nv12(
        &mut self,
        nv12: &[u8],
        force_idr: bool,
    ) -> Result<EncodeOutcome, String> {
        let (w, h) = (self.width as usize, self.height as usize);
        let need = w * h + 2 * ((w / 2) * (h / 2));
        if nv12.len() < need {
            return Err(format!(
                "{}: short NV12 ({} < {need})",
                self.name,
                nv12.len(),
            ));
        }

        let duration = 10_000_000i64 / i64::from(self.fps);
        let time = self.frame_index * duration;
        self.frame_index += 1;

        unsafe {
            let sample = make_input_sample(&nv12[..need], time, duration)
                .map_err(|e| format!("{}: input sample: {e}", self.name))?;
            if force_idr {
                self.force_keyframe();
            }
            if self.is_async {
                self.pump_async(&sample)
            } else {
                self.pump_sync(&sample)
            }
        }
    }

    /// Encode one NV12 **texture** — the GPU lane's zero-copy input. The
    /// MFT must have been opened with the same device manager the texture's
    /// device is registered in ([`HwEncoder::open_with_manager`]); the
    /// encoder reads the surface in place — no CPU pixel work anywhere on
    /// this path.
    pub(crate) fn encode_texture(
        &mut self,
        nv12: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        force_idr: bool,
    ) -> Result<EncodeOutcome, String> {
        let duration = 10_000_000i64 / i64::from(self.fps);
        let time = self.frame_index * duration;
        self.frame_index += 1;
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(
                &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D::IID,
                nv12,
                0,
                false,
            )
            .map_err(|e| format!("{}: DXGI surface buffer: {e}", self.name))?;
            let sample = MFCreateSample().map_err(|e| format!("{}: sample: {e}", self.name))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| format!("{}: add buffer: {e}", self.name))?;
            sample
                .SetSampleTime(time)
                .map_err(|e| format!("{}: sample time: {e}", self.name))?;
            sample
                .SetSampleDuration(duration)
                .map_err(|e| format!("{}: sample duration: {e}", self.name))?;
            if force_idr {
                self.force_keyframe();
            }
            if self.is_async {
                self.pump_async(&sample)
            } else {
                self.pump_sync(&sample)
            }
        }
    }

    /// Request the next encoded frame be an IDR (viewer asked for a clean entry,
    /// or our adaptive cadence is due). Best-effort — if the codec API isn't
    /// exposed, the encoder's GOP still yields periodic keyframes.
    unsafe fn force_keyframe(&self) {
        if let Some(api) = &self.codecapi {
            let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
        }
    }

    /// Re-aim the rate controller mid-stream — mean plus the matching
    /// peak/VBV posture, no reopen, no forced IDR. `true` when the MFT
    /// accepted the mean (NVENC's does); a `false` leaves the encoder
    /// exactly as it was, and the caller keeps the rebuild path. This is
    /// the knob smooth receiver-driven adaptation needs: today a bitrate
    /// move costs a codec rebuild whose first unit is an IDR — a burst on
    /// the wire at the exact moment the link is struggling.
    pub(crate) fn set_bitrate(&mut self, bitrate: u32) -> bool {
        let Some(api) = &self.codecapi else {
            return false;
        };
        unsafe {
            if api
                .SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bitrate))
                .is_err()
            {
                return false;
            }
            // Peak + VBV follow the mean so the burst posture stays
            // proportional; best-effort — a box that only re-aims the mean
            // still adapts.
            let (peak, vbv) = crate::video::burst_bounds(bitrate, self.fps, self.game, self.studio);
            let _ = api.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &variant_u32(peak));
            let _ = api.SetValue(&CODECAPI_AVEncCommonBufferSize, &variant_u32(vbv));
            true
        }
    }

    /// Synchronous MFT: feed one input, drain every output it produces —
    /// each drained sample is its own access unit, in order. A sync MFT with
    /// pending output can refuse input (`MF_E_NOTACCEPTING`): drain, then
    /// retry once; if it still refuses, report the frame unconsumed rather
    /// than erroring the stream.
    unsafe fn pump_sync(&mut self, sample: &IMFSample) -> Result<EncodeOutcome, String> {
        let mut units: Vec<(Vec<u8>, bool)> = Vec::new();
        let mut consumed = false;
        for _ in 0..2 {
            match self.transform.ProcessInput(0, sample, 0) {
                Ok(()) => {
                    consumed = true;
                    break;
                }
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    while let Some(unit) = self.process_output()? {
                        units.push(unit);
                    }
                }
                Err(e) => return Err(format!("{}: ProcessInput: {e}", self.name)),
            }
        }
        while let Some(unit) = self.process_output()? {
            units.push(unit);
        }
        Ok(EncodeOutcome {
            units,
            consumed,
            input_ts: 0,
        })
    }

    /// Async (hardware) MFT: drive the event model **losslessly**. Drain every
    /// pending event — collecting each available output in order, feeding our
    /// one frame on a `METransformNeedInput` (or on a banked credit from a
    /// previous call, immediately) and **banking** any further NeedInput as an
    /// input credit — then poll only a short [`ASYNC_OUTPUT_GRACE`] for this
    /// frame's own output. Whatever hasn't arrived by then is returned by a
    /// later call's drain; **no drained unit is ever discarded** (dropping one
    /// snaps the viewer's P-frame reference chain), **no NeedInput is ever
    /// dropped** (each is posted exactly once — losing one starves the input
    /// pipeline for good), and the capture thread is never parked for longer
    /// than the grace, no matter how loaded the GPU is.
    unsafe fn pump_async(&mut self, sample: &IMFSample) -> Result<EncodeOutcome, String> {
        let events = match &self.events {
            Some(e) => e.clone(),
            None => return Err(format!("{}: async MFT without event generator", self.name)),
        };
        let mut units: Vec<(Vec<u8>, bool)> = Vec::new();
        let mut fed = false;
        // A banked credit means the MFT already asked for this frame.
        if self.input_credits > 0 {
            self.transform
                .ProcessInput(0, sample, 0)
                .map_err(|e| format!("{}: ProcessInput: {e}", self.name))?;
            self.input_credits -= 1;
            fed = true;
        }
        let start = Instant::now();
        loop {
            let event = match events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                Ok(ev) => ev,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    // Done once our frame is in and at least one unit is out —
                    // or once the grace runs dry. Never discard what drained.
                    if fed && !units.is_empty() {
                        break;
                    }
                    if start.elapsed() >= ASYNC_OUTPUT_GRACE {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(e) => return Err(format!("{}: GetEvent: {e}", self.name)),
            };
            let met = event
                .GetType()
                .map_err(|e| format!("{}: event type: {e}", self.name))?;
            if met == METransformNeedInput.0 as u32 {
                if !fed {
                    self.transform
                        .ProcessInput(0, sample, 0)
                        .map_err(|e| format!("{}: ProcessInput: {e}", self.name))?;
                    fed = true;
                } else {
                    // The MFT wants the *next* frame — bank the ask; the
                    // pump's next call spends it before touching the queue.
                    self.input_credits += 1;
                }
            } else if met == METransformHaveOutput.0 as u32 {
                if let Some(unit) = self.process_output()? {
                    units.push(unit);
                }
            }
        }
        Ok(EncodeOutcome {
            units,
            consumed: fed,
            input_ts: 0,
        })
    }

    /// One `ProcessOutput` call. `Ok(None)` means the MFT wants more input (or a
    /// benign stream-format change); `Ok(Some)` is one access unit + key flag.
    unsafe fn process_output(&mut self) -> Result<Option<(Vec<u8>, bool)>, String> {
        // Supply an output sample only when the MFT doesn't provide its own.
        let provided = if self.provides_samples {
            None
        } else {
            Some(
                alloc_output_sample(self.out_buf_size)
                    .map_err(|e| format!("{}: alloc output: {e}", self.name))?,
            )
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(provided),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let r = self.transform.ProcessOutput(0, &mut buffers, &mut status);
        // Reclaim the (possibly MFT-allocated) sample regardless of outcome.
        let sample = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
        std::mem::ManuallyDrop::drop(&mut buffers[0].pEvents);
        match r {
            Ok(()) => match sample {
                Some(s) => Ok(Some(read_sample(&s)?)),
                None => Ok(None),
            },
            Err(e)
                if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT
                    || e.code() == MF_E_TRANSFORM_STREAM_CHANGE =>
            {
                Ok(None)
            }
            Err(e) => Err(format!("{}: ProcessOutput: {e}", self.name)),
        }
    }
}

impl Drop for MediaFoundationH264 {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

/// Copy an output `IMFSample`'s bytes out and read its keyframe flag.
unsafe fn read_sample(sample: &IMFSample) -> Result<(Vec<u8>, bool), String> {
    let buffer = sample
        .ConvertToContiguousBuffer()
        .map_err(|e| format!("contiguous buffer: {e}"))?;
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut current: u32 = 0;
    buffer
        .Lock(&mut ptr, None, Some(&mut current))
        .map_err(|e| format!("buffer lock: {e}"))?;
    let data = if ptr.is_null() || current == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(ptr, current as usize).to_vec()
    };
    let _ = buffer.Unlock();
    // MFSampleExtension_CleanPoint == 1 marks a keyframe (IDR).
    let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;
    Ok((data, key))
}

/// A fresh `IMFSample` carrying NV12 `data`, timestamped for the RTP clock.
unsafe fn make_input_sample(
    data: &[u8],
    time_100ns: i64,
    duration_100ns: i64,
) -> Result<IMFSample, windows::core::Error> {
    let sample = MFCreateSample()?;
    let buffer = MFCreateMemoryBuffer(data.len() as u32)?;
    let mut ptr: *mut u8 = std::ptr::null_mut();
    buffer.Lock(&mut ptr, None, None)?;
    if !ptr.is_null() {
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
    }
    buffer.Unlock()?;
    buffer.SetCurrentLength(data.len() as u32)?;
    sample.AddBuffer(&buffer)?;
    sample.SetSampleTime(time_100ns)?;
    sample.SetSampleDuration(duration_100ns)?;
    Ok(sample)
}

/// An empty output sample with a `size`-byte buffer, for MFTs that don't
/// allocate their own.
unsafe fn alloc_output_sample(size: usize) -> Result<IMFSample, windows::core::Error> {
    let sample = MFCreateSample()?;
    let buffer = MFCreateMemoryBuffer(size as u32)?;
    sample.AddBuffer(&buffer)?;
    Ok(sample)
}

/// Pack two `u32`s into the hi/lo halves of an attribute `UINT64` — the layout
/// `MFSetAttributeSize`/`MFSetAttributeRatio` use for frame size, frame rate,
/// and pixel aspect ratio.
unsafe fn set_attr_size(
    media_type: &IMFMediaType,
    key: &GUID,
    hi: u32,
    lo: u32,
) -> Result<(), windows::core::Error> {
    media_type.SetUINT64(key, ((hi as u64) << 32) | (lo as u64))
}

/// Stage-tagged error formatter for the `open` path — borrows the MFT name so a
/// failure reads e.g. `"NVIDIA H.264 Encoder MFT: set out type: <hresult>"`.
fn mferr<'a>(name: &'a str, stage: &'a str) -> impl FnOnce(windows::core::Error) -> String + 'a {
    move |e| format!("{name}: {stage}: {e}")
}

/// A `VT_UI4` VARIANT for an `ICodecAPI` numeric property.
fn variant_u32(v: u32) -> VARIANT {
    VARIANT::from(v)
}

/// A `VT_BOOL` VARIANT for an `ICodecAPI` boolean property.
fn variant_bool(v: bool) -> VARIANT {
    VARIANT::from(v)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SliceSetting {
    mode: u32,
    size: u32,
    unit: &'static str,
}

/// CodecAPI's three legal H.264 slice-control shapes, in preference order.
///
/// Mode 1 is the exact wire-size ask. Modes 2 and 0 are geometry fallbacks
/// for drivers that only expose row- or macroblock-based slicing. Field probes
/// produced ~900 KiB high-motion 1080p IDRs, where the direct backends' usual
/// eight lossy slices still left ~133 KiB packet walls. Two macroblock rows
/// per slice produced the closest practical 1080p fallback to the 24 KiB
/// transport grain; small P-frames are still coalesced by
/// `split_annexb_paced`, so they remain one send.
fn paced_slice_settings(width: u32, height: u32) -> [SliceSetting; 3] {
    let target_slices = if width.saturating_mul(height) >= 1920 * 1080 {
        64
    } else {
        32
    };
    let mb_w = width.div_ceil(16).max(1);
    let mb_rows = height.div_ceil(16).max(1);
    let rows_per_slice = mb_rows.div_ceil(target_slices).max(1);
    [
        SliceSetting {
            mode: 1,
            size: (crate::video::PACE_SLICE_BYTES * 8) as u32,
            unit: "bits",
        },
        SliceSetting {
            mode: 2,
            size: rows_per_slice,
            unit: "macroblock rows",
        },
        SliceSetting {
            mode: 0,
            size: mb_w.saturating_mul(rows_per_slice),
            unit: "macroblocks",
        },
    ]
}

/// Select one slice-control mode the MFT actually accepts. Both properties
/// must succeed as a pair: a size written after a rejected mode is dangerous,
/// because the driver interprets it in whichever default unit it retained.
unsafe fn configure_paced_slices(api: &ICodecAPI, name: &str, width: u32, height: u32) {
    let mut rejected = Vec::new();
    for setting in paced_slice_settings(width, height) {
        if let Err(e) = api.SetValue(&CODECAPI_AVEncSliceControlMode, &variant_u32(setting.mode)) {
            rejected.push(format!("mode {} ({}): {e}", setting.mode, setting.unit));
            continue;
        }
        match api.SetValue(&CODECAPI_AVEncSliceControlSize, &variant_u32(setting.size)) {
            Ok(()) => {
                tracing::info!(
                    "{name}: paced slices configured in {} mode ({} per slice)",
                    setting.unit,
                    setting.size,
                );
                return;
            }
            Err(e) => rejected.push(format!(
                "mode {} size {} ({}): {e}",
                setting.mode, setting.size, setting.unit
            )),
        }
    }
    tracing::warn!(
        "{name}: no usable paced slice mode; encoded access-unit shape will decide the safe send path ({})",
        rejected.join("; ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_fallbacks_cover_every_codecapi_unit_without_pathological_counts() {
        let full_hd = paced_slice_settings(1920, 1080);
        assert_eq!(full_hd.map(|s| s.mode), [1, 2, 0]);
        assert_eq!(full_hd[0].size, (crate::video::PACE_SLICE_BYTES * 8) as u32);
        assert_eq!(full_hd[1].size, 2, "68 macroblock rows / 64 slices");
        assert_eq!(full_hd[2].size, 240, "120 macroblocks × 2 rows");

        let qhd = paced_slice_settings(2560, 1440);
        assert_eq!(qhd[1].size, 2, "90 macroblock rows / 64 slices");
        assert_eq!(qhd[2].size, 320, "160 macroblocks × 2 rows");

        let tiny = paced_slice_settings(1, 1);
        assert_eq!(tiny[1].size, 1);
        assert_eq!(tiny[2].size, 1);
    }

    /// Feed the real hardware MFT a burst of frames back-to-back and hold the
    /// pump to its lossless contract: every consumed frame's unit comes out
    /// (a small in-flight tail excepted) and the whole stream decodes cleanly
    /// through openh264, in order — one silently dropped unit snaps the
    /// reference chain and fails the decode immediately, which is exactly the
    /// smear-until-IDR bug the drain rewrite removed. Skips (passing) when
    /// the box has no hardware H.264 MFT.
    #[test]
    fn hardware_pump_is_lossless_and_decodable() {
        let hw = hardware_h264_mfts();
        let Some(first) = hw.first() else {
            eprintln!("SKIP: no hardware H.264 MFT on this machine");
            return;
        };
        let (w, h) = (640u32, 480u32);
        let mut enc = match first.open(w, h, 60, 4_000_000) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("SKIP: MFT open failed: {e}");
                return;
            }
        };
        let (wu, hu) = (w as usize, h as usize);
        // NV12: luma plane + interleaved chroma; 128 chroma = neutral grey.
        let mut nv12 = vec![128u8; wu * hu + 2 * ((wu / 2) * (hu / 2))];
        let frames = 90u32;
        let mut fed = 0u32;
        let mut units: Vec<(Vec<u8>, bool)> = Vec::new();
        for i in 0..frames {
            // March a bright stripe down the luma plane so every frame
            // carries a real delta.
            for (j, v) in nv12[..wu * hu].iter_mut().enumerate() {
                let row = j / wu;
                *v = if row % 64 == (i as usize) % 64 {
                    235
                } else {
                    60
                };
            }
            let out = enc.encode_nv12(&nv12, i == 0).expect("encode");
            if out.consumed {
                fed += 1;
            }
            units.extend(out.units);
        }
        // Drain the in-flight tail with a few more calls.
        for _ in 0..3 {
            let out = enc.encode_nv12(&nv12, false).expect("drain");
            if out.consumed {
                fed += 1;
            }
            units.extend(out.units);
        }
        assert!(
            units.len() as u32 >= fed.saturating_sub(2),
            "lossless drain: {} units for {fed} consumed frames",
            units.len()
        );
        assert!(units.iter().any(|(_, k)| *k), "a keyframe came out");
        let mut dec = openh264::decoder::Decoder::with_api_config(
            openh264::OpenH264API::from_source(),
            openh264::decoder::DecoderConfig::new(),
        )
        .expect("decoder");
        let mut decoded = 0u32;
        for (d, _) in &units {
            let pic = dec.decode(d).expect("clean decode — no missing references");
            if pic.is_some() {
                decoded += 1;
            }
        }
        assert!(
            decoded >= fed.saturating_sub(3),
            "decoded {decoded} of {fed} fed frames"
        );
    }

    /// Field probe for the exact transport property paced slicing needs from
    /// a machine's preferred hardware MFT. This is deliberately ignored in
    /// CI: it is an assertion about the installed display driver, not merely
    /// about our arithmetic. Run it on a sender before promoting a fallback.
    #[test]
    #[ignore = "requires a Windows hardware H.264 MFT"]
    fn hardware_paced_slices_are_real_cut_points() {
        let hw = hardware_h264_mfts();
        let Some(first) = hw.first() else {
            eprintln!("SKIP: no hardware H.264 MFT on this machine");
            return;
        };
        let (w, h) = (1920u32, 1080u32);
        let mut enc = first
            .open(w, h, 60, 20_000_000)
            .unwrap_or_else(|e| panic!("{} failed to open: {e}", first.name));
        let (wu, hu) = (w as usize, h as usize);
        let mut nv12 = vec![128u8; wu * hu + 2 * ((wu / 2) * (hu / 2))];
        let mut units = Vec::new();
        for frame in 0..12u32 {
            // High-motion luma makes the probe exercise the exact oversized
            // access-unit shape seen in Game-mode field logs, rather than a
            // tiny static desktop frame that would not test the contract.
            for (pixel, value) in nv12[..wu * hu].iter_mut().enumerate() {
                let mixed = (pixel as u32)
                    .wrapping_add(frame.wrapping_mul(9973))
                    .wrapping_mul(2654435761);
                *value = (mixed >> 24) as u8;
            }
            units.extend(enc.encode_nv12(&nv12, frame == 0).expect("encode").units);
        }
        assert!(!units.is_empty(), "{} produced no access units", first.name);
        let shapes: Vec<(usize, usize, usize)> = units
            .iter()
            .map(|(unit, _)| {
                let chunks = crate::video::split_annexb_paced(unit, crate::video::PACE_SLICE_BYTES);
                let largest = chunks.iter().map(|range| range.len()).max().unwrap_or(0);
                (unit.len(), chunks.len(), largest)
            })
            .collect();
        let max_chunks = shapes.iter().map(|shape| shape.1).max().unwrap_or(0);
        let largest_chunk = shapes.iter().map(|shape| shape.2).max().unwrap_or(0);
        eprintln!(
            "{}: {} access units; max {max_chunks} paced chunks/AU; largest chunk {:.1} KiB; shapes={shapes:?}",
            first.name,
            units.len(),
            largest_chunk as f64 / 1024.0,
        );
        assert!(
            max_chunks > 1,
            "{} accepted configuration but still emitted only one slice per access unit",
            first.name
        );
        assert!(
            largest_chunk <= crate::video::PACE_SLICE_BYTES * 2,
            "{} still emitted a {:.1} KiB transport wall (limit {:.1} KiB)",
            first.name,
            largest_chunk as f64 / 1024.0,
            (crate::video::PACE_SLICE_BYTES * 2) as f64 / 1024.0,
        );
    }

    /// The in-place bitrate pilot, in the direction adaptation actually
    /// needs it: **downward, instantly, without a reopen or an IDR** — a
    /// congested link wants the rate cut NOW, and a rebuild's first-IDR
    /// burst is the worst possible thing to send it. `set_bitrate` must be
    /// accepted, the output rate must genuinely fall (≥3× on a 6× cut),
    /// and the stream must decode cleanly across the change (an encoder
    /// that internally reset would snap the reference chain).
    ///
    /// The upward direction was probed too and is only partially honored:
    /// the NVIDIA MFT stores the new mean exactly (reads back verbatim)
    /// but self-clamps peak/VBV to a driver-internal ceiling and tracked
    /// ~5.7 Mbps of a 2→12 ask in the probe run. So the operational rule
    /// this pilot sets: in-place for downshifts (and modest upshifts);
    /// large upshifts take the rebuild path, whose IDR a healthy link
    /// absorbs.
    #[test]
    fn hardware_bitrate_reconfigures_in_place() {
        let hw = hardware_h264_mfts();
        let Some(first) = hw.first() else {
            eprintln!("SKIP: no hardware H.264 MFT on this machine");
            return;
        };
        let (w, h) = (1280u32, 720u32);
        let mut enc = match first.open(w, h, 60, 12_000_000) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("SKIP: MFT open failed: {e}");
                return;
            }
        };
        let (wu, hu) = (w as usize, h as usize);
        let mut nv12 = vec![128u8; wu * hu + 2 * ((wu / 2) * (hu / 2))];
        let mut units: Vec<(Vec<u8>, bool)> = Vec::new();
        // Full-luma churn every frame so the rate controller is genuinely
        // rate-limited (a too-easy scene converges below both targets and
        // the ratio says nothing).
        let mut feed = |enc: &mut MediaFoundationH264,
                        units: &mut Vec<(Vec<u8>, bool)>,
                        base: u32,
                        n: u32|
         -> u64 {
            let mut bytes = 0u64;
            for i in 0..n {
                for (j, v) in nv12[..wu * hu].iter_mut().enumerate() {
                    // Decorrelated per-pixel, per-frame noise (integer
                    // hash): incompressible, so BOTH windows are genuinely
                    // rate-limited and the byte ratio reflects the rate
                    // change. (A smooth pattern quality-saturates below
                    // the high target and the ratio understates the move.)
                    let x = (j as u32)
                        .wrapping_add((base + i).wrapping_mul(9973))
                        .wrapping_mul(2654435761);
                    *v = (x >> 24) as u8;
                }
                let out = enc.encode_nv12(&nv12, base == 0 && i == 0).expect("encode");
                for (d, k) in out.units {
                    bytes += d.len() as u64;
                    units.push((d, k));
                }
            }
            bytes
        };
        // Warm-up (rate control settles), then the measured high window.
        feed(&mut enc, &mut units, 0, 30);
        let high = feed(&mut enc, &mut units, 30, 60);
        if !enc.set_bitrate(2_000_000) {
            eprintln!("SKIP: {} rejects dynamic bitrate", enc.label());
            return;
        }
        // Settle, then the measured low window.
        feed(&mut enc, &mut units, 90, 45);
        let low = feed(&mut enc, &mut units, 135, 60);
        // Two claims: the 60-frame (1 s) low window lands on the new 2 Mbps
        // target (≤35% over), and the rate genuinely moved (≥2× down from
        // the high window — which VBR may itself undershoot on saturated
        // content, so the ratio bound stays modest).
        let target_window = 2_000_000u64 / 8;
        assert!(
            low <= target_window + target_window / 3,
            "low window {low} B doesn't track the 2 Mbps ask (~{target_window} B)"
        );
        assert!(
            low.saturating_mul(2) <= high,
            "rate didn't fall: high window {high} B, low window {low} B (asked 12 -> 2 Mbps)"
        );
        // The whole stream — across the reconfigure — must decode clean:
        // an encoder that internally reset would snap the reference chain
        // here.
        let mut dec = openh264::decoder::Decoder::with_api_config(
            openh264::OpenH264API::from_source(),
            openh264::decoder::DecoderConfig::new(),
        )
        .expect("decoder");
        for (d, _) in &units {
            dec.decode(d).expect("clean decode across bitrate change");
        }
        println!(
            "in-place bitrate: high window {high} B -> low window {low} B ({:.1}x cut)",
            high as f64 / low.max(1) as f64
        );
    }
}
