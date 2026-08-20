//! NVDEC (nvcuvid) H.264/HEVC decode — the receive twin of
//! [`crate::nvenc`].
//!
//! Why this exists: Studio's Lossless tier encodes HEVC (transquant
//! bypass in Main profile — the flavor hardware decoders open), but the
//! webview can't be relied on to decode HEVC at all: Edge/WebView2
//! delegates it to an OS codec package Microsoft has retired, and
//! Chromium ships no software HEVC. The viewer's native-decode lane
//! ([`crate::video_decode`]) is the path that answers to nobody: this
//! module drives the GPU's decode engine directly and hands back NV12,
//! which the lane converts and ships to the window exactly like its
//! openh264 output.
//!
//! Same discipline as the encode side: no build-time dependency — the
//! API is loaded at runtime from `nvcuvid.dll` + `nvcuda.dll` (both ship
//! with the NVIDIA driver), every failure is soft, and the FFI subset is
//! hand-transcribed from NVIDIA's MIT-licensed dynlink headers
//! (ffnvcodec n12.0.16.0: `dynlink_nvcuvid.h`, `dynlink_cuviddec.h`,
//! `dynlink_cuda.h`), layouts pinned by size asserts. `tcu_ulong` is
//! `unsigned long` — 4 bytes on Windows (LLP64), which this module is
//! `cfg`'d to. `CUVIDPICPARAMS` stays opaque: the parser fills it, we
//! hand the pointer straight back to the decoder.
//!
//! Threading contract: one owner thread (the route's decode thread) —
//! the parser's callbacks fire synchronously inside
//! `cuvidParseVideoData` on the calling thread, so there is no hidden
//! concurrency anywhere in here.

#![cfg(windows)]

use std::ffi::c_void;

use windows::core::PCSTR;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

// ---------------------------------------------------------------------------
// FFI: cuda driver + nvcuvid subsets
// ---------------------------------------------------------------------------

type CuResult = i32;
const CUDA_SUCCESS: CuResult = 0;

type CuContext = *mut c_void;
type CuVideoParser = *mut c_void;
type CuVideoDecoder = *mut c_void;

// `cudaVideoCodec` values from NVIDIA's Video Codec SDK headers. Keep these
// explicit: this crate dynamically loads the driver API and deliberately has
// no SDK/header build dependency.
const CUDA_VIDEO_CODEC_H264: i32 = 4;
const CUDA_VIDEO_CODEC_HEVC: i32 = 8;
/// `cudaVideoCodec_AV1` — the codec id the AV1 rung will pass to
/// `CreateVideoParser`/`CreateDecoder` (the whole HEVC path parameterizes
/// on codec, so AV1 reuses most of it). Named now so the seam is obvious;
/// unused until [`NvdecAv1`] is implemented.
#[allow(dead_code)]
const CUDA_VIDEO_CODEC_AV1: i32 = 11;
const CUDA_VIDEO_CHROMA_420: i32 = 1;
const CUDA_VIDEO_SURFACE_NV12: i32 = 0;
const CUDA_VIDEO_DEINTERLACE_WEAVE: i32 = 0;

const CUVID_PKT_TIMESTAMP: u32 = 0x02;
const CUVID_PKT_ENDOFPICTURE: u32 = 0x08;

const CU_MEMORYTYPE_HOST: i32 = 1;
const CU_MEMORYTYPE_DEVICE: i32 = 2;

/// One bridge feed is one AU (or one paced chunk), so ordinary IPP decode
/// produces at most a handful of callbacks in one synchronous parse. Keep a
/// hard ceiling so a malformed aggregate cannot grow callback state without
/// bound before `decode` regains control.
const MAX_READY_PICTURES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NvCodec {
    H264,
    Hevc,
}

impl NvCodec {
    const fn id(self) -> i32 {
        match self {
            Self::H264 => CUDA_VIDEO_CODEC_H264,
            Self::Hevc => CUDA_VIDEO_CODEC_HEVC,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "HEVC",
        }
    }
}

/// `CUVIDSOURCEDATAPACKET`.
#[repr(C)]
struct SourceDataPacket {
    flags: u32,
    payload_size: u32,
    payload: *const u8,
    timestamp: i64,
}
const _: () = assert!(std::mem::size_of::<SourceDataPacket>() == 24);

/// `CUVIDEOFORMAT` (the sequence callback's payload).
#[repr(C)]
struct VideoFormat {
    codec: i32,
    frame_rate_num: u32,
    frame_rate_den: u32,
    progressive_sequence: u8,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    min_num_decode_surfaces: u8,
    coded_width: u32,
    coded_height: u32,
    display_left: i32,
    display_top: i32,
    display_right: i32,
    display_bottom: i32,
    chroma_format: i32,
    bitrate: u32,
    dar_x: i32,
    dar_y: i32,
    video_signal_description: [u8; 4],
    seqhdr_data_length: u32,
}
const _: () = assert!(std::mem::size_of::<VideoFormat>() == 64);

/// `CUVIDPARSERDISPINFO`.
#[repr(C)]
struct ParserDispInfo {
    picture_index: i32,
    progressive_frame: i32,
    top_field_first: i32,
    repeat_first_field: i32,
    timestamp: i64,
}
const _: () = assert!(std::mem::size_of::<ParserDispInfo>() == 24);

/// The readable prefix of the otherwise-opaque `CUVIDPICPARAMS`.
#[repr(C)]
struct PicParamsPrefix {
    pic_width_in_mbs: i32,
    frame_height_in_mbs: i32,
    curr_pic_idx: i32,
}

type SequenceCb = unsafe extern "system" fn(*mut c_void, *mut VideoFormat) -> i32;
type DecodeCb = unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32;
type DisplayCb = unsafe extern "system" fn(*mut c_void, *mut ParserDispInfo) -> i32;

/// `CUVIDPARSERPARAMS`.
#[repr(C)]
struct ParserParams {
    codec_type: i32,
    max_num_decode_surfaces: u32,
    clock_rate: u32,
    error_threshold: u32,
    max_display_delay: u32,
    /// `bAnnexb:1` (AV1 only) + 31 reserved.
    flags: u32,
    reserved1: [u32; 4],
    user_data: *mut c_void,
    pfn_sequence_callback: Option<SequenceCb>,
    pfn_decode_picture: Option<DecodeCb>,
    pfn_display_picture: Option<DisplayCb>,
    pfn_get_operating_point: *mut c_void,
    pfn_get_sei_msg: *mut c_void,
    reserved2: [*mut c_void; 5],
    ext_video_info: *mut c_void,
}
const _: () = assert!(std::mem::size_of::<ParserParams>() == 136);

/// `CUVIDDECODECREATEINFO` (`tcu_ulong` = u32 on Windows).
#[repr(C)]
struct DecodeCreateInfo {
    width: u32,
    height: u32,
    num_decode_surfaces: u32,
    codec_type: i32,
    chroma_format: i32,
    creation_flags: u32,
    bit_depth_minus8: u32,
    intra_decode_only: u32,
    max_width: u32,
    max_height: u32,
    reserved1: u32,
    display_area: [i16; 4],
    output_format: i32,
    deinterlace_mode: i32,
    target_width: u32,
    target_height: u32,
    num_output_surfaces: u32,
    vid_lock: *mut c_void,
    target_rect: [i16; 4],
    enable_histogram: u32,
    reserved2: [u32; 4],
}
const _: () = assert!(std::mem::size_of::<DecodeCreateInfo>() == 112);

