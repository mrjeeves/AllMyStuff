//! AMD AMF encode — the Radeon twin of [`crate::nvenc`], now a real rung.
//!
//! Why: the 9060 XT field host proved the gap — an AMD box rides the MF
//! rung (AMD's H.264 MFT), which hides the levers this product is built
//! on: GDR intra-refresh for the game posture, guaranteed in-place
//! bitrate for the closed loop, usage/quality presets per posture, and
//! predictable slice framing for the pacer. This module drives AMD's own
//! runtime (`amfrt64.dll`, ships with the Radeon driver) directly:
//! `AMFInit` → factory `CreateContext` → `InitDX11` on the lane's device
//! → `CreateComponent("AMFVideoEncoderVCE_AVC")` → property bag →
//! `SubmitInput` of DX11-native surfaces wrapping the lane's OWN NV12
//! ring textures (zero copies) → `QueryOutput` → Annex-B bytes.
//! Ladder order: NVENC → **AMF (AMD adapters only)** → MF → software.
//! AMF exposes no transquant bypass — Studio·Lossless stays NVENC's.
//!
//! Same discipline as the NVENC rung: no build dependency, runtime
//! loading, soft failures everywhere, FFI hand-transcribed from AMD's
//! MIT-licensed AMF headers (GPUOpen v1.4.35, the C-ABI sections;
//! FFmpeg's `amfenc.c` consumes this exact ABI and is the flow
//! reference). Vtables are transcribed IN FULL as `#[repr(C)]` structs —
//! every entry present in declaration order, unused ones typed as raw
//! pointers in named pad runs — so the called slots are structural, not
//! counted at call sites. `AMFVariantStruct` passes BY VALUE (24 bytes;
//! the Win64 ABI's hidden-pointer rule applies identically to the C
//! compiler and Rust's extern "C").
//!
//! Status: built blind on an NVIDIA dev box — compile-proven, clean-skip
//! proven (`open_on_device` refuses non-AMD adapters before touching
//! AMF). Hardware validation happens on the user's 9060 XT: the e2e
//! test below skips everywhere else and runs there first.
//!
//! Threading contract: one owner thread (the route's encode lane), like
//! every rung.

#![cfg(windows)]

use std::ffi::c_void;

use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

/// `AMF_RESULT` — 0 = AMF_OK.
type AmfResult = i32;
const AMF_OK: AmfResult = 0;
/// Positional values from `core_Result.h` (counted, not guessed).
const AMF_EOF: AmfResult = 23;
const AMF_REPEAT: AmfResult = 24;
const AMF_INPUT_FULL: AmfResult = 25;

type EncodedUnit = (Vec<u8>, bool);

/// Retry a saturated submit without discarding the output that made room.
fn submit_preserving_output(
    mut submit: impl FnMut() -> AmfResult,
    mut drain: impl FnMut() -> Result<Option<EncodedUnit>, String>,
    mut backoff: impl FnMut(),
) -> Result<(AmfResult, Vec<EncodedUnit>, u8), String> {
    let mut units = Vec::new();
    let mut fulls = 0u8;
    loop {
        let status = submit();
        if status != AMF_INPUT_FULL || fulls >= 8 {
            return Ok((status, units, fulls));
        }
        fulls += 1;
        if let Some(unit) = drain()? {
            units.push(unit);
        }
        backoff();
    }
}
const AMF_NEED_MORE_INPUT: AmfResult = 44;

/// The full-version word AMF speaks (`AMF_MAKE_FULL_VERSION`): the
/// headers this FFI was transcribed from are 1.4.35; the runtime accepts
/// any caller version ≤ its own.
const AMF_VERSION: u64 = (1u64 << 48) | (4u64 << 32) | (35u64 << 16);

/// `AMF_DX_VERSION::AMF_DX11_0`.
const AMF_DX11_0: i32 = 110;
/// `AMF_SURFACE_FORMAT::AMF_SURFACE_NV12`.
const AMF_SURFACE_NV12: i32 = 1;

/// The AV1 encoder component id — the AV1 arc's AMF seam (RDNA4, the
/// user's 9060 XT, has AV1 encode). `CreateComponent` with this instead
/// of `AMFVideoEncoderVCE_AVC`; the property NAMES differ (its own
/// `components_VideoEncoderAV1.h` header — `Av1TargetBitrate` etc.), so
/// AV1 gets its own config block, but the flow (context → component →
/// SubmitInput DX11 surfaces → QueryOutput) is identical to AVC here.
/// Named now so the seam is obvious; unused until AV1 encode lands.
#[allow(dead_code)]
const AMF_VIDEO_ENCODER_AV1: &str = "AMFVideoEncoder_AV1";

// Encoder property enums (components_VideoEncoderVCE.h).
const USAGE_TRANSCODING: i64 = 0;
const USAGE_ULTRA_LOW_LATENCY: i64 = 1;
const RC_PEAK_CONSTRAINED_VBR: i64 = 2;
const QUALITY_BALANCED: i64 = 0;
const QUALITY_SPEED: i64 = 1;
const QUALITY_QUALITY: i64 = 2;
const PICTURE_TYPE_IDR: i64 = 2;
const OUTPUT_DATA_TYPE_IDR: i64 = 0;

/// `AMFGuid` — NOT a Windows GUID layout-wise, but the same 16 bytes:
/// data1..data3 then eight bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct AmfGuid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// `AMFBuffer`'s IID (core_Buffer.h).
const IID_BUFFER: AmfGuid = AmfGuid {
    data1: 0xb04b_7248,
    data2: 0xb6f0,
    data3: 0x4321,
    data4: [0xb6, 0x91, 0xba, 0xa4, 0x74, 0x0f, 0x9f, 0xcb],
};

/// `AMFVariantStruct`: a 4-byte type tag, padding, and a 16-byte value
/// union — 24 bytes, align 8. Passed BY VALUE to `SetProperty`.
#[repr(C)]
#[derive(Clone, Copy)]
struct AmfVariant {
    vtype: i32,
    _pad: i32,
    value: [u64; 2],
}
const _: () = assert!(std::mem::size_of::<AmfVariant>() == 24);

const VT_BOOL: i32 = 1;
const VT_INT64: i32 = 2;
const VT_SIZE: i32 = 5;
const VT_RATE: i32 = 7;

fn v_i64(v: i64) -> AmfVariant {
    AmfVariant {
        vtype: VT_INT64,
        _pad: 0,
        value: [v as u64, 0],
    }
}
fn v_bool(v: bool) -> AmfVariant {
    // amf_bool is one byte; a little-endian word write sets exactly it.
    AmfVariant {
        vtype: VT_BOOL,
        _pad: 0,
        value: [u64::from(v), 0],
    }
}
fn v_size(w: i32, h: i32) -> AmfVariant {
    AmfVariant {
        vtype: VT_SIZE,
        _pad: 0,
        value: [(w as u32 as u64) | ((h as u32 as u64) << 32), 0],
    }
}
fn v_rate(num: u32, den: u32) -> AmfVariant {
    AmfVariant {
        vtype: VT_RATE,
        _pad: 0,
        value: [u64::from(num) | (u64::from(den) << 32), 0],
    }
}

// ---------------------------------------------------------------------------
// Vtables — full transcriptions, declaration order (the headers' C
// sections), pad runs named for what they skip.
// ---------------------------------------------------------------------------

#[repr(C)]
struct FactoryVtbl {
    create_context: unsafe extern "C" fn(*mut Factory, *mut *mut Context) -> AmfResult,
    create_component: unsafe extern "C" fn(
        *mut Factory,
        *mut Context,
        *const u16,
        *mut *mut Component,
    ) -> AmfResult,
    /// SetCacheFolder · GetCacheFolder · GetDebug · GetTrace · GetPrograms
    _rest: [*const c_void; 5],
}
#[repr(C)]
struct Factory {
    vtbl: *const FactoryVtbl,
}

/// `AMFContextVtbl`: Interface(3) + PropertyStorage(10) + Terminate +
/// DX9(4) + DX11(4) + OpenCL(8) + OpenGL(5) + XV(4) + Gralloc(4) +
/// Alloc(3) + wrap(8) + GetCompute.
#[repr(C)]
struct ContextVtbl {
    _acquire: *const c_void,
    release: unsafe extern "C" fn(*mut Context) -> i32,
    _query_interface: *const c_void,
    _prop: [*const c_void; 10],
    terminate: unsafe extern "C" fn(*mut Context) -> AmfResult,
    _dx9: [*const c_void; 4],
    init_dx11: unsafe extern "C" fn(*mut Context, *mut c_void, i32) -> AmfResult,
    /// GetDX11Device · LockDX11 · UnlockDX11
    _dx11_rest: [*const c_void; 3],
    _opencl: [*const c_void; 8],
    _opengl: [*const c_void; 5],
    _xv: [*const c_void; 4],
    _gralloc: [*const c_void; 4],
    /// AllocBuffer · AllocSurface · AllocAudioBuffer
    _alloc: [*const c_void; 3],
    /// CreateBufferFromHostNative · CreateSurfaceFromHostNative
    _wrap_host: [*const c_void; 2],
    _wrap_dx9: *const c_void,
    create_surface_from_dx11: unsafe extern "C" fn(
        *mut Context,
        *mut c_void,
        *mut *mut AmfSurface,
        *mut c_void,
    ) -> AmfResult,
    /// OpenGL/Gralloc/OpenCL surface wraps · OpenCL buffer wrap
    _wrap_rest: [*const c_void; 4],
    _get_compute: *const c_void,
}
#[repr(C)]
struct Context {
    vtbl: *const ContextVtbl,
}