/// `CUVIDPROCPARAMS`.
#[repr(C)]
struct ProcParams {
    progressive_frame: i32,
    second_field: i32,
    top_field_first: i32,
    unpaired_field: i32,
    reserved_flags: u32,
    reserved_zero: u32,
    raw_input_dptr: u64,
    raw_input_pitch: u32,
    raw_input_format: u32,
    raw_output_dptr: u64,
    raw_output_pitch: u32,
    reserved1: u32,
    output_stream: *mut c_void,
    reserved: [u32; 46],
    histogram_dptr: *mut u64,
    reserved2: [*mut c_void; 1],
}
const _: () = assert!(std::mem::size_of::<ProcParams>() == 264);

/// `CUDA_MEMCPY2D` (v2 semantics — `size_t` fields).
#[repr(C)]
struct CudaMemcpy2d {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: i32,
    src_host: *const c_void,
    src_device: u64,
    src_array: *mut c_void,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: i32,
    dst_host: *mut c_void,
    dst_device: u64,
    dst_array: *mut c_void,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}
const _: () = assert!(std::mem::size_of::<CudaMemcpy2d>() == 128);

macro_rules! load_fn {
    ($module:expr, $name:literal, $ty:ty) => {{
        let p = GetProcAddress($module, PCSTR(concat!($name, "\0").as_ptr()))
            .ok_or(concat!($name, " missing"))?;
        // The transmute is fully annotated — the source is the FARPROC fn
        // pointer, the destination is the caller-supplied `$ty`. Clippy's
        // `missing_transmute_annotations` can't see the destination through
        // the macro metavariable, so allow it here at the one definition.
        #[allow(clippy::missing_transmute_annotations)]
        let f = std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(p);
        f
    }};
}

/// The `nvcuda.dll` subset. `cuMemcpy2D`/`cuCtxCreate`-family symbols are
/// loaded by their `_v2` export names — the plain names keep CUDA 2.x
/// semantics for ABI compatibility and are wrong to call today.
struct Cuda {
    init: unsafe extern "system" fn(u32) -> CuResult,
    device_get: unsafe extern "system" fn(*mut i32, i32) -> CuResult,
    primary_ctx_retain: unsafe extern "system" fn(*mut CuContext, i32) -> CuResult,
    primary_ctx_release: unsafe extern "system" fn(i32) -> CuResult,
    ctx_set_current: unsafe extern "system" fn(CuContext) -> CuResult,
    memcpy_2d: unsafe extern "system" fn(*const CudaMemcpy2d) -> CuResult,
    mem_host_alloc: unsafe extern "system" fn(*mut *mut c_void, usize, u32) -> CuResult,
    mem_free_host: unsafe extern "system" fn(*mut c_void) -> CuResult,
}

/// The `nvcuvid.dll` subset.
struct Cuvid {
    create_parser: unsafe extern "system" fn(*mut CuVideoParser, *mut ParserParams) -> CuResult,
    parse_video_data: unsafe extern "system" fn(CuVideoParser, *mut SourceDataPacket) -> CuResult,
    destroy_parser: unsafe extern "system" fn(CuVideoParser) -> CuResult,
    create_decoder:
        unsafe extern "system" fn(*mut CuVideoDecoder, *mut DecodeCreateInfo) -> CuResult,
    destroy_decoder: unsafe extern "system" fn(CuVideoDecoder) -> CuResult,
    decode_picture: unsafe extern "system" fn(CuVideoDecoder, *mut c_void) -> CuResult,
    map_video_frame: unsafe extern "system" fn(
        CuVideoDecoder,
        i32,
        *mut u64,
        *mut u32,
        *mut ProcParams,
    ) -> CuResult,
    unmap_video_frame: unsafe extern "system" fn(CuVideoDecoder, u64) -> CuResult,
}

struct Tables {
    cuda: Cuda,
    cuvid: Cuvid,
}

/// Load both driver DLLs once per process. `Err` = no NVIDIA driver — the
/// caller keeps its software rungs.
fn tables() -> Result<&'static Tables, String> {
    static T: std::sync::OnceLock<Result<&'static Tables, String>> = std::sync::OnceLock::new();
    T.get_or_init(|| unsafe {
        let cuda_dll = LoadLibraryA(PCSTR(c"nvcuda.dll".as_ptr() as *const u8))
            .map_err(|e| format!("nvcuda.dll not loadable: {e}"))?;
        let cuvid_dll = LoadLibraryA(PCSTR(c"nvcuvid.dll".as_ptr() as *const u8))
            .map_err(|e| format!("nvcuvid.dll not loadable: {e}"))?;
        let cuda = Cuda {
            init: load_fn!(cuda_dll, "cuInit", _),
            device_get: load_fn!(cuda_dll, "cuDeviceGet", _),
            primary_ctx_retain: load_fn!(cuda_dll, "cuDevicePrimaryCtxRetain", _),
            primary_ctx_release: load_fn!(cuda_dll, "cuDevicePrimaryCtxRelease_v2", _),
            ctx_set_current: load_fn!(cuda_dll, "cuCtxSetCurrent", _),
            memcpy_2d: load_fn!(cuda_dll, "cuMemcpy2D_v2", _),
            mem_host_alloc: load_fn!(cuda_dll, "cuMemHostAlloc", _),
            mem_free_host: load_fn!(cuda_dll, "cuMemFreeHost", _),
        };
        let cuvid = Cuvid {
            create_parser: load_fn!(cuvid_dll, "cuvidCreateVideoParser", _),
            parse_video_data: load_fn!(cuvid_dll, "cuvidParseVideoData", _),
            destroy_parser: load_fn!(cuvid_dll, "cuvidDestroyVideoParser", _),
            create_decoder: load_fn!(cuvid_dll, "cuvidCreateDecoder", _),
            destroy_decoder: load_fn!(cuvid_dll, "cuvidDestroyDecoder", _),
            decode_picture: load_fn!(cuvid_dll, "cuvidDecodePicture", _),
            map_video_frame: load_fn!(cuvid_dll, "cuvidMapVideoFrame64", _),
            unmap_video_frame: load_fn!(cuvid_dll, "cuvidUnmapVideoFrame64", _),
        };
        let status = (cuda.init)(0);
        if status != CUDA_SUCCESS {
            return Err(format!("cuInit: {status}"));
        }
        Ok(&*Box::leak(Box::new(Tables { cuda, cuvid })))
    })
    .clone()
}

// ---------------------------------------------------------------------------
// The decoder
// ---------------------------------------------------------------------------

/// One decoded picture: tightly-packed NV12 (`w`-byte rows, `h` luma rows
/// then `h/2` interleaved-chroma rows) plus the presentation time carried
/// through the parser.
pub struct NvFrame {
    pub width: u32,
    pub height: u32,
    pub nv12: Vec<u8>,
    pub ts_us: u64,
}

/// Callback-side state. Boxed so its address is stable for the parser's
/// `user_data`.
struct State {
    tables: &'static Tables,
    codec: NvCodec,
    decoder: CuVideoDecoder,
    decode_surfaces: u32,
    coded_width: u32,
    coded_height: u32,
    display_width: u32,
    display_height: u32,
    /// Display-order pictures ready after the current parse call:
    /// `(picture_index, timestamp)`.
    ready: Vec<(i32, i64)>,
    /// Page-locked staging for the device→host copy — DMA lands here at
    /// full PCIe rate (a pageable destination forces the driver through
    /// an internal bounce buffer), then one memcpy fills the caller's
    /// `Vec`. Allocated at sequence time, freed in `Drop`.
    pinned: *mut u8,
    pinned_len: usize,
    error: Option<String>,
}

/// An NVDEC HEVC session: Annex-B access units in, NV12 pictures out, in
/// display order, synchronously. `ulMaxDisplayDelay` is 0 — for the IPP
/// streams our encoder produces, every fed picture surfaces immediately.
pub struct NvdecHevc {
    tables: &'static Tables,
    ctx: CuContext,
    device: i32,
    parser: CuVideoParser,
    state: Box<State>,
}

// SAFETY: owned and driven by one route-decode thread (the same contract
// as the encoder's Send) — the Send bound exists for thread spawn, the
// value never migrates mid-stream.
unsafe impl Send for NvdecHevc {}

unsafe extern "system" fn on_sequence(user: *mut c_void, format: *mut VideoFormat) -> i32 {
    if user.is_null() {
        return 0;
    }
    let state = &mut *(user as *mut State);
    if format.is_null() {
        state.error = Some("NVDEC sequence callback returned no format".into());
        return 0;
    }
    let f = &*format;
    if f.codec != state.codec.id() {
        state.error = Some(format!(
            "NVDEC parser expected {} (codec {}) but announced codec {}",
            state.codec.name(),
            state.codec.id(),
            f.codec,
        ));
        return 0;
    }
    if f.chroma_format != CUDA_VIDEO_CHROMA_420
        || f.bit_depth_luma_minus8 != 0
        || f.bit_depth_chroma_minus8 != 0
        || f.progressive_sequence == 0
    {
        state.error = Some(format!(
            "unsupported stream shape (chroma {} · depth 8+{}/8+{} · progressive {})",
            f.chroma_format,
            f.bit_depth_luma_minus8,
            f.bit_depth_chroma_minus8,
            f.progressive_sequence,
        ));
        return 0;
    }
    let Some(display_w) = f.display_right.checked_sub(f.display_left) else {
        state.error = Some("NVDEC display rectangle overflow".into());
        return 0;
    };
    let Some(display_h) = f.display_bottom.checked_sub(f.display_top) else {
        state.error = Some("NVDEC display rectangle overflow".into());
        return 0;
    };
    let rect_fits = f.display_left >= 0
        && f.display_top >= 0
        && display_w >= 2
        && display_h >= 2
        && display_w % 2 == 0
        && display_h % 2 == 0
        && u32::try_from(f.display_right).is_ok_and(|v| v <= f.coded_width)
        && u32::try_from(f.display_bottom).is_ok_and(|v| v <= f.coded_height)
        && i16::try_from(f.display_left).is_ok()
        && i16::try_from(f.display_top).is_ok()
        && i16::try_from(f.display_right).is_ok()
        && i16::try_from(f.display_bottom).is_ok();
    if f.coded_width < 2 || f.coded_height < 2 || !rect_fits {
        state.error = Some(format!(
            "invalid NVDEC geometry {}x{} display [{},{},{},{}]",
            f.coded_width,
            f.coded_height,
            f.display_left,
            f.display_top,
            f.display_right,
            f.display_bottom,
        ));
        return 0;
    }
    let surfaces = u32::from(f.min_num_decode_surfaces).max(4);
    let display_w = display_w as u32;
    let display_h = display_h as u32;
    if !state.decoder.is_null() {
        // Mid-stream format change: only *fully* identical geometry can
        // reuse the decoder — coded AND display. Coded dims alone are a
        // trap: HEVC pads to CTB alignment, so a lane rebuild that moves
        // the display size within one alignment bucket (1280×718 ↔ ×720
        // both code 1280×768) would sail through a coded-only check and
        // keep the stale crop forever (red-team round 2, finding 1).
        // Failing loudly is the recovery: the bridge drops the session
        // and reopens fresh at the next key unit.
        if f.coded_width == state.coded_width
            && f.coded_height == state.coded_height
            && display_w == state.display_width
            && display_h == state.display_height
            && surfaces <= state.decode_surfaces
        {
            return state.decode_surfaces as i32;
        }
        state.error = Some("mid-stream geometry or decode-surface change".into());
        return 0;
    }
    state.coded_width = f.coded_width;
    state.coded_height = f.coded_height;
    state.display_width = display_w;
    state.display_height = display_h;
    let mut info = DecodeCreateInfo {
        width: f.coded_width,
        height: f.coded_height,
        num_decode_surfaces: surfaces,
        codec_type: state.codec.id(),
        chroma_format: CUDA_VIDEO_CHROMA_420,
        creation_flags: 0, // cudaVideoCreate_Default (CUVID path)
        bit_depth_minus8: 0,
        intra_decode_only: 0,
        max_width: f.coded_width,
        max_height: f.coded_height,
        reserved1: 0,
        display_area: [
            f.display_left as i16,
            f.display_top as i16,
            f.display_right as i16,
            f.display_bottom as i16,
        ],
        output_format: CUDA_VIDEO_SURFACE_NV12,
        deinterlace_mode: CUDA_VIDEO_DEINTERLACE_WEAVE,
        // Target = DISPLAY size, not coded: HEVC pads the coded frame up
        // to CTB alignment (720 → 736/768), and a coded-sized target makes
        // the map postprocessor *scale* the display rect up to fill it —
        // a subtle vertical stretch that cost this module its first
        // byte-exact round trip. Display-sized target + display_area =
        // 1:1 crop, no resample.
        target_width: state.display_width,
        target_height: state.display_height,
        num_output_surfaces: 2,
        vid_lock: std::ptr::null_mut(),
        target_rect: [0; 4],
        enable_histogram: 0,
        reserved2: [0; 4],
    };
    let status = (state.tables.cuvid.create_decoder)(&mut state.decoder, &mut info);
    if status != CUDA_SUCCESS || state.decoder.is_null() {
        state.decoder = std::ptr::null_mut();
        state.error = Some(if status == CUDA_SUCCESS {
            "cuvidCreateDecoder returned a null decoder".into()
        } else {
            format!("cuvidCreateDecoder: {status}")
        });
        return 0;
    }
    state.decode_surfaces = surfaces;
    let Some(need) = (state.display_width as usize)
        .checked_mul(state.display_height as usize)
        .and_then(|pixels| pixels.checked_mul(3))
        .map(|bytes| bytes / 2)
    else {
        state.error = Some("NVDEC sequence output size overflow".into());
        return 0;
    };
    let mut p: *mut c_void = std::ptr::null_mut();
    if (state.tables.cuda.mem_host_alloc)(&mut p, need, 0) == CUDA_SUCCESS {
        state.pinned = p as *mut u8;
        state.pinned_len = need;
    } else {
        // Pinned staging is an optimization, not a dependency — the copy
        // path falls back to the pageable destination.
        state.pinned = std::ptr::null_mut();
        state.pinned_len = 0;
    }
    let signal = f.video_signal_description;
    tracing::info!(
        "NVDEC {} sequence: coded {}x{} · display {}x{} at {},{} · surfaces {} · fps {}/{} · color full={} primaries={} transfer={} matrix={} · pinned-staging={}",
        state.codec.name(),
        f.coded_width,
        f.coded_height,
        state.display_width,
        state.display_height,
        f.display_left,
        f.display_top,
        surfaces,
        f.frame_rate_num,
        f.frame_rate_den,
        signal[0] & 0x08 != 0,
        signal[1],
        signal[2],
        signal[3],
        !state.pinned.is_null(),
    );
    surfaces as i32
}

unsafe extern "system" fn on_decode(user: *mut c_void, pic: *mut c_void) -> i32 {
    if user.is_null() {
        return 0;
    }
    let state = &mut *(user as *mut State);
    if pic.is_null() {
        state.error = Some("NVDEC decode callback returned no picture".into());
        return 0;
    }
    if state.decoder.is_null() {
        state
            .error
            .get_or_insert_with(|| "NVDEC decode callback has no decoder".into());
        return 0;
    }
    let status = (state.tables.cuvid.decode_picture)(state.decoder, pic);
    if status != CUDA_SUCCESS {
        state.error = Some(format!(
            "cuvidDecodePicture (idx {}): {status}",
            (*(pic as *const PicParamsPrefix)).curr_pic_idx
        ));
        return 0;
    }
    1
}