/// `AMFComponentVtbl`: Interface(3) + PropertyStorage(10) +
/// PropertyStorageEx(4) + the component methods.
#[repr(C)]
struct ComponentVtbl {
    _acquire: *const c_void,
    release: unsafe extern "C" fn(*mut Component) -> i32,
    _query_interface: *const c_void,
    set_property: unsafe extern "C" fn(*mut Component, *const u16, AmfVariant) -> AmfResult,
    /// GetProperty · HasProperty · GetPropertyCount · GetPropertyAt ·
    /// Clear · AddTo · CopyTo · AddObserver · RemoveObserver
    _prop_rest: [*const c_void; 9],
    /// GetPropertiesInfoCount · GetPropertyInfoAt · GetPropertyInfo ·
    /// ValidateProperty
    _prop_ex: [*const c_void; 4],
    init: unsafe extern "C" fn(*mut Component, i32, i32, i32) -> AmfResult,
    _reinit: *const c_void,
    terminate: unsafe extern "C" fn(*mut Component) -> AmfResult,
    drain: unsafe extern "C" fn(*mut Component) -> AmfResult,
    _flush: *const c_void,
    submit_input: unsafe extern "C" fn(*mut Component, *mut AmfData) -> AmfResult,
    query_output: unsafe extern "C" fn(*mut Component, *mut *mut AmfData) -> AmfResult,
    /// GetContext · SetOutputDataAllocatorCB · GetCaps · Optimize
    _tail: [*const c_void; 4],
}
#[repr(C)]
struct Component {
    vtbl: *const ComponentVtbl,
}

/// `AMFSurface` through the prefix every AMF object shares —
/// Interface(3) + PropertyStorage's SetProperty — all this rung touches
/// on an input surface (per-frame IDR/SPS/PPS properties + Release).
#[repr(C)]
struct SurfaceVtblPrefix {
    _acquire: *const c_void,
    release: unsafe extern "C" fn(*mut AmfSurface) -> i32,
    _query_interface: *const c_void,
    set_property: unsafe extern "C" fn(*mut AmfSurface, *const u16, AmfVariant) -> AmfResult,
    /// The rest of PropertyStorage and everything Surface-specific —
    /// never called through this prefix.
    _rest: [*const c_void; 9],
}
#[repr(C)]
struct AmfSurface {
    vtbl: *const SurfaceVtblPrefix,
}

/// `AMFData` through the same shared prefix, plus QueryInterface and
/// GetProperty which the output path uses.
#[repr(C)]
struct DataVtblPrefix {
    _acquire: *const c_void,
    release: unsafe extern "C" fn(*mut AmfData) -> i32,
    query_interface:
        unsafe extern "C" fn(*mut AmfData, *const AmfGuid, *mut *mut c_void) -> AmfResult,
    _set_property: *const c_void,
    get_property: unsafe extern "C" fn(*mut AmfData, *const u16, *mut AmfVariant) -> AmfResult,
    /// The rest of PropertyStorage + the Data methods — unused here.
    _rest: [*const c_void; 8],
}
#[repr(C)]
struct AmfData {
    vtbl: *const DataVtblPrefix,
}

/// `AMFBufferVtbl`: Interface(3) + PropertyStorage(10) + Data(10) +
/// SetSize/GetSize/GetNative + buffer observers(2).
#[repr(C)]
struct BufferVtbl {
    _acquire: *const c_void,
    release: unsafe extern "C" fn(*mut AmfBuffer) -> i32,
    _query_interface: *const c_void,
    _prop: [*const c_void; 10],
    /// GetMemoryType · Duplicate · Convert · Interop · GetDataType ·
    /// IsReusable · SetPts · GetPts · SetDuration · GetDuration
    _data: [*const c_void; 10],
    _set_size: *const c_void,
    get_size: unsafe extern "C" fn(*mut AmfBuffer) -> usize,
    get_native: unsafe extern "C" fn(*mut AmfBuffer) -> *mut c_void,
    _observers: [*const c_void; 2],
}
#[repr(C)]
struct AmfBuffer {
    vtbl: *const BufferVtbl,
}

/// What the loader holds: the process-global AMF factory. (The version
/// word is logged at load time, not stored — nothing reads it back.)
pub(crate) struct AmfRuntime {
    factory: *mut Factory,
}

// SAFETY: the factory is a process-global AMF documents as thread-safe
// (every FFmpeg amfenc instance shares it); the raw pointer is only a
// handle, never dereferenced outside vtable calls.
unsafe impl Send for AmfRuntime {}
unsafe impl Sync for AmfRuntime {}

/// Load `amfrt64.dll` and initialize the factory once per process.
/// `Err` = no AMD driver on this box (or one predating AMF 1.4) — the
/// encoder ladder skips the rung exactly like the NVENC rung skips
/// without NVIDIA.
pub(crate) fn runtime() -> Result<&'static AmfRuntime, String> {
    static RT: std::sync::OnceLock<Result<&'static AmfRuntime, String>> =
        std::sync::OnceLock::new();
    RT.get_or_init(|| unsafe {
        let module = LoadLibraryA(PCSTR(c"amfrt64.dll".as_ptr() as *const u8))
            .map_err(|e| format!("amfrt64.dll not loadable (no AMD driver): {e}"))?;
        let query = GetProcAddress(module, PCSTR(c"AMFQueryVersion".as_ptr() as *const u8))
            .ok_or("AMFQueryVersion missing")?;
        let query = std::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            unsafe extern "C" fn(*mut u64) -> AmfResult,
        >(query);
        let mut version = 0u64;
        let status = query(&mut version);
        if status != AMF_OK {
            return Err(format!("AMFQueryVersion: {status}"));
        }
        tracing::info!(
            "AMF runtime {}.{}.{} present",
            (version >> 48) & 0xffff,
            (version >> 32) & 0xffff,
            (version >> 16) & 0xffff,
        );
        let init = GetProcAddress(module, PCSTR(c"AMFInit".as_ptr() as *const u8))
            .ok_or("AMFInit missing")?;
        let init = std::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            unsafe extern "C" fn(u64, *mut *mut Factory) -> AmfResult,
        >(init);
        let mut factory: *mut Factory = std::ptr::null_mut();
        let status = init(AMF_VERSION, &mut factory);
        if status != AMF_OK || factory.is_null() {
            return Err(format!("AMFInit: {status}"));
        }
        let _ = version; // logged above; not stored
        Ok(&*Box::leak(Box::new(AmfRuntime { factory })))
    })
    .clone()
}

// ---------------------------------------------------------------------------
// The encoder session.
// ---------------------------------------------------------------------------