unsafe extern "system" fn on_display(user: *mut c_void, disp: *mut ParserDispInfo) -> i32 {
    if user.is_null() {
        return 0;
    }
    let state = &mut *(user as *mut State);
    if disp.is_null() {
        return 1; // EOS marker (only sent when asked for; be tolerant)
    }
    let d = &*disp;
    if d.picture_index < 0 {
        state.error = Some(format!(
            "NVDEC display callback returned invalid picture {}",
            d.picture_index
        ));
        return 0;
    }
    if state.ready.len() >= MAX_READY_PICTURES {
        state.error = Some(format!(
            "NVDEC produced more than {MAX_READY_PICTURES} pictures from one input"
        ));
        return 0;
    }
    state.ready.push((d.picture_index, d.timestamp));
    1
}

impl NvdecHevc {
    /// Open a session on the primary CUDA context of device 0. On a
    /// multi-GPU viewer the decode engine may not be the render GPU — a
    /// LUID-matched device pick is a follow-up; every field box today is
    /// single-GPU NVIDIA.
    pub fn open() -> Result<Self, String> {
        Self::open_codec(NvCodec::Hevc)
    }

    fn open_codec(codec: NvCodec) -> Result<Self, String> {
        let tables = tables()?;
        unsafe {
            let mut device = 0i32;
            let status = (tables.cuda.device_get)(&mut device, 0);
            if status != CUDA_SUCCESS {
                return Err(format!("cuDeviceGet: {status}"));
            }
            let mut ctx: CuContext = std::ptr::null_mut();
            let status = (tables.cuda.primary_ctx_retain)(&mut ctx, device);
            if status != CUDA_SUCCESS {
                return Err(format!("cuDevicePrimaryCtxRetain: {status}"));
            }
            if ctx.is_null() {
                let _ = (tables.cuda.primary_ctx_release)(device);
                return Err("cuDevicePrimaryCtxRetain returned a null context".into());
            }
            let status = (tables.cuda.ctx_set_current)(ctx);
            if status != CUDA_SUCCESS {
                let _ = (tables.cuda.primary_ctx_release)(device);
                return Err(format!("cuCtxSetCurrent: {status}"));
            }
            let mut state = Box::new(State {
                tables,
                codec,
                decoder: std::ptr::null_mut(),
                decode_surfaces: 0,
                coded_width: 0,
                coded_height: 0,
                display_width: 0,
                display_height: 0,
                ready: Vec::new(),
                pinned: std::ptr::null_mut(),
                pinned_len: 0,
                error: None,
            });
            let mut params: ParserParams = std::mem::zeroed();
            params.codec_type = codec.id();
            params.max_num_decode_surfaces = 20; // sequence callback overrides
            params.clock_rate = 1_000_000; // timestamps in µs
            params.error_threshold = 100;
            params.max_display_delay = 0; // low latency: display == decode order for IPP
            params.user_data = &mut *state as *mut State as *mut c_void;
            params.pfn_sequence_callback = Some(on_sequence);
            params.pfn_decode_picture = Some(on_decode);
            params.pfn_display_picture = Some(on_display);
            let mut parser: CuVideoParser = std::ptr::null_mut();
            let status = (tables.cuvid.create_parser)(&mut parser, &mut params);
            if status != CUDA_SUCCESS || parser.is_null() {
                if !parser.is_null() {
                    let _ = (tables.cuvid.destroy_parser)(parser);
                }
                let _ = (tables.cuda.ctx_set_current)(std::ptr::null_mut());
                let _ = (tables.cuda.primary_ctx_release)(device);
                return Err(if status == CUDA_SUCCESS {
                    "cuvidCreateVideoParser returned a null parser".into()
                } else {
                    format!("cuvidCreateVideoParser: {status}")
                });
            }
            Ok(Self {
                tables,
                ctx,
                device,
                parser,
                state,
            })
        }
    }

    pub fn label(&self) -> &'static str {
        "NVDEC (HEVC, hardware)"
    }

    /// Feed one Annex-B access unit; returns every picture that became
    /// display-ready (for our IPP streams: the AU's own picture, same
    /// call). An `Err` means the session is wedged — drop it and re-enter
    /// at the sender's next IDR, exactly the openh264 recovery shape.
    pub fn decode(&mut self, au: &[u8], ts_us: u64) -> Result<Vec<NvFrame>, String> {
        if au.is_empty() {
            return Ok(Vec::new());
        }
        let payload_size = u32::try_from(au.len())
            .map_err(|_| format!("NVDEC input too large ({} bytes)", au.len()))?;
        unsafe {
            if self.parser.is_null() {
                return Err("NVDEC parser is not available".into());
            }
            let status = (self.tables.cuda.ctx_set_current)(self.ctx);
            if status != CUDA_SUCCESS {
                return Err(format!("cuCtxSetCurrent: {status}"));
            }
            self.state.ready.clear();
            let mut packet = SourceDataPacket {
                flags: CUVID_PKT_TIMESTAMP | CUVID_PKT_ENDOFPICTURE,
                payload_size,
                payload: au.as_ptr(),
                timestamp: ts_us as i64,
            };
            let status = (self.tables.cuvid.parse_video_data)(self.parser, &mut packet);
            if let Some(e) = self.state.error.take() {
                return Err(e);
            }
            if status != CUDA_SUCCESS {
                return Err(format!("cuvidParseVideoData: {status}"));
            }
            let mut out = Vec::with_capacity(self.state.ready.len());
            let ready = std::mem::take(&mut self.state.ready);
            for (pic_idx, ts) in ready {
                out.push(self.map_out(pic_idx, ts)?);
            }
            Ok(out)
        }
    }

    /// Map one decoded surface and copy its NV12 down to host memory
    /// (tight `w`-byte rows).
    unsafe fn map_out(&mut self, pic_idx: i32, ts: i64) -> Result<NvFrame, String> {
        let (w, h) = (self.state.display_width, self.state.display_height);
        if self.state.decoder.is_null() || w < 2 || h < 2 || w % 2 != 0 || h % 2 != 0 {
            return Err(format!("NVDEC output state is invalid ({w}x{h})"));
        }
        let need = (w as usize)
            .checked_mul(h as usize)
            .and_then(|pixels| pixels.checked_mul(3))
            .map(|bytes| bytes / 2)
            .ok_or("NVDEC output size overflow")?;
        // The mapped surface is target-sized (display-sized — see the
        // create info): its chroma plane starts after `target_height`
        // luma rows.
        let surface_h = self.state.display_height as usize;
        let mut proc_params: ProcParams = std::mem::zeroed();
        proc_params.progressive_frame = 1;
        let (mut dptr, mut pitch) = (0u64, 0u32);
        let status = (self.tables.cuvid.map_video_frame)(
            self.state.decoder,
            pic_idx,
            &mut dptr,
            &mut pitch,
            &mut proc_params,
        );
        if status != CUDA_SUCCESS {
            return Err(format!("cuvidMapVideoFrame64: {status}"));
        }
        if dptr == 0 || pitch < w {
            let _ = (self.tables.cuvid.unmap_video_frame)(self.state.decoder, dptr);
            return Err(format!(
                "cuvidMapVideoFrame64 returned invalid surface (ptr {dptr:#x}, pitch {pitch}, width {w})"
            ));
        }
        let mut nv12 = vec![0u8; need];
        // DMA into the page-locked staging when we have it (full-rate
        // transfer), one memcpy out; else straight into the Vec.
        let staged = !self.state.pinned.is_null() && self.state.pinned_len >= need;
        let dst_base = if staged {
            self.state.pinned
        } else {
            nv12.as_mut_ptr()
        };
        let copy = |src_dev: u64, dst: *mut u8, rows: usize| -> CuResult {
            let desc = CudaMemcpy2d {
                src_x_in_bytes: 0,
                src_y: 0,
                src_memory_type: CU_MEMORYTYPE_DEVICE,
                src_host: std::ptr::null(),
                src_device: src_dev,
                src_array: std::ptr::null_mut(),
                src_pitch: pitch as usize,
                dst_x_in_bytes: 0,
                dst_y: 0,
                dst_memory_type: CU_MEMORYTYPE_HOST,
                dst_host: dst as *mut c_void,
                dst_device: 0,
                dst_array: std::ptr::null_mut(),
                dst_pitch: w as usize,
                width_in_bytes: w as usize,
                height: rows,
            };
            (self.tables.cuda.memcpy_2d)(&desc)
        };
        // NV12 in the mapped surface: luma plane, then interleaved chroma
        // at `pitch * target_height` (the create-info target height).
        let luma = copy(dptr, dst_base, h as usize);
        let chroma = copy(
            dptr + (pitch as u64) * (surface_h as u64),
            unsafe { dst_base.add((w as usize) * (h as usize)) },
            (h as usize) / 2,
        );
        let status = (self.tables.cuvid.unmap_video_frame)(self.state.decoder, dptr);
        if luma != CUDA_SUCCESS || chroma != CUDA_SUCCESS {
            return Err(format!("cuMemcpy2D: {luma}/{chroma}"));
        }
        if status != CUDA_SUCCESS {
            return Err(format!("cuvidUnmapVideoFrame64: {status}"));
        }
        if staged {
            unsafe {
                std::ptr::copy_nonoverlapping(self.state.pinned, nv12.as_mut_ptr(), need);
            }
        }
        Ok(NvFrame {
            width: w,
            height: h,
            nv12,
            ts_us: ts as u64,
        })
    }
}