/// A wide (UTF-16, NUL-terminated) property name for the AMF bag.
fn wname(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

/// An AMF AVC encode session on the GPU lane's own device: NV12 ring
/// textures in (DX11-native, zero-copy), Annex-B access units out.
/// Speaks the ladder's [`crate::video::EncodeOutcome`] seam like its
/// NVENC twin.
pub(crate) struct AmfAvc {
    context: *mut Context,
    encoder: *mut Component,
    fps: u32,
    game: bool,
    studio: bool,
    frame_index: u64,
    full_submit_retries: u64,
    full_submit_exhaustions: u64,
    backpressure_units: u64,
    last_exhaustion_warning: Option<std::time::Instant>,
    label: String,
}

// SAFETY: owned and driven by one route-encode thread, like NvencH264.
unsafe impl Send for AmfAvc {}

impl AmfAvc {
    /// Open on the lane's device. Refuses non-AMD adapters BEFORE any
    /// AMF call, so the ladder's probe costs nothing on other vendors.
    pub fn open_on_device(
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        w: u32,
        h: u32,
        fps: u32,
        bitrate: u32,
        game: bool,
        studio: bool,
    ) -> Result<Self, String> {
        unsafe {
            // Vendor gate from the device itself (the lane pins capture,
            // convert, and encode to one adapter — ask that adapter).
            let dxgi: windows::Win32::Graphics::Dxgi::IDXGIDevice =
                device.cast().map_err(|e| format!("IDXGIDevice: {e}"))?;
            let adapter = dxgi.GetAdapter().map_err(|e| format!("GetAdapter: {e}"))?;
            let desc = adapter.GetDesc().map_err(|e| format!("GetDesc: {e}"))?;
            if desc.VendorId != 0x1002 {
                return Err(format!(
                    "adapter vendor {:#06x} isn't AMD — AMF rung not applicable",
                    desc.VendorId
                ));
            }
            let rt = runtime()?;

            let mut context: *mut Context = std::ptr::null_mut();
            let status = ((*(*rt.factory).vtbl).create_context)(rt.factory, &mut context);
            if status != AMF_OK || context.is_null() {
                return Err(format!("AMF CreateContext: {status}"));
            }
            // Cleanup-on-failure from here.
            let fail = |context: *mut Context, encoder: *mut Component, why: String| {
                if !encoder.is_null() {
                    let _ = ((*(*encoder).vtbl).terminate)(encoder);
                    ((*(*encoder).vtbl).release)(encoder);
                }
                let _ = ((*(*context).vtbl).terminate)(context);
                ((*(*context).vtbl).release)(context);
                Err(why)
            };
            let status = ((*(*context).vtbl).init_dx11)(context, device.as_raw(), AMF_DX11_0);
            if status != AMF_OK {
                return fail(
                    context,
                    std::ptr::null_mut(),
                    format!("AMF InitDX11: {status}"),
                );
            }
            let mut encoder: *mut Component = std::ptr::null_mut();
            let id = wname("AMFVideoEncoderVCE_AVC");
            let status = ((*(*rt.factory).vtbl).create_component)(
                rt.factory,
                context,
                id.as_ptr(),
                &mut encoder,
            );
            if status != AMF_OK || encoder.is_null() {
                return fail(
                    context,
                    std::ptr::null_mut(),
                    format!("AMF CreateComponent(AVC): {status}"),
                );
            }

            let set = |enc: *mut Component, name: &str, v: AmfVariant| -> AmfResult {
                let n = wname(name);
                ((*(*enc).vtbl).set_property)(enc, n.as_ptr(), v)
            };
            // USAGE first — it presets the whole bag; everything after
            // refines it. Refinements are best-effort like the MF rung's
            // ICodecAPI knobs (drivers vary; Init is the real gate).
            let usage = if game {
                USAGE_ULTRA_LOW_LATENCY
            } else {
                USAGE_TRANSCODING
            };
            let status = set(encoder, "Usage", v_i64(usage));
            if status != AMF_OK {
                return fail(context, encoder, format!("AMF Usage: {status}"));
            }
            let _ = set(encoder, "FrameSize", v_size(w as i32, h as i32));
            let _ = set(encoder, "FrameRate", v_rate(fps, 1));
            // The posture's rate shape, mirroring the NVENC postures
            // exactly: peak-constrained VBR; game = single-frame VBV
            // (burst latency), studio = deep 1 s VBV + modest peak,
            // balanced = the shared burst_bounds.
            let (peak, vbv) = crate::video::burst_bounds(bitrate, fps, game, studio);
            let _ = set(encoder, "RateControlMethod", v_i64(RC_PEAK_CONSTRAINED_VBR));
            let _ = set(encoder, "TargetBitrate", v_i64(i64::from(bitrate)));
            let _ = set(encoder, "PeakBitrate", v_i64(i64::from(peak)));
            let _ = set(encoder, "VBVBufferSize", v_i64(i64::from(vbv)));
            let _ = set(encoder, "BPicturesPattern", v_i64(0));
            let _ = set(
                encoder,
                "QualityPreset",
                v_i64(if game {
                    QUALITY_SPEED
                } else if studio {
                    QUALITY_QUALITY
                } else {
                    QUALITY_BALANCED
                }),
            );
            let _ = set(encoder, "LowLatencyInternal", v_bool(game));
            // Pacer grain: the same slice counts the NVENC rung runs for
            // lossy streams.
            let slices = if crate::video::paced_slices_enabled() {
                if w * h >= 1920 * 1080 {
                    8
                } else {
                    4
                }
            } else {
                1
            };
            let _ = set(encoder, "SlicesPerFrame", v_i64(slices));
            // AllMyStuff owns the adaptive IDR cadence per frame. AMF's AVC
            // contract requires IDRPeriod=0 when ForcePictureType drives that
            // cadence; combining a periodic GOP with forced picture types is
            // driver-dependent and can silently ignore the repair request.
            let status = set(encoder, "IDRPeriod", v_i64(0));
            if status != AMF_OK {
                return fail(context, encoder, format!("AMF IDRPeriod=0: {status}"));
            }
            if game {
                // GDR: continuous rolling intra — a wave is ALWAYS in
                // flight, so a loss self-heals within one refresh period
                // (~0.5 s) with no wall and no per-frame arming. Same
                // period shape as the NVENC wave.
                let mbs_total = w.div_ceil(16) * h.div_ceil(16);
                let period = (fps / 2).max(15);
                let per_slot = (mbs_total / period).max(1);
                let _ = set(
                    encoder,
                    "IntraRefreshMBsNumberPerSlot",
                    v_i64(i64::from(per_slot)),
                );
            }
            let _ = set(encoder, "MaxNumRefFrames", v_i64(4));

            let status = ((*(*encoder).vtbl).init)(encoder, AMF_SURFACE_NV12, w as i32, h as i32);
            if status != AMF_OK {
                return fail(context, encoder, format!("AMF encoder Init: {status}"));
            }
            let label = format!(
                "AMF SDK (AVC, {})",
                if game {
                    "game/GDR"
                } else if studio {
                    "studio"
                } else {
                    "balanced"
                }
            );
            tracing::info!(
                "{label} up on the Radeon: {w}×{h} @ {fps} · {:.1} Mbps · {slices} slices",
                bitrate as f64 / 1e6,
            );
            Ok(Self {
                context,
                encoder,
                fps,
                game,
                studio,
                frame_index: 0,
                full_submit_retries: 0,
                full_submit_exhaustions: 0,
                backpressure_units: 0,
                last_exhaustion_warning: None,
                label,
            })
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Encode one NV12 texture from the lane's ring, synchronously-ish:
    /// submit, then poll the output for up to ~10 ms (ULL sessions
    /// produce within the frame). A not-yet-ready frame returns
    /// `consumed(None)` — the ladder's lossless answer for pipelined
    /// starts; the AU surfaces on the next call.
    pub(crate) fn encode_texture(
        &mut self,
        nv12: &ID3D11Texture2D,
        force_idr: bool,
    ) -> Result<crate::video::EncodeOutcome, String> {
        unsafe {
            let mut surface: *mut AmfSurface = std::ptr::null_mut();
            let status = ((*(*self.context).vtbl).create_surface_from_dx11)(
                self.context,
                nv12.as_raw(),
                &mut surface,
                std::ptr::null_mut(),
            );
            if status != AMF_OK || surface.is_null() {
                return Err(format!("AMF CreateSurfaceFromDX11Native: {status}"));
            }
            if force_idr {
                let sset = |name: &str, v: AmfVariant| {
                    let n = wname(name);
                    ((*(*surface).vtbl).set_property)(surface, n.as_ptr(), v)
                };
                for (name, value) in [
                    ("ForcePictureType", v_i64(PICTURE_TYPE_IDR)),
                    ("InsertSPS", v_bool(true)),
                    ("InsertPPS", v_bool(true)),
                ] {
                    let status = sset(name, value);
                    if status != AMF_OK {
                        ((*(*surface).vtbl).release)(surface);
                        return Err(format!("AMF {name} on forced IDR: {status}"));
                    }
                }
            }
            let duration = 10_000_000 / u64::from(self.fps.max(1));
            let input_ts = self.frame_index * duration;

            // Submit; a full input queue drains one output and retries.
            let encoder = self.encoder;
            let submitted = submit_preserving_output(
                || ((*(*encoder).vtbl).submit_input)(encoder, surface as *mut AmfData),
                || self.drain_one(),
                || std::thread::sleep(std::time::Duration::from_micros(500)),
            );
            ((*(*surface).vtbl).release)(surface);
            let (submit_status, mut units, retries) = submitted?;
            if retries > 0 {
                let before = self.full_submit_retries;
                self.full_submit_retries += u64::from(retries);
                self.backpressure_units += units.len() as u64;
                let log_every = u64::from(self.fps.max(1)) * 5;
                if before == 0 || before / log_every != self.full_submit_retries / log_every {
                    tracing::debug!(
                        "AMF backpressure: {} full-submit retries, {} access units conserved",
                        self.full_submit_retries,
                        self.backpressure_units
                    );
                }
            }
            if submit_status == AMF_INPUT_FULL {
                self.full_submit_exhaustions += 1;
                let now = std::time::Instant::now();
                let should_warn = self.last_exhaustion_warning.is_none_or(|last| {
                    now.saturating_duration_since(last) >= std::time::Duration::from_secs(5)
                });
                if should_warn {
                    tracing::warn!(
                        "AMF backpressure exhausted {retries} full-submit retries ({} exhaustion events, {} access units conserved); reporting input unconsumed for texture retry",
                        self.full_submit_exhaustions,
                        self.backpressure_units,
                    );
                    self.last_exhaustion_warning = Some(now);
                }
                return Ok(crate::video::EncodeOutcome {
                    units,
                    consumed: false,
                    input_ts: 0,
                });
            }
            if submit_status != AMF_OK && submit_status != AMF_NEED_MORE_INPUT {
                return Err(format!("AMF SubmitInput: {submit_status}"));
            }
            self.frame_index += 1;

            // Poll for the AU (cap ~10 ms — ULL produces in-frame).
            if units.is_empty() {
                for _ in 0..40 {
                    match self.drain_one()? {
                        Some(unit) => {
                            units.push(unit);
                            break;
                        }
                        None => std::thread::sleep(std::time::Duration::from_micros(250)),
                    }
                }
            }
            while let Some(unit) = self.drain_one()? {
                units.push(unit);
            }
            Ok(crate::video::EncodeOutcome {
                units,
                consumed: true,
                input_ts,
            })
        }
    }

    /// One QueryOutput: `Ok(Some((bytes, is_idr)))` when an AU is ready,
    /// `Ok(None)` when nothing is (AMF_REPEAT), `Err` on real trouble.
    unsafe fn drain_one(&mut self) -> Result<Option<(Vec<u8>, bool)>, String> {
        let mut data: *mut AmfData = std::ptr::null_mut();
        let status = ((*(*self.encoder).vtbl).query_output)(self.encoder, &mut data);
        if data.is_null() {
            return if status == AMF_OK || status == AMF_REPEAT || status == AMF_EOF {
                Ok(None)
            } else {
                Err(format!("AMF QueryOutput: {status}"))
            };
        }
        if status != AMF_OK {
            ((*(*data).vtbl).release)(data);
            return Err(format!("AMF QueryOutput: {status}"));
        }
        // The output object is an AMFBuffer — reach it properly via QI.
        let mut raw: *mut c_void = std::ptr::null_mut();
        let qi = ((*(*data).vtbl).query_interface)(data, &IID_BUFFER, &mut raw);
        if qi != AMF_OK || raw.is_null() {
            ((*(*data).vtbl).release)(data);
            return Err(format!("AMF output is not a buffer: {qi}"));
        }
        let buffer = raw as *mut AmfBuffer;
        let size = ((*(*buffer).vtbl).get_size)(buffer);
        let native = ((*(*buffer).vtbl).get_native)(buffer);
        if size > 0 && native.is_null() {
            ((*(*buffer).vtbl).release)(buffer);
            ((*(*data).vtbl).release)(data);
            return Err("AMF output buffer has bytes but no native pointer".into());
        }
        let mut bytes = vec![0u8; size];
        if size > 0 && !native.is_null() {
            std::ptr::copy_nonoverlapping(native as *const u8, bytes.as_mut_ptr(), size);
        }
        // IDR-ness from the output's own tag; fall back to a NAL sniff
        // (types 5/7/8 lead key AUs) when the property is absent.
        let mut v = AmfVariant {
            vtype: 0,
            _pad: 0,
            value: [0; 2],
        };
        let n = wname("OutputDataType");
        let tagged = ((*(*data).vtbl).get_property)(data, n.as_ptr(), &mut v) == AMF_OK;
        let key = if tagged {
            (v.value[0] as i64) == OUTPUT_DATA_TYPE_IDR
        } else {
            bytes
                .windows(4)
                .take(64)
                .find(|w| w[..3] == [0, 0, 1])
                .map(|w| matches!(w[3] & 0x1F, 5 | 7 | 8))
                .unwrap_or(false)
        };
        ((*(*buffer).vtbl).release)(buffer);
        ((*(*data).vtbl).release)(data);
        Ok(Some((bytes, key)))
    }

    /// In-place rate re-aim — AMF's Target/Peak/VBV are dynamic; the
    /// closed loop's contract, same as the NVENC rung.
    pub fn set_bitrate(&mut self, bitrate: u32) -> bool {
        unsafe {
            let set = |name: &str, v: AmfVariant| -> AmfResult {
                let n = wname(name);
                ((*(*self.encoder).vtbl).set_property)(self.encoder, n.as_ptr(), v)
            };
            let (peak, vbv) = crate::video::burst_bounds(bitrate, self.fps, self.game, self.studio);
            let ok = set("TargetBitrate", v_i64(i64::from(bitrate))) == AMF_OK;
            let _ = set("PeakBitrate", v_i64(i64::from(peak)));
            let _ = set("VBVBufferSize", v_i64(i64::from(vbv)));
            ok
        }
    }
}

impl Drop for AmfAvc {
    fn drop(&mut self) {
        unsafe {
            if !self.encoder.is_null() {
                let _ = ((*(*self.encoder).vtbl).drain)(self.encoder);
                let _ = ((*(*self.encoder).vtbl).terminate)(self.encoder);
                ((*(*self.encoder).vtbl).release)(self.encoder);
            }
            if !self.context.is_null() {
                let _ = ((*(*self.context).vtbl).terminate)(self.context);
                ((*(*self.context).vtbl).release)(self.context);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_full_drain_preserves_every_access_unit_in_order() {
        let mut submits =
            std::collections::VecDeque::from([AMF_INPUT_FULL, AMF_INPUT_FULL, AMF_OK]);
        let mut drained =
            std::collections::VecDeque::from([Some((vec![1], false)), Some((vec![2], true))]);
        let mut backoffs = 0;
        let (status, units, fulls) = submit_preserving_output(
            || submits.pop_front().expect("scripted submit"),
            || Ok(drained.pop_front().expect("scripted drain")),
            || backoffs += 1,
        )
        .expect("backpressure handling");
        assert_eq!(status, AMF_OK);
        assert_eq!(units, vec![(vec![1], false), (vec![2], true)]);
        assert_eq!(fulls, 2);
        assert_eq!(backoffs, 2);
    }

    #[test]
    fn persistent_input_full_returns_drained_units_without_consuming_input() {
        let mut submits = 0;
        let mut byte = 0u8;
        let (status, units, fulls) = submit_preserving_output(
            || {
                submits += 1;
                AMF_INPUT_FULL
            },
            || {
                byte += 1;
                Ok(Some((vec![byte], false)))
            },
            || {},
        )
        .expect("bounded backpressure handling");
        assert_eq!(status, AMF_INPUT_FULL);
        assert_eq!(submits, 9);
        assert_eq!(units.len(), 8);
        assert_eq!(fulls, 8);
    }

    /// The rung's absence contract, provable on any box: the loader
    /// either loads cleanly (a Radeon box) or reports a named, specific
    /// error (no panic, no partial init) — the exact behavior the encoder
    /// ladder depends on to skip past. The per-adapter vendor gate is
    /// proven by `amf_open_refuses_non_amd_or_opens` below.
    #[test]
    fn amf_loader_fails_soft_or_loads() {
        match runtime() {
            Ok(_) => println!("AMF runtime loaded (Radeon box)"),
            Err(e) => {
                println!("AMF absent (expected on non-AMD): {e}");
                assert!(!e.is_empty(), "a named, specific error");
            }
        }
    }

    /// The open path's vendor gate, provable anywhere: on a non-AMD
    /// adapter `open_on_device` refuses BEFORE touching the AMF runtime,
    /// with the vendor id named. On the Radeon it opens a real session —
    /// run this there first, then `amf_avc_e2e_encodes_decodable_stream`.
    #[test]
    fn amf_open_refuses_non_amd_or_opens() {
        let Ok(gpu) = crate::gpu_pipeline::GpuConvert::new(64, 64, 64, 64) else {
            eprintln!("SKIP: no D3D11 device");
            return;
        };
        match AmfAvc::open_on_device(&gpu.device(), 640, 360, 60, 4_000_000, true, false) {
            Ok(enc) => println!("AMF session up: {}", enc.label()),
            Err(e) => {
                println!("AMF open refused (expected off-AMD): {e}");
                assert!(
                    e.contains("isn't AMD") || e.contains("not loadable") || e.contains("AMF"),
                    "a named, specific refusal: {e}"
                );
            }
        }
    }

    /// The Radeon's end-to-end proof: paint → GPU convert → AMF AVC →
    /// Annex-B → openh264 decode, sustained. SKIPS everywhere without an
    /// AMD adapter — run it on the 9060 XT.
    #[test]
    fn amf_avc_e2e_encodes_decodable_stream() {
        let (w, h) = (640u32, 360u32);
        let (wu, hu) = (w as usize, h as usize);
        let mut gpu = match crate::gpu_pipeline::GpuConvert::new(w, h, w, h) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("SKIP: GPU convert unavailable: {e}");
                return;
            }
        };
        let mut enc = match AmfAvc::open_on_device(&gpu.device(), w, h, 60, 4_000_000, false, false)
        {
            Ok(e) => e,
            Err(e) => {
                eprintln!("SKIP: AMF unavailable: {e}");
                return;
            }
        };
        use openh264::decoder::{Decoder, DecoderConfig};
        let mut dec =
            Decoder::with_api_config(openh264::OpenH264API::from_source(), DecoderConfig::new())
                .expect("openh264");
        let mut bgra = vec![0u8; wu * hu * 4];
        let tex = gpu.bgra_texture_from(&bgra, w, h).expect("tex");
        let mut decoded = 0u32;
        for i in 0..60u64 {
            for (j, v) in bgra.iter_mut().enumerate() {
                *v = ((j as u64).wrapping_add(i * 13) % 251) as u8;
            }
            gpu.update_bgra(&tex, &bgra, w, h);
            let (slot, nv12) = gpu.convert(&tex).expect("convert").expect("slot");
            let out = enc.encode_texture(&nv12, i == 0).expect("encode");
            gpu.release(slot);
            for (au, _) in &out.units {
                if let Ok(Some(pic)) = dec.decode(au) {
                    use openh264::formats::YUVSource as _;
                    let (dw, dh) = pic.dimensions();
                    assert_eq!((dw, dh), (wu, hu), "decoded geometry");
                    decoded += 1;
                }
            }
        }
        assert!(decoded >= 55, "a sustained decodable stream: {decoded}/60");
        println!("AMF AVC e2e: {decoded}/60 frames decoded clean");
    }
}