impl Drop for NvdecHevc {
    fn drop(&mut self) {
        unsafe {
            (self.tables.cuda.ctx_set_current)(self.ctx);
            if !self.parser.is_null() {
                let _ = (self.tables.cuvid.destroy_parser)(self.parser);
            }
            if !self.state.decoder.is_null() {
                let _ = (self.tables.cuvid.destroy_decoder)(self.state.decoder);
            }
            if !self.state.pinned.is_null() {
                let _ = (self.tables.cuda.mem_free_host)(self.state.pinned as *mut c_void);
            }
            let _ = (self.tables.cuda.ctx_set_current)(std::ptr::null_mut());
            let _ = (self.tables.cuda.primary_ctx_release)(self.device);
        }
    }
}

/// An NVDEC H.264 session. It deliberately wraps the same parser/decoder
/// implementation as HEVC: nvcuvid parameterizes both objects by codec ID,
/// while AU framing, display callbacks, NV12 mapping, and recovery are
/// identical for the progressive 8-bit 4:2:0 streams this pipeline emits.
pub struct NvdecH264(NvdecHevc);

impl NvdecH264 {
    pub fn open() -> Result<Self, String> {
        NvdecHevc::open_codec(NvCodec::H264).map(Self)
    }

    pub fn label(&self) -> &'static str {
        "NVDEC (H.264, hardware)"
    }

    pub fn decode(&mut self, au: &[u8], ts_us: u64) -> Result<Vec<NvFrame>, String> {
        self.0.decode(au, ts_us)
    }
}

/// NVDEC AV1 decode — **STUB**. The seam for the AV1 arc: it will mirror
/// [`NvdecHevc`] almost exactly (nvcuvid's parser/decoder path
/// parameterizes on codec, so most of this file is reused with
/// [`CUDA_VIDEO_CODEC_AV1`]), with two AV1-specific differences the
/// implementer should know: the parser's `flags` bit `bAnnexb` selects
/// the temporal-unit framing AV1 uses (no start codes — see
/// `sniff_av1_obu` in `video_decode.rs`), and AV1's film-grain synthesis
/// is applied at map time. Same `decode(au, ts_us) -> Vec<NvFrame>` seam
/// as the HEVC twin so the bridge treats it identically. `open` reports
/// the honest not-yet so the ladder falls soft.
pub struct NvdecAv1 {
    // Fields land with the implementation (parser + decoder + State,
    // like NvdecHevc). Empty stub keeps the type real for the dispatch.
    _priv: (),
}

// SAFETY: will be owned+driven by one route-decode thread like NvdecHevc.
unsafe impl Send for NvdecAv1 {}

impl NvdecAv1 {
    pub fn open() -> Result<Self, String> {
        Err("NVDEC AV1 decode not yet implemented".into())
    }

    pub fn label(&self) -> &'static str {
        "NVDEC (AV1, hardware) [stub]"
    }

    pub fn decode(&mut self, _au: &[u8], _ts_us: u64) -> Result<Vec<NvFrame>, String> {
        Err("NVDEC AV1 decode not yet implemented".into())
    }
}

/// NV12 → RGBA, BT.709 limited range — the colorimetry the capture lane's
/// VideoProcessor conversion feeds the encoder for HD content.
///
/// Shaped for the decode thread's frame budget: each 2×2 quad shares one
/// (U,V) pair, so the three chroma terms are computed once and applied to
/// four luma samples, and every row is a bounds-checked-once slice so the
/// hot loop stays branch-free. `w` and `h` must be even (NV12 guarantees
/// it; the decoder's display crop keeps it so).
pub fn nv12_to_rgba(nv12: &[u8], w: usize, h: usize, out: &mut [u8]) {
    // Defensive: every current caller sizes `out`/`nv12` from the same
    // `w`×`h`, but `panic = "abort"` turns any mismatch (an odd dimension,
    // a future caller) into a whole-node kill via the `split_at`/slice
    // panics below. A bad frame should be dropped, not fatal.
    if w == 0 || h == 0 || nv12.len() < w * h * 3 / 2 || out.len() < w * h * 4 {
        return;
    }
    let (luma, chroma) = nv12.split_at(w * h);
    let quads = h / 2;
    // Band the work across a few scoped threads at real frame sizes —
    // identical output to the single-thread path (bands split at chroma
    // rows), ~3–4× on the wall clock for the cost of four short-lived
    // spawns per frame. Small frames stay single-threaded.
    let workers = if quads >= 128 { 4 } else { 1 };
    if workers == 1 {
        band_dispatch(luma, chroma, w, 0..quads, out);
        return;
    }
    let per = quads.div_ceil(workers);
    std::thread::scope(|s| {
        let mut rest = out;
        for k in 0..workers {
            let range = (k * per).min(quads)..((k + 1) * per).min(quads);
            if range.is_empty() {
                break;
            }
            let bytes = (range.end - range.start) * 2 * w * 4;
            let (mine, tail) = rest.split_at_mut(bytes);
            rest = tail;
            s.spawn(move || band_dispatch(luma, chroma, w, range, mine));
        }
    });
}

#[inline(always)]
fn px(y: u8, tr: i32, tg: i32, tb: i32, o: &mut [u8]) {
    let yy = 298 * (i32::from(y) - 16);
    o[0] = ((yy + tr) >> 8).clamp(0, 255) as u8;
    o[1] = ((yy + tg) >> 8).clamp(0, 255) as u8;
    o[2] = ((yy + tb) >> 8).clamp(0, 255) as u8;
    o[3] = 255;
}

/// The scalar reference lane — the definition of correct. The AVX2 lane
/// below performs the *identical* integer operations eight pixels at a
/// time, so its output is byte-equal by construction (and pinned by the
/// `simd_lane_matches_scalar` test, which is the real guarantee).
fn band_scalar(
    luma: &[u8],
    chroma: &[u8],
    w: usize,
    yp_range: std::ops::Range<usize>,
    out: &mut [u8],
) {
    let yp0 = yp_range.start;
    for yp in yp_range {
        let crow = &chroma[yp * w..][..w];
        let l0 = &luma[(yp * 2) * w..][..w];
        let l1 = &luma[(yp * 2 + 1) * w..][..w];
        let (o0, rest) = out[(yp - yp0) * 2 * w * 4..].split_at_mut(w * 4);
        let o1 = &mut rest[..w * 4];
        for xp in 0..w / 2 {
            let u = i32::from(crow[xp * 2]) - 128;
            let v = i32::from(crow[xp * 2 + 1]) - 128;
            let (tr, tg, tb) = (459 * v, -55 * u - 136 * v, 541 * u);
            let x0 = xp * 8;
            px(l0[xp * 2], tr, tg, tb, &mut o0[x0..x0 + 4]);
            px(l0[xp * 2 + 1], tr, tg, tb, &mut o0[x0 + 4..x0 + 8]);
            px(l1[xp * 2], tr, tg, tb, &mut o1[x0..x0 + 4]);
            px(l1[xp * 2 + 1], tr, tg, tb, &mut o1[x0 + 4..x0 + 8]);
        }
    }
}

/// Pick the widest lane the CPU carries. Detection is one cached load;
/// narrow frames stay scalar (the vector body wants ≥8 luma columns).
fn band_dispatch(
    luma: &[u8],
    chroma: &[u8],
    w: usize,
    yp_range: std::ops::Range<usize>,
    out: &mut [u8],
) {
    #[cfg(target_arch = "x86_64")]
    {
        static AVX2: std::sync::LazyLock<bool> =
            std::sync::LazyLock::new(|| std::arch::is_x86_feature_detected!("avx2"));
        if *AVX2 && w >= 8 {
            // SAFETY: feature presence just checked; the fn upholds the
            // same slice contracts as the scalar lane.
            unsafe { band_avx2(luma, chroma, w, yp_range, out) };
            return;
        }
    }
    band_scalar(luma, chroma, w, yp_range, out);
}

/// The AVX2 lane: eight pixels per step across two luma rows, exactly
/// the scalar math on i32 lanes — widen, multiply, add, arithmetic
/// shift right 8, clamp 0..255 (`max`/`min`), pack `R | G<<8 | B<<16 |
/// A<<24` and store 32 bytes. `vpermd` duplicates each (U,V) across its
/// 2×2 quad, mirroring the scalar quad sharing. Any width remainder
/// (<8 columns) runs the scalar quad — same `px`, same bytes.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn band_avx2(
    luma: &[u8],
    chroma: &[u8],
    w: usize,
    yp_range: std::ops::Range<usize>,
    out: &mut [u8],
) {
    use std::arch::x86_64::*;
    // One macro, not a closure: closures don't inherit `target_feature`,
    // and an un-featured closure body would demote every intrinsic call.
    macro_rules! clamp_term {
        ($yy:expr, $t:expr, $zero:expr, $c255:expr) => {
            _mm256_min_epi32(
                $c255,
                _mm256_max_epi32($zero, _mm256_srai_epi32::<8>(_mm256_add_epi32($yy, $t))),
            )
        };
    }
    let yp0 = yp_range.start;
    let pairs = w / 2;
    let vec_pairs = pairs & !3; // 4 chroma pairs = 8 luma columns per step
                                // Non-temporal stores when alignment allows: the RGBA output is
                                // ~3× the input and write-allocate (RFO) doubles its DRAM traffic —
                                // at 1440p the kernel is memory-bound, so bypassing the cache on
                                // the store side is worth more than any more arithmetic. 16-byte
                                // alignment holds when the band's base is 16-aligned and the row
                                // stride keeps it (w % 4 == 0 — every real frame); otherwise the
                                // unaligned store path stands.
    let streamable = (out.as_ptr() as usize) & 15 == 0 && w.is_multiple_of(4);
    let idx_u = _mm256_setr_epi32(0, 0, 2, 2, 4, 4, 6, 6);
    let idx_v = _mm256_setr_epi32(1, 1, 3, 3, 5, 5, 7, 7);
    let c16 = _mm256_set1_epi32(16);
    let c128 = _mm256_set1_epi32(128);
    let c298 = _mm256_set1_epi32(298);
    let c459 = _mm256_set1_epi32(459);
    let cm55 = _mm256_set1_epi32(-55);
    let cm136 = _mm256_set1_epi32(-136);
    let c541 = _mm256_set1_epi32(541);
    let c255 = _mm256_set1_epi32(255);
    let zero = _mm256_setzero_si256();
    let alpha = _mm256_set1_epi32(0xFF00_0000u32 as i32);
    for yp in yp_range {
        let crow = &chroma[yp * w..][..w];
        let l0 = &luma[(yp * 2) * w..][..w];
        let l1 = &luma[(yp * 2 + 1) * w..][..w];
        let (o0, rest) = out[(yp - yp0) * 2 * w * 4..].split_at_mut(w * 4);
        let o1 = &mut rest[..w * 4];
        let mut xp = 0usize;
        while xp < vec_pairs {
            let x = xp * 2;
            let cbytes = _mm_loadl_epi64(crow.as_ptr().add(x) as *const __m128i);
            let c = _mm256_cvtepu8_epi32(cbytes);
            let u = _mm256_sub_epi32(_mm256_permutevar8x32_epi32(c, idx_u), c128);
            let v = _mm256_sub_epi32(_mm256_permutevar8x32_epi32(c, idx_v), c128);
            let tr = _mm256_mullo_epi32(c459, v);
            let tg = _mm256_add_epi32(_mm256_mullo_epi32(cm55, u), _mm256_mullo_epi32(cm136, v));
            let tb = _mm256_mullo_epi32(c541, u);

            let y8 = _mm_loadl_epi64(l0.as_ptr().add(x) as *const __m128i);
            let yy = _mm256_mullo_epi32(c298, _mm256_sub_epi32(_mm256_cvtepu8_epi32(y8), c16));
            let r = clamp_term!(yy, tr, zero, c255);
            let g = clamp_term!(yy, tg, zero, c255);
            let b = clamp_term!(yy, tb, zero, c255);
            let px0 = _mm256_or_si256(
                _mm256_or_si256(r, _mm256_slli_epi32::<8>(g)),
                _mm256_or_si256(_mm256_slli_epi32::<16>(b), alpha),
            );
            let d0 = o0.as_mut_ptr().add(x * 4);
            if streamable {
                _mm_stream_si128(d0 as *mut __m128i, _mm256_castsi256_si128(px0));
                _mm_stream_si128(
                    d0.add(16) as *mut __m128i,
                    _mm256_extracti128_si256::<1>(px0),
                );
            } else {
                _mm256_storeu_si256(d0 as *mut __m256i, px0);
            }

            let y8 = _mm_loadl_epi64(l1.as_ptr().add(x) as *const __m128i);
            let yy = _mm256_mullo_epi32(c298, _mm256_sub_epi32(_mm256_cvtepu8_epi32(y8), c16));
            let r = clamp_term!(yy, tr, zero, c255);
            let g = clamp_term!(yy, tg, zero, c255);
            let b = clamp_term!(yy, tb, zero, c255);
            let px1 = _mm256_or_si256(
                _mm256_or_si256(r, _mm256_slli_epi32::<8>(g)),
                _mm256_or_si256(_mm256_slli_epi32::<16>(b), alpha),
            );
            let d1 = o1.as_mut_ptr().add(x * 4);
            if streamable {
                _mm_stream_si128(d1 as *mut __m128i, _mm256_castsi256_si128(px1));
                _mm_stream_si128(
                    d1.add(16) as *mut __m128i,
                    _mm256_extracti128_si256::<1>(px1),
                );
            } else {
                _mm256_storeu_si256(d1 as *mut __m256i, px1);
            }
            xp += 4;
        }
        for xp in vec_pairs..pairs {
            let u = i32::from(crow[xp * 2]) - 128;
            let v = i32::from(crow[xp * 2 + 1]) - 128;
            let (tr, tg, tb) = (459 * v, -55 * u - 136 * v, 541 * u);
            let x0 = xp * 8;
            px(l0[xp * 2], tr, tg, tb, &mut o0[x0..x0 + 4]);
            px(l0[xp * 2 + 1], tr, tg, tb, &mut o0[x0 + 4..x0 + 8]);
            px(l1[xp * 2], tr, tg, tb, &mut o1[x0..x0 + 4]);
            px(l1[xp * 2 + 1], tr, tg, tb, &mut o1[x0 + 4..x0 + 8]);
        }
    }
    if streamable {
        // Drain the write-combining buffers before anyone reads the
        // frame — non-temporal stores are weakly ordered.
        _mm_sfence();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SIMD lane's whole contract: byte-identical output to the
    /// scalar reference on every size class — vector-width multiples,
    /// tail widths, tiny frames, and the full value range (the LCG walks
    /// well beyond video-legal Y/U/V, so the clamps are exercised on
    /// both rails). Skips silently only where the CPU has no AVX2.
    #[test]
    fn simd_lane_matches_scalar_byte_for_byte() {
        #[cfg(target_arch = "x86_64")]
        {
            if !std::arch::is_x86_feature_detected!("avx2") {
                eprintln!("SKIP: no AVX2 on this CPU");
                return;
            }
            let mut seed = 0x1234_5678u32;
            let mut rng = move || {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 24) as u8
            };
            // Widths: multiple-of-8, tail-of-2/4/6 columns, minimum lane.
            for (w, h) in [(64, 16), (100, 8), (102, 6), (1280, 24), (8, 2), (12, 4)] {
                let nv12: Vec<u8> = (0..w * h * 3 / 2).map(|_| rng()).collect();
                let mut scalar = vec![0u8; w * h * 4];
                let mut simd = vec![0u8; w * h * 4];
                let (luma, chroma) = nv12.split_at(w * h);
                band_scalar(luma, chroma, w, 0..h / 2, &mut scalar);
                unsafe { band_avx2(luma, chroma, w, 0..h / 2, &mut simd) };
                assert_eq!(scalar, simd, "lane divergence at {w}×{h}");
            }
            // And through the public entry (threading + dispatch).
            let (w, h) = (1440, 720);
            let nv12: Vec<u8> = (0..w * h * 3 / 2).map(|_| rng()).collect();
            let mut via_public = vec![0u8; w * h * 4];
            nv12_to_rgba(&nv12, w, h, &mut via_public);
            let mut reference = vec![0u8; w * h * 4];
            let (luma, chroma) = nv12.split_at(w * h);
            band_scalar(luma, chroma, w, 0..h / 2, &mut reference);
            assert_eq!(reference, via_public, "threaded dispatch divergence");
        }
    }

    /// Read the exact NV12 bytes of a GPU-lane texture back to the CPU
    /// via a staging copy — the encoder's literal input, for byte-exact
    /// comparison against the decoder's output.
    unsafe fn readback_nv12(
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        tex: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        w: usize,
        h: usize,
    ) -> Vec<u8> {
        use windows::Win32::Graphics::Direct3D11::*;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        tex.GetDesc(&mut desc);
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut staging))
            .expect("staging");
        let staging = staging.unwrap();
        let ctx = device.GetImmediateContext().expect("ctx");
        ctx.CopyResource(&staging, tex);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .expect("map");
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData as *const u8;
        let mut out = vec![0u8; w * h * 3 / 2];
        for row in 0..h {
            std::ptr::copy_nonoverlapping(base.add(row * pitch), out.as_mut_ptr().add(row * w), w);
        }
        let chroma_base = base.add(pitch * h);
        for row in 0..h / 2 {
            std::ptr::copy_nonoverlapping(
                chroma_base.add(row * pitch),
                out.as_mut_ptr().add(w * h + row * w),
                w,
            );
        }
        ctx.Unmap(&staging, 0);
        out
    }

    /// Ordinary H.264 through the exact receive contract: the sender may split
    /// at slice NALs, but the bridge reassembles same-timestamp chunks before
    /// setting NVDEC's END_OF_PICTURE. The reconstructed AU must remain byte-
    /// exact and decode to the same pictures as the original.
    #[test]
    fn nvdec_h264_whole_and_reassembled_paced_au_match() {
        use openh264::encoder::{
            BitRate, Encoder, EncoderConfig, FrameRate, RateControlMode, UsageType,
        };
        use openh264::formats::{RgbSliceU8, YUVBuffer};

        let (w, h) = (640usize, 480usize);
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(10_000_000))
            .max_frame_rate(FrameRate::from_hz(60.0))
            .max_slice_len(4 * 1024);
        let mut enc = match Encoder::with_api_config(openh264::OpenH264API::from_source(), config) {
            Ok(enc) => enc,
            Err(e) => {
                eprintln!("SKIP: OpenH264 encoder unavailable: {e}");
                return;
            }
        };
        let mut whole = match NvdecH264::open() {
            Ok(dec) => dec,
            Err(e) => {
                eprintln!("SKIP: H.264 NVDEC unavailable: {e}");
                return;
            }
        };
        let mut paced = NvdecH264::open().expect("second H.264 NVDEC session");
        let mut rgb = vec![0u8; w * h * 3];
        let mut whole_out = Vec::new();
        let mut paced_out = Vec::new();
        let mut saw_split = false;

        for frame in 0..12u64 {
            let (pixels, _) = rgb.as_chunks_mut::<3>();
            for (p, px) in pixels.iter_mut().enumerate() {
                let x = p % w;
                let y = p / w;
                let tile = (((x / 8) + (y / 8) + frame as usize) & 1) as u8;
                px[0] = if tile == 0 { 24 } else { 228 };
                px[1] = ((x + frame as usize * 7) & 255) as u8;
                px[2] = ((y * 3 + frame as usize * 11) & 255) as u8;
            }
            let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&rgb, (w, h)));
            let au = enc.encode(&yuv).expect("H.264 encode").to_vec();
            if au.is_empty() {
                continue;
            }
            let ts = frame * 16_667;
            whole_out.extend(whole.decode(&au, ts).expect("whole-AU NVDEC"));
            let chunks = crate::video::split_annexb_paced(&au, 4 * 1024);
            saw_split |= chunks.len() > 1;
            let mut rebuilt = Vec::with_capacity(au.len());
            for chunk in chunks {
                rebuilt.extend_from_slice(&au[chunk]);
            }
            assert_eq!(rebuilt, au, "paced chunk coalescing is byte-exact");
            paced_out.extend(paced.decode(&rebuilt, ts).expect("coalesced H.264 NVDEC"));
        }

        assert!(saw_split, "encoder produced a real multi-slice access unit");
        assert_eq!(
            paced_out.len(),
            whole_out.len(),
            "one picture per access unit"
        );
        assert!(!whole_out.is_empty(), "at least one H.264 picture decoded");
        for (paced, whole) in paced_out.iter().zip(&whole_out) {
            assert_eq!((paced.width, paced.height), (w as u32, h as u32));
            assert_eq!(paced.ts_us, whole.ts_us, "timestamp survives chunk feeds");
            assert_eq!(paced.nv12, whole.nv12, "paced decode is byte-identical");
        }
    }

    /// The production NVIDIA path at both field resolutions: D3D11 capture
    /// texture -> NV12 VideoProcessor -> NVENC H.264 -> NVDEC. This proves the
    /// driver encoder's actual profile/VUI/slice output, not only OpenH264's
    /// convenient test stream.
    #[test]
    fn nvenc_h264_nvdec_round_trip_1080p_and_1440p() {
        for (w, h) in [(1920u32, 1080u32), (2560u32, 1440u32)] {
            let (wu, hu) = (w as usize, h as usize);
            let mut gpu = match crate::gpu_pipeline::GpuConvert::new(w, h, w, h) {
                Ok(gpu) => gpu,
                Err(e) => {
                    eprintln!("SKIP: GPU convert unavailable at {w}x{h}: {e}");
                    return;
                }
            };
            let mut enc = match crate::nvenc::NvencH264::open_on_device(
                &gpu.device(),
                w,
                h,
                60,
                30_000_000,
                false,
                false,
            ) {
                Ok(enc) => enc,
                Err(e) => {
                    eprintln!("SKIP: NVENC H.264 unavailable at {w}x{h}: {e}");
                    return;
                }
            };
            let mut dec = NvdecH264::open().expect("H.264 NVDEC after NVENC opened");
            let mut doc = vec![0u8; wu * (hu + 96) * 4];
            crate::nvenc::tests_support::paint_document(&mut doc, wu, hu + 96);
            let mut bgra = vec![0u8; wu * hu * 4];
            let tex = gpu.bgra_texture_from(&bgra, w, h).expect("BGRA texture");
            let mut decoded = 0u32;

            for frame in 0..10u64 {
                let offset = frame as usize * 4;
                bgra.copy_from_slice(&doc[offset * wu * 4..][..wu * hu * 4]);
                gpu.update_bgra(&tex, &bgra, w, h);
                let (slot, nv12) = gpu.convert(&tex).expect("convert").expect("NV12 slot");
                let out = enc
                    .encode_texture(&nv12, frame == 0)
                    .expect("NVENC H.264 encode");
                gpu.release(slot);
                for (au, _) in out.units {
                    for picture in dec.decode(&au, frame * 16_667).expect("NVDEC H.264 decode") {
                        assert_eq!((picture.width, picture.height), (w, h));
                        assert_eq!(picture.nv12.len(), wu * hu * 3 / 2);
                        decoded += 1;
                    }
                }
            }
            assert!(decoded >= 9, "{w}x{h}: decoded {decoded}/10 NVENC frames");
        }
    }

    /// The whole Studio·Lossless media plane in one test, both hardware
    /// engines: paint → GPU convert → **NVENC HEVC lossless** → Annex-B →
    /// **NVDEC** → NV12, asserting the decoder's output is *byte-exact*
    /// against the encoder's input for every frame. This is the claim
    /// "lossless" makes, proven end to end on the real silicon.
    #[test]
    fn nvdec_hevc_lossless_round_trip() {
        let (w, h) = (1280u32, 720u32);
        let (wu, hu) = (w as usize, h as usize);
        let mut gpu = match crate::gpu_pipeline::GpuConvert::new(w, h, w, h) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("SKIP: GPU convert unavailable: {e}");
                return;
            }
        };
        let mut enc =
            match crate::nvenc::NvencH264::open_lossless_hevc_on_device(&gpu.device(), w, h, 60) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("SKIP: NVENC HEVC session unavailable: {e}");
                    return;
                }
            };
        let mut dec = match NvdecHevc::open() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("SKIP: NVDEC unavailable: {e}");
                return;
            }
        };
        let mut doc = vec![0u8; wu * (hu + 300) * 4];
        crate::nvenc::tests_support::paint_document(&mut doc, wu, hu + 300);
        let mut bgra = vec![0u8; wu * hu * 4];
        let tex = gpu.bgra_texture_from(&bgra, w, h).expect("tex");
        let (mut decoded, mut exact) = (0u32, 0u32);
        let mut dec_ms: Vec<f32> = Vec::new();
        for i in 0..60u64 {
            let off = (i as usize) * 3;
            bgra.copy_from_slice(&doc[off * wu * 4..][..wu * hu * 4]);
            gpu.update_bgra(&tex, &bgra, w, h);
            let (slot, nv12_tex) = gpu.convert(&tex).expect("convert").expect("slot");
            let reference = unsafe { readback_nv12(&gpu.device(), &nv12_tex, wu, hu) };
            let out = enc.encode_texture(&nv12_tex, i == 0).expect("encode");
            gpu.release(slot);
            for (au, _) in &out.units {
                let t = std::time::Instant::now();
                let frames = dec.decode(au, i * 16_667).expect("decode");
                dec_ms.push(t.elapsed().as_secs_f32() * 1000.0);
                for f in frames {
                    assert_eq!((f.width, f.height), (w, h), "decoded geometry");
                    decoded += 1;
                    if f.nv12 == reference {
                        exact += 1;
                    } else {
                        // Split the mismatch by plane and magnitude — ±1
                        // luma everywhere means "not actually lossless"
                        // (transform rounding); a wrecked chroma plane
                        // means the map copy's offsets are wrong.
                        let plane = wu * hu;
                        let stat = |a: &[u8], b: &[u8]| {
                            let (mut n, mut max) = (0usize, 0i32);
                            for (x, y) in a.iter().zip(b) {
                                let d = (i32::from(*x) - i32::from(*y)).abs();
                                if d != 0 {
                                    n += 1;
                                    max = max.max(d);
                                }
                            }
                            (n, max)
                        };
                        let (ln, lmax) = stat(&f.nv12[..plane], &reference[..plane]);
                        let (cn, cmax) = stat(&f.nv12[plane..], &reference[plane..]);
                        // Shift search: if decoded == reference displaced
                        // by (dx,dy), the mismatch is geometry, not
                        // fidelity — and the offset names the culprit.
                        let (mut best, mut best_at) = (usize::MAX, (0i32, 0i32));
                        for dy in -6i32..=6 {
                            for dx in -6i32..=6 {
                                let mut n = 0usize;
                                for y in (8..hu - 8).step_by(3) {
                                    let sy = (y as i32 + dy) as usize;
                                    for x in 8..wu - 8 {
                                        let sx = (x as i32 + dx) as usize;
                                        if f.nv12[y * wu + x] != reference[sy * wu + sx] {
                                            n += 1;
                                        }
                                    }
                                }
                                if n < best {
                                    (best, best_at) = (n, (dx, dy));
                                }
                            }
                        }
                        // The most telling datum: which (reference →
                        // decoded) value pairs dominate the mismatch.
                        let mut pairs: std::collections::HashMap<(u8, u8), usize> =
                            std::collections::HashMap::new();
                        for (d, r) in f.nv12[..plane].iter().zip(&reference[..plane]) {
                            if d != r {
                                *pairs.entry((*r, *d)).or_default() += 1;
                            }
                        }
                        let mut top: Vec<_> = pairs.into_iter().collect();
                        top.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
                        let top: Vec<String> = top
                            .iter()
                            .take(8)
                            .map(|((r, d), n)| format!("{r}→{d}×{n}"))
                            .collect();
                        panic!(
                            "frame {i}: luma {ln}/{plane} differ (max Δ{lmax}) · chroma {cn}/{} differ (max Δ{cmax}) · best shift (dx {}, dy {}) leaves {best} diffs · top ref→dec: {}",
                            plane / 2, best_at.0, best_at.1, top.join(" ")
                        );
                    }
                }
            }
        }
        assert_eq!(decoded, 60, "a picture out for every frame in");
        assert_eq!(exact, 60, "every picture byte-exact");
        dec_ms.sort_by(f32::total_cmp);
        let n = dec_ms.len();
        println!(
            "NVDEC HEVC lossless round-trip: {decoded}/60 byte-exact · decode+copy avg {:.2} ms · p95 {:.2} · max {:.2}",
            dec_ms.iter().sum::<f32>() / n as f32,
            dec_ms[(n * 95 / 100).min(n - 1)],
            dec_ms[n - 1],
        );
    }
}
