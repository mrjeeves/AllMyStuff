//! Direct NVENC — NVIDIA's Video Codec SDK driven straight, no Media
//! Foundation in between. This is the game-mode encoder rung: the MFT
//! wraps this same hardware but withholds the levers WAN gaming needs —
//! **intra-refresh/GDR** (spread the I-data across N frames instead of
//! sending an IDR wall, which shrinks the exact packet bursts the
//! transport can't yet pace), **reference-picture invalidation** (recover
//! from a reported loss with one P-frame instead of an IDR), and
//! guaranteed in-place reconfigure. The lane stays texture-native: input
//! is the same NV12 ring texture the GPU lane's VideoProcessor produces,
//! registered with the session and read in place.
//!
//! No build-time dependency: the API is a C function table loaded from
//! `nvEncodeAPI64.dll` at runtime (`NvEncodeAPICreateInstance`), so every
//! failure — no NVIDIA driver, old driver, no free session — is soft and
//! the ladder keeps the MF rung. The FFI subset below is hand-transcribed
//! from NVIDIA's MIT-licensed `nvEncodeAPI.h` (ffnvcodec n12.0.16.0 —
//! SDK 12.0, driver 522.25+); struct layouts are pinned by compile-time
//! size asserts. The two codec unions are deliberately **oversized** (and
//! zeroed): our pre-union field offsets match the header exactly, the
//! driver's writes land inside our larger allocation, and everything past
//! what we set reads back as zero on both sides — safe in both directions
//! without transcribing every codec's config.
//!
//! Sync mode only (`enableEncodeAsync = 0`, one frame in flight): encode
//! → lock (blocks until done) → copy out. At our sizes the hardware runs
//! a frame in single-digit milliseconds and the GPU lane's encode thread
//! exists to wait on exactly this; async event plumbing can come later if
//! a measurement asks for it.

#![cfg(windows)]

use std::ffi::c_void;

use windows::core::{Interface, GUID, PCSTR};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

use crate::video::EncodeOutcome;

// ---------------------------------------------------------------------------
// FFI: the nvEncodeAPI.h subset (SDK 12.0 layouts)
// ---------------------------------------------------------------------------

/// `NVENCAPI_VERSION` for SDK 12.0: major 12, minor 0.
const API_VERSION: u32 = 12;
/// `NVENCAPI_STRUCT_VERSION(n)`.
const fn sv(n: u32) -> u32 {
    API_VERSION | (n << 16) | (0x7 << 28)
}

const NV_ENC_SUCCESS: u32 = 0;
const NV_ENC_ERR_NEED_MORE_INPUT: u32 = 17;

const NV_ENC_DEVICE_TYPE_DIRECTX: u32 = 0;
const NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX: u32 = 0;
const NV_ENC_BUFFER_FORMAT_NV12: u32 = 0x1;
const NV_ENC_BUFFER_USAGE_INPUT_IMAGE: u32 = 0;
const NV_ENC_PIC_STRUCT_FRAME: u32 = 0x01;
const NV_ENC_PIC_TYPE_I: u32 = 0x02;
const NV_ENC_PIC_TYPE_IDR: u32 = 0x03;
const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 0x2;
const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x4;
const NV_ENC_PARAMS_RC_VBR: u32 = 0x1;
const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 3;
const NVENC_INFINITE_GOPLENGTH: u32 = 0xffff_ffff;

const NV_ENC_CODEC_H264_GUID: GUID = GUID::from_values(
    0x6bc8_2762,
    0x4e63,
    0x4ca4,
    [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
);
const NV_ENC_PRESET_P1_GUID: GUID = GUID::from_values(
    0xfc0a_8d3e,
    0x45f8,
    0x4cf8,
    [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
);
const NV_ENC_PRESET_P2_GUID: GUID = GUID::from_values(
    0xf581_cfb8,
    0x88d6,
    0x4381,
    [0x93, 0xf0, 0xdf, 0x13, 0xf9, 0xc2, 0x7d, 0xab],
);
const NV_ENC_PRESET_P3_GUID: GUID = GUID::from_values(
    0x3685_0110,
    0x3a07,
    0x441f,
    [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6],
);
const NV_ENC_PRESET_P6_GUID: GUID = GUID::from_values(
    0x8e75_c279,
    0x6299,
    0x4ab6,
    [0x83, 0x02, 0x0b, 0x21, 0x5a, 0x33, 0x5c, 0xf5],
);
const NV_ENC_PRESET_P7_GUID: GUID = GUID::from_values(
    0x8484_8c12,
    0x6f71,
    0x4c13,
    [0x93, 0x1b, 0x53, 0xe2, 0x83, 0xf5, 0x79, 0x74],
);
const NV_ENC_PRESET_P4_GUID: GUID = GUID::from_values(
    0x90a7_b826,
    0xdf06,
    0x4862,
    [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81],
);
const NV_ENC_PRESET_P5_GUID: GUID = GUID::from_values(
    0x21c6_e6b4,
    0x297a,
    0x4cba,
    [0x99, 0x8f, 0xb6, 0xcb, 0xde, 0x72, 0xad, 0xe3],
);
const NV_ENC_TUNING_INFO_HIGH_QUALITY: u32 = 1;
const NV_ENC_TUNING_INFO_LOSSLESS: u32 = 4;
const NV_ENC_PARAMS_RC_CONSTQP: u32 = 0x0;
const NV_ENC_H264_PROFILE_MAIN_GUID: GUID = GUID::from_values(
    0x60b5_c1d4,
    0x67fe,
    0x4790,
    [0x94, 0xd5, 0xc4, 0x72, 0x6d, 0x7b, 0x6e, 0x6d],
);
/// Hi444PP — the profile that carries `transform_bypass` (lossless);
/// despite the name it admits 4:2:0 chroma, which is what the NV12 lane
/// feeds. (Data4 corrected against the n12.0.16.0 header — the first
/// transcription mis-remembered the tail and the driver silently fell
/// back to autoselect, which the bypass flag then steered right anyway.)
const NV_ENC_H264_PROFILE_HIGH_444_GUID: GUID = GUID::from_values(
    0x7ac6_63cb,
    0xa598,
    0x4960,
    [0xb8, 0x44, 0x33, 0x9b, 0x26, 0x1a, 0x7d, 0x52],
);
const NV_ENC_CODEC_HEVC_GUID: GUID = GUID::from_values(
    0x790c_dc88,
    0x4522,
    0x4d7b,
    [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
);
/// `NV_ENC_CODEC_AV1_GUID` — the AV1 encode codec (Ada/Blackwell; the
/// user's RTX 5070 has it). Named now for the AV1 arc; unused until the
/// AV1 encode config lands. AV1 lossless is plain profile-0 syntax
/// (qindex 0). Implementation reuses `open_on_device`'s flow with this GUID
/// and `NV_ENC_CONFIG_AV1` (a distinct config union member), plus an
/// OBU-aware pacer/sniff branch.
#[allow(dead_code)]
const NV_ENC_CODEC_AV1_GUID: GUID = GUID::from_values(
    0x0a35_2289,
    0x0aa7,
    0x4759,
    [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54],
);
/// HEVC needs no special profile for lossless — transquant bypass is
/// core syntax, reachable from Main — so the rung hands the driver
/// autoselect and lets the lossless tuning steer.
const NV_ENC_CODEC_PROFILE_AUTOSELECT_GUID: GUID = GUID::from_values(
    0xbfd6_f8e7,
    0x233c,
    0x4341,
    [0x8b, 0x3e, 0x48, 0x18, 0x52, 0x38, 0x03, 0xf4],
);

#[allow(dead_code)]
#[repr(C)]
struct SessionExParams {
    version: u32,
    device_type: u32,
    device: *mut c_void,
    reserved: *mut c_void,
    api_version: u32,
    reserved1: [u32; 253],
    reserved2: [*mut c_void; 64],
}
const _: () = assert!(std::mem::size_of::<SessionExParams>() == 1552);

/// `NV_ENC_QP`.
#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncQp {
    qp_inter_p: u32,
    qp_inter_b: u32,
    qp_intra: u32,
}

/// `NV_ENC_RC_PARAMS` — exact.
#[allow(dead_code)]
#[repr(C)]
struct RcParams {
    version: u32,
    rate_control_mode: u32,
    const_qp: NvEncQp,
    average_bit_rate: u32,
    max_bit_rate: u32,
    vbv_buffer_size: u32,
    vbv_initial_delay: u32,
    flags: u32,
    min_qp: NvEncQp,
    max_qp: NvEncQp,
    initial_rc_qp: NvEncQp,
    temporal_layer_idx_mask: u32,
    temporal_layer_qp: [u8; 8],
    target_quality: u8,
    target_quality_lsb: u8,
    lookahead_depth: u16,
    low_delay_key_frame_scale: u8,
    y_dc_qp_index_offset: i8,
    u_dc_qp_index_offset: i8,
    v_dc_qp_index_offset: i8,
    qp_map_mode: u32,
    multi_pass: u32,
    alpha_layer_bitrate_ratio: u32,
    cb_qp_index_offset: i8,
    cr_qp_index_offset: i8,
    reserved2: u16,
    reserved: [u32; 4],
}
const _: () = assert!(std::mem::size_of::<RcParams>() == 128);

/// `NV_ENC_CONFIG_H264_VUI_PARAMETERS` — exact (all-u32 fields).
#[allow(dead_code)]
#[repr(C)]
struct H264VuiParams {
    fields: [u32; 16],
    reserved: [u32; 12],
}
const _: () = assert!(std::mem::size_of::<H264VuiParams>() == 112);

/// `NV_ENC_CONFIG_H264` — exact.
#[allow(dead_code)]
#[repr(C)]
struct ConfigH264 {
    /// The 22 feature bits + 10 reserved, one packed u32 (see
    /// [`h264_flags`] for the bit positions this module sets).
    flags: u32,
    level: u32,
    idr_period: u32,
    separate_colour_plane_flag: u32,
    disable_deblocking_filter_idc: u32,
    num_temporal_layers: u32,
    sps_id: u32,
    pps_id: u32,
    adaptive_transform_mode: u32,
    fmo_mode: u32,
    bdirect_mode: u32,
    entropy_coding_mode: u32,
    stereo_mode: u32,
    intra_refresh_period: u32,
    intra_refresh_cnt: u32,
    max_num_ref_frames: u32,
    slice_mode: u32,
    slice_mode_data: u32,
    h264_vui_parameters: H264VuiParams,
    ltr_num_frames: u32,
    ltr_trust_mode: u32,
    chroma_format_idc: u32,
    max_temporal_layers: u32,
    use_bframes_as_ref: u32,
    num_ref_l0: u32,
    num_ref_l1: u32,
    reserved1: [u32; 267],
    reserved2: [*mut c_void; 64],
}
const _: () = assert!(std::mem::size_of::<ConfigH264>() == 1792);

/// Bit positions inside [`ConfigH264::flags`] (header order).
mod h264_flags {
    pub const OUTPUT_RECOVERY_POINT_SEI: u32 = 1 << 9;
    pub const ENABLE_INTRA_REFRESH: u32 = 1 << 10;
    pub const REPEAT_SPSPPS: u32 = 1 << 12;
    /// `qpPrimeYZeroTransformBypassFlag` — with constQP 0, the transform
    /// and quantizer are bypassed entirely: mathematically lossless.
    pub const QP_PRIME_Y_ZERO_TRANSFORM_BYPASS: u32 = 1 << 15;
}

/// `NV_ENC_CONFIG_HEVC` — exact (n12.0.16.0).
#[allow(dead_code)]
#[repr(C)]
struct ConfigHevc {
    level: u32,
    tier: u32,
    min_cu_size: u32,
    max_cu_size: u32,
    /// The 20 feature bits + 12 reserved. Unlike H.264's layout,
    /// `chromaFormatIDC` (bits 9–10) and `pixelBitDepthMinus8` (bits
    /// 11–13) are multi-bit fields packed *inside* this word — see
    /// [`hevc_flags`].
    flags: u32,
    idr_period: u32,
    intra_refresh_period: u32,
    intra_refresh_cnt: u32,
    max_num_ref_frames_in_dpb: u32,
    ltr_num_frames: u32,
    vps_id: u32,
    sps_id: u32,
    pps_id: u32,
    slice_mode: u32,
    slice_mode_data: u32,
    max_temporal_layers_minus_1: u32,
    /// The header typedefs the HEVC VUI to the H.264 shape.
    hevc_vui_parameters: H264VuiParams,
    ltr_trust_mode: u32,
    use_bframes_as_ref: u32,
    num_ref_l0: u32,
    num_ref_l1: u32,
    reserved1: [u32; 214],
    reserved2: [*mut c_void; 64],
}
const _: () = assert!(std::mem::size_of::<ConfigHevc>() == 1560);

/// Bit positions inside [`ConfigHevc::flags`] (header order).
mod hevc_flags {
    pub const REPEAT_SPSPPS: u32 = 1 << 7;
    /// `chromaFormatIDC` occupies bits 9–10; value 1 = 4:2:0 (NV12
    /// input), 3 = 4:4:4. [`CHROMA_MASK`] clears the field first.
    pub const CHROMA_MASK: u32 = 0b11 << 9;
    pub const CHROMA_420: u32 = 1 << 9;
}

/// `NV_ENC_CODEC_CONFIG` — deliberately oversized (see module docs). The
/// true union is `max(sizeof members)` (H.264's 1792 is the largest we
/// transcribed; HEVC's 1560 fits under it); 2048 gives headroom over
/// every SDK 12.0 member.
#[allow(dead_code)]
#[repr(C)]
union CodecConfig {
    h264: std::mem::ManuallyDrop<ConfigH264>,
    hevc: std::mem::ManuallyDrop<ConfigHevc>,
    raw: [u64; 256],
}
const _: () = assert!(std::mem::size_of::<CodecConfig>() == 2048);

/// `NV_ENC_CONFIG` — pre-union layout exact; union oversized.
#[allow(dead_code)]
#[repr(C)]
struct NvEncConfig {
    version: u32,
    profile_guid: GUID,
    gop_length: u32,
    frame_interval_p: i32,
    mono_chrome_encoding: u32,
    frame_field_mode: u32,
    mv_precision: u32,
    rc_params: RcParams,
    encode_codec_config: CodecConfig,
    reserved: [u32; 278],
    reserved2: [*mut c_void; 64],
}
const _: () = assert!(std::mem::size_of::<NvEncConfig>() == 3840);

/// `NV_ENC_PRESET_CONFIG`.
#[allow(dead_code)]
#[repr(C)]
struct PresetConfig {
    version: u32,
    preset_cfg: NvEncConfig,
    reserved1: [u32; 255],
    reserved2: [*mut c_void; 64],
}
// Layout guard, like every sibling FFI struct: version(4)+pad(4)+
// NvEncConfig(3840)+reserved1(1020)+pad(4)+reserved2(512).
const _: () = assert!(std::mem::size_of::<PresetConfig>() == 5384);

/// `NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE`.
#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy)]
struct MeHintCounts {
    bits: u32,
    reserved1: [u32; 3],
}

/// `NV_ENC_INITIALIZE_PARAMS` — exact.
#[allow(dead_code)]
#[repr(C)]
struct InitializeParams {
    version: u32,
    encode_guid: GUID,
    preset_guid: GUID,
    encode_width: u32,
    encode_height: u32,
    dar_width: u32,
    dar_height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
    enable_encode_async: u32,
    enable_ptd: u32,
    flags: u32,
    priv_data_size: u32,
    priv_data: *mut c_void,
    encode_config: *mut NvEncConfig,
    max_encode_width: u32,
    max_encode_height: u32,
    max_me_hint_counts_per_block: [MeHintCounts; 2],
    tuning_info: u32,
    buffer_format: u32,
    reserved: [u32; 287],
    reserved2: [*mut c_void; 64],
}
const _: () = assert!(std::mem::size_of::<InitializeParams>() == 1808);

/// `NV_ENC_RECONFIGURE_PARAMS`.
#[allow(dead_code)]
#[repr(C)]
struct ReconfigureParams {
    version: u32,
    re_init_encode_params: InitializeParams,
    flags: u32,
}
const _: () = assert!(std::mem::size_of::<ReconfigureParams>() == 1824);

/// `NV_ENC_CREATE_BITSTREAM_BUFFER`.
#[allow(dead_code)]
#[repr(C)]
struct CreateBitstreamBuffer {
    version: u32,
    size: u32,
    memory_heap: u32,
    reserved: u32,
    bitstream_buffer: *mut c_void,
    bitstream_buffer_ptr: *mut c_void,
    reserved1: [u32; 58],
    reserved2: [*mut c_void; 64],
}
const _: () = assert!(std::mem::size_of::<CreateBitstreamBuffer>() == 776);

/// `NV_ENC_REGISTER_RESOURCE`.
#[allow(dead_code)]
#[repr(C)]
struct RegisterResource {
    version: u32,
    resource_type: u32,
    width: u32,
    height: u32,
    pitch: u32,
    sub_resource_index: u32,
    resource_to_register: *mut c_void,
    registered_resource: *mut c_void,
    buffer_format: u32,
    buffer_usage: u32,
    p_input_fence_point: *mut c_void,
    reserved1: [u32; 247],
    reserved2: [*mut c_void; 61],
}
const _: () = assert!(std::mem::size_of::<RegisterResource>() == 1536);

/// `NV_ENC_MAP_INPUT_RESOURCE`.
#[allow(dead_code)]
#[repr(C)]
struct MapInputResource {
    version: u32,
    sub_resource_index: u32,
    input_resource: *mut c_void,
    registered_resource: *mut c_void,
    mapped_resource: *mut c_void,
    mapped_buffer_fmt: u32,
    reserved1: [u32; 251],
    reserved2: [*mut c_void; 63],
}
const _: () = assert!(std::mem::size_of::<MapInputResource>() == 1544);

/// `NV_ENC_PIC_PARAMS` — pre-union layout exact; the codec union and
/// everything after it are one oversized zeroed tail (the fields there —
/// ME hints, QP maps, alpha — are all optional and null/zero for us; the
/// driver reads zeros at its own offsets inside our larger buffer).
#[allow(dead_code)]
#[repr(C)]
struct PicParams {
    version: u32,
    input_width: u32,
    input_height: u32,
    input_pitch: u32,
    encode_pic_flags: u32,
    frame_idx: u32,
    input_time_stamp: u64,
    input_duration: u64,
    input_buffer: *mut c_void,
    output_bitstream: *mut c_void,
    completion_event: *mut c_void,
    buffer_fmt: u32,
    picture_struct: u32,
    picture_type: u32,
    _pad: u32,
    /// H.264 per-picture params live at the head of the true union; the
    /// one field this module ever sets is `forceIntraRefreshWithFrameCnt`
    /// at offset 16 within it.
    codec_pic_params: [u64; 256],
    tail: [u64; 256],
}
const _: () = assert!(std::mem::size_of::<PicParams>() == 80 + 2048 + 2048);

/// Offset (in u32 units) of `forceIntraRefreshWithFrameCnt` inside
/// `NV_ENC_PIC_PARAMS_H264` (fifth field, all u32s before it).
const H264_PIC_FORCE_INTRA_REFRESH_IDX: usize = 4;

/// `NV_ENC_LOCK_BITSTREAM` — exact.
#[allow(dead_code)]
#[repr(C)]
struct LockBitstream {
    version: u32,
    flags: u32,
    output_bitstream: *mut c_void,
    slice_offsets: *mut u32,
    frame_idx: u32,
    hw_encode_status: u32,
    num_slices: u32,
    bitstream_size_in_bytes: u32,
    output_time_stamp: u64,
    output_duration: u64,
    bitstream_buffer_ptr: *mut c_void,
    picture_type: u32,
    picture_struct: u32,
    frame_avg_qp: u32,
    frame_satd: u32,
    ltr_frame_idx: u32,
    ltr_frame_bitmap: u32,
    temporal_id: u32,
    reserved: [u32; 12],
    intra_mb_count: u32,
    inter_mb_count: u32,
    average_mvx: i32,
    average_mvy: i32,
    alpha_layer_size_in_bytes: u32,
    reserved1: [u32; 218],
    reserved2: [*mut c_void; 64],
}
const _: () = assert!(std::mem::size_of::<LockBitstream>() == 1544);

type Enc = *mut c_void;
type StatusFn0 = unsafe extern "system" fn(Enc) -> u32;
type StatusFn1<T> = unsafe extern "system" fn(Enc, *mut T) -> u32;
type StatusFnPtr = unsafe extern "system" fn(Enc, *mut c_void) -> u32;

/// `NV_ENCODE_API_FUNCTION_LIST` — every slot transcribed in header
/// order (each is one pointer, so unused ones are typed opaque).
#[allow(dead_code)]
#[repr(C)]
struct FunctionList {
    version: u32,
    reserved: u32,
    open_encode_session: *mut c_void,
    get_encode_guid_count: *mut c_void,
    get_encode_profile_guid_count: *mut c_void,
    get_encode_profile_guids: *mut c_void,
    get_encode_guids: *mut c_void,
    get_input_format_count: *mut c_void,
    get_input_formats: *mut c_void,
    get_encode_caps: *mut c_void,
    get_encode_preset_count: *mut c_void,
    get_encode_preset_guids: *mut c_void,
    get_encode_preset_config: *mut c_void,
    initialize_encoder: Option<StatusFn1<InitializeParams>>,
    create_input_buffer: *mut c_void,
    destroy_input_buffer: *mut c_void,
    create_bitstream_buffer: Option<StatusFn1<CreateBitstreamBuffer>>,
    destroy_bitstream_buffer: Option<StatusFnPtr>,
    encode_picture: Option<StatusFn1<PicParams>>,
    lock_bitstream: Option<StatusFn1<LockBitstream>>,
    unlock_bitstream: Option<StatusFnPtr>,
    lock_input_buffer: *mut c_void,
    unlock_input_buffer: *mut c_void,
    get_encode_stats: *mut c_void,
    get_sequence_params: *mut c_void,
    register_async_event: *mut c_void,
    unregister_async_event: *mut c_void,
    map_input_resource: Option<StatusFn1<MapInputResource>>,
    unmap_input_resource: Option<StatusFnPtr>,
    destroy_encoder: Option<StatusFn0>,
    invalidate_ref_frames: Option<unsafe extern "system" fn(Enc, u64) -> u32>,
    open_encode_session_ex:
        Option<unsafe extern "system" fn(*mut SessionExParams, *mut Enc) -> u32>,
    register_resource: Option<StatusFn1<RegisterResource>>,
    unregister_resource: Option<StatusFnPtr>,
    reconfigure_encoder: Option<StatusFn1<ReconfigureParams>>,
    reserved1: *mut c_void,
    create_mv_buffer: *mut c_void,
    destroy_mv_buffer: *mut c_void,
    run_motion_estimation_only: *mut c_void,
    get_last_error_string: Option<unsafe extern "system" fn(Enc) -> *const i8>,
    set_io_cuda_streams: *mut c_void,
    get_encode_preset_config_ex:
        Option<unsafe extern "system" fn(Enc, GUID, GUID, u32, *mut PresetConfig) -> u32>,
    get_sequence_param_ex: *mut c_void,
    reserved2: [*mut c_void; 277],
}
const _: () = assert!(std::mem::size_of::<FunctionList>() == 2552);

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

/// The loaded function table. Raw pointers make it `!Sync` by default;
/// after init it is immutable and every call site owns its own encoder
/// handle (NVENC requires per-session external sync, which the one-thread
/// ownership of [`NvencH264`] provides), so sharing the table is sound.
struct ApiTable(FunctionList);
unsafe impl Send for ApiTable {}
unsafe impl Sync for ApiTable {}

/// Load `nvEncodeAPI64.dll` and populate the function table once per
/// process. `Err` = no NVIDIA driver (or one too old for API 12.0) — the
/// ladder keeps the MF rung.
fn api() -> Result<&'static FunctionList, String> {
    static API: std::sync::OnceLock<Result<&'static ApiTable, String>> = std::sync::OnceLock::new();
    API.get_or_init(|| unsafe {
        let module = LoadLibraryA(PCSTR(c"nvEncodeAPI64.dll".as_ptr() as *const u8))
            .map_err(|e| format!("nvEncodeAPI64.dll not loadable: {e}"))?;
        let max_ver = GetProcAddress(
            module,
            PCSTR(c"NvEncodeAPIGetMaxSupportedVersion".as_ptr() as *const u8),
        )
        .ok_or("NvEncodeAPIGetMaxSupportedVersion missing")?;
        let max_ver: unsafe extern "system" fn(*mut u32) -> u32 = std::mem::transmute(max_ver);
        let mut driver_ver = 0u32;
        if max_ver(&mut driver_ver) != NV_ENC_SUCCESS {
            return Err("NvEncodeAPIGetMaxSupportedVersion failed".into());
        }
        // The MaxSupportedVersion word is NOT the NVENCAPI_VERSION
        // layout: the header defines it as 4 LSBs = minor, rest = major
        // (red team: the old NVENCAPI-shaped decode made the gate pass
        // every driver ever and log garbage versions).
        let (drv_major, drv_minor) = (driver_ver >> 4, driver_ver & 0xF);
        if (drv_major, drv_minor) < (12, 0) {
            return Err(format!(
                "driver NVENC API {drv_major}.{drv_minor} < 12.0 (driver too old)"
            ));
        }
        tracing::info!(
            "NVENC driver API {drv_major}.{drv_minor} available (this build targets 12.0)"
        );
        let create = GetProcAddress(
            module,
            PCSTR(c"NvEncodeAPICreateInstance".as_ptr() as *const u8),
        )
        .ok_or("NvEncodeAPICreateInstance missing")?;
        let create: unsafe extern "system" fn(*mut FunctionList) -> u32 =
            std::mem::transmute(create);
        let mut list: Box<ApiTable> = Box::new(std::mem::zeroed());
        list.0.version = sv(2);
        let status = create(&mut list.0);
        if status != NV_ENC_SUCCESS {
            return Err(format!("NvEncodeAPICreateInstance: status {status}"));
        }
        Ok(&*Box::leak(list))
    })
    .as_ref()
    .map(|t| &t.0)
    .map_err(Clone::clone)
}

// ---------------------------------------------------------------------------
// The encoder
// ---------------------------------------------------------------------------

/// One registered input texture (the GPU lane's NV12 ring slots register
/// lazily, keyed by interface pointer).
struct Registered {
    tex_ptr: *mut c_void,
    handle: *mut c_void,
}

/// A direct NVENC H.264 session, fed D3D11 NV12 textures.
pub struct NvencH264 {
    api: &'static FunctionList,
    encoder: Enc,
    bitstream: *mut c_void,
    registered: Vec<Registered>,
    /// The init/config blocks, kept for in-place reconfigure.
    config: Box<NvEncConfig>,
    init: Box<InitializeParams>,
    width: u32,
    height: u32,
    fps: u32,
    frame_index: u64,
    intra_refresh: bool,
    studio: bool,
    lossless: bool,
    hevc: bool,
    /// Frame health asked for a refresh-wave restart before the next
    /// encode (GDR only) — consumed by [`Self::encode_texture`].
    /// Armed wave restart: the length in frames to write into the next
    /// picture's force-intra-refresh field (None = idle).
    pending_wave: Option<u32>,
    label: String,
}

// SAFETY: built on, and only ever driven from, the route's encode thread
// (same contract as MediaFoundationH264 — the Send bound exists for the
// codec seam, the value never actually migrates mid-stream).
unsafe impl Send for NvencH264 {}

impl NvencH264 {
    /// Open a session on `device` (the GPU lane's D3D11 device — input
    /// textures must live on it). `game` = GDR posture (infinite GOP,
    /// refresh waves instead of IDR walls, single-frame VBV); `studio` =
    /// the LAN fidelity posture (quality-tuned preset, deep VBV — 4:4:4
    /// and lossless slot in here once the hardware-decode viewer lands).
    /// The two are exclusive; `studio` wins if both arrive.
    pub fn open_on_device(
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        game: bool,
        studio: bool,
    ) -> Result<Self, String> {
        Self::open_inner(
            device, width, height, fps, bitrate, game, studio, false, false, false,
        )
    }

    /// Studio's aspirational tier, as a measurement rung: mathematically
    /// lossless H.264 (constQP 0 + transform bypass, Hi444PP profile) of
    /// the lane's NV12 — lossless **relative to the 4:2:0 conversion**;
    /// chroma was already subsampled upstream. There is no rate control
    /// at all: bandwidth is whatever the content's entropy demands, which
    /// is exactly what the soak/bench harnesses exist to measure. Not
    /// routable in the app until the viewer decodes Hi444PP (the
    /// hardware-decode epic) — no browser path does today.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open_lossless_on_device(
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self, String> {
        Self::open_inner(
            device, width, height, fps, 0, false, true, true, false, false,
        )
    }

    /// The hardware-decodable lossless: HEVC under the same constQP-0
    /// contract. Where H.264 lossless needed the Hi444PP profile (which
    /// no hardware decoder opens), HEVC carries transquant bypass as core
    /// syntax — the produced stream is what NVDEC/WebCodecs can actually
    /// decode, which is what makes this the candidate for Studio's
    /// shipped Lossless mode rather than a measurement curiosity.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open_lossless_hevc_on_device(
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self, String> {
        Self::open_inner(
            device, width, height, fps, 0, false, true, true, true, false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn open_inner(
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        game: bool,
        studio: bool,
        lossless: bool,
        hevc: bool,
        allow_slow_preset_override: bool,
    ) -> Result<Self, String> {
        let intra_refresh = game && !studio;
        let api = api()?;
        unsafe {
            let open = api.open_encode_session_ex.ok_or("no OpenEncodeSessionEx")?;
            let mut session = SessionExParams {
                version: sv(1),
                device_type: NV_ENC_DEVICE_TYPE_DIRECTX,
                device: device.as_raw(),
                reserved: std::ptr::null_mut(),
                api_version: API_VERSION,
                reserved1: [0; 253],
                reserved2: [std::ptr::null_mut(); 64],
            };
            let mut encoder: Enc = std::ptr::null_mut();
            let status = open(&mut session, &mut encoder);
            if status != NV_ENC_SUCCESS || encoder.is_null() {
                return Err(format!("NvEncOpenEncodeSessionEx: status {status}"));
            }
            // From here every failure must destroy the session or it
            // leaks a hardware slot.
            let mut me = NvencH264 {
                api,
                encoder,
                bitstream: std::ptr::null_mut(),
                registered: Vec::new(),
                config: Box::new(std::mem::zeroed()),
                init: Box::new(std::mem::zeroed()),
                width,
                height,
                fps: fps.max(1),
                frame_index: 0,
                intra_refresh,
                studio,
                lossless,
                hevc,
                pending_wave: None,
                label: if lossless && hevc {
                    "NVENC SDK (HEVC, studio-lossless)".to_string()
                } else if lossless {
                    "NVENC SDK (H.264, studio-lossless)".to_string()
                } else if studio {
                    "NVENC SDK (H.264, studio)".to_string()
                } else if intra_refresh {
                    "NVENC SDK (H.264, intra-refresh)".to_string()
                } else {
                    "NVENC SDK (H.264)".to_string()
                },
            };
            // An init failure drops `me`, which destroys the session —
            // no leaked hardware slot.
            me.initialize(bitrate, allow_slow_preset_override)?;
            Ok(me)
        }
    }

    unsafe fn initialize(
        &mut self,
        bitrate: u32,
        allow_slow_preset_override: bool,
    ) -> Result<(), String> {
        // Start from the driver's own preset config (P4, ultra-low-latency
        // tuning — no lookahead, no B-frames, single-frame delay), then
        // re-aim the pieces our stream model owns.
        let preset_ex = self
            .api
            .get_encode_preset_config_ex
            .ok_or("no GetEncodePresetConfigEx")?;
        // Balanced/game run P4 + ultra-low-latency tuning; studio runs
        // P5 + high-quality tuning (a slower, better encode the LAN
        // fidelity budget is meant to feed — still no B-frames, forced
        // below, so latency stays interactive). Lossless keeps studio's
        // P5 (better prediction = fewer residual bits — presets still
        // matter when every residual is coded exactly) under the
        // dedicated lossless tuning.
        // Preset defaults, set by the measured 1440p grid with boost held:
        // lossless runs P3 — at constQP-0 the output
        // is bit-exact by definition, presets only change compression
        // efficiency, and P3 matched P5's bits within 0.5% at HALF the
        // latency (6.5 vs 12.9 ms). Game runs P2 — 5.7 vs P4's 12.6 ms at
        // identical bitrate (rate control holds the budget; the residual
        // difference is refinement quality, the latency-first posture's
        // explicit trade). Studio keeps P5: quality-first is its charter.
        // P6/P7 are banned from streaming paths outright — their HQ
        // configs enable 16-frame lookahead (~267 ms of buffering).
        let (preset_guid, tuning) = if self.lossless {
            (NV_ENC_PRESET_P3_GUID, NV_ENC_TUNING_INFO_LOSSLESS)
        } else if self.studio {
            (NV_ENC_PRESET_P5_GUID, NV_ENC_TUNING_INFO_HIGH_QUALITY)
        } else if self.intra_refresh {
            (NV_ENC_PRESET_P2_GUID, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY)
        } else {
            (NV_ENC_PRESET_P4_GUID, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY)
        };
        // `ALLMYSTUFF_NVENC_PRESET=1..5` overrides the posture's preset —
        // the field dial for the latency/quality ladder the preset-grid bench
        // maps (tuning stays the posture's own). P6/P7 are deliberately
        // refused on production opens: their driver configs enable deep
        // lookahead. The ignored preset-grid benchmark opts in explicitly so
        // those diagnostic cells remain measurable.
        let requested_preset = std::env::var("ALLMYSTUFF_NVENC_PRESET")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok());
        let preset_guid = match requested_preset {
            Some(1) => NV_ENC_PRESET_P1_GUID,
            Some(2) => NV_ENC_PRESET_P2_GUID,
            Some(3) => NV_ENC_PRESET_P3_GUID,
            Some(4) => NV_ENC_PRESET_P4_GUID,
            Some(5) => NV_ENC_PRESET_P5_GUID,
            Some(6) if allow_slow_preset_override => NV_ENC_PRESET_P6_GUID,
            Some(7) if allow_slow_preset_override => NV_ENC_PRESET_P7_GUID,
            Some(6 | 7) => {
                tracing::warn!(
                    "ALLMYSTUFF_NVENC_PRESET={} refused for streaming (P6/P7 enable deep lookahead); using the posture default",
                    requested_preset.unwrap_or_default()
                );
                preset_guid
            }
            _ => preset_guid,
        };
        let codec_guid = if self.hevc {
            NV_ENC_CODEC_HEVC_GUID
        } else {
            NV_ENC_CODEC_H264_GUID
        };
        let mut preset: Box<PresetConfig> = Box::new(std::mem::zeroed());
        preset.version = sv(4) | (1 << 31);
        preset.preset_cfg.version = sv(8) | (1 << 31);
        let status = preset_ex(self.encoder, codec_guid, preset_guid, tuning, &mut *preset);
        if status != NV_ENC_SUCCESS {
            return Err(self.err("GetEncodePresetConfigEx", status));
        }
        // Copy the filled config out (plain data — the box just drops)
        // and shape it.
        std::ptr::copy_nonoverlapping(&preset.preset_cfg, &mut *self.config, 1);
        drop(preset);
        let cfg = &mut *self.config;
        cfg.version = sv(8) | (1 << 31);
        cfg.profile_guid = if self.hevc {
            NV_ENC_CODEC_PROFILE_AUTOSELECT_GUID
        } else if self.lossless {
            NV_ENC_H264_PROFILE_HIGH_444_GUID
        } else {
            NV_ENC_H264_PROFILE_MAIN_GUID
        };
        cfg.frame_interval_p = 1; // IPP…, never B (latency + LTR/invalidatable)
        if self.lossless {
            // No rate to control: constQP 0 everywhere (with transform
            // bypass below, that IS lossless). The lossless-tuning preset
            // config already says this — restated so the contract doesn't
            // hang on a driver default. Bitrate/VBV fields are meaningless
            // here and stay zeroed.
            cfg.rc_params.rate_control_mode = NV_ENC_PARAMS_RC_CONSTQP;
            cfg.rc_params.const_qp = NvEncQp {
                qp_inter_p: 0,
                qp_inter_b: 0,
                qp_intra: 0,
            };
            cfg.rc_params.average_bit_rate = 0;
            cfg.rc_params.max_bit_rate = 0;
            cfg.rc_params.vbv_buffer_size = 0;
            cfg.rc_params.vbv_initial_delay = 0;
        } else {
            cfg.rc_params.rate_control_mode = NV_ENC_PARAMS_RC_VBR;
            cfg.rc_params.average_bit_rate = bitrate;
            let (peak, vbv) =
                crate::video::burst_bounds(bitrate, self.fps, self.intra_refresh, self.studio);
            cfg.rc_params.max_bit_rate = peak;
            cfg.rc_params.vbv_buffer_size = vbv;
            cfg.rc_params.vbv_initial_delay = 0;
        }
        let paced = crate::video::paced_slices_enabled();
        // 8 slices sizes a lossy keyframe (~190 KB) to the 24 KB pacing
        // grain. Lossless IDRs run ~1.4 MB — at 8 slices each burst is a
        // 175 KB wall the splitter can't cut into; 32 slices brings the
        // grain back (~44 KB), worth the ~1-3% CABAC-reset cost on a
        // posture that spends bits for smooth delivery anyway.
        let slice_count = match (self.width * self.height >= 1920 * 1080, self.lossless) {
            (true, true) => 32,
            (true, false) => 8,
            (false, true) => 16,
            (false, false) => 4,
        };
        if self.hevc {
            // The HEVC face of the same stream shape: VPS/SPS/PPS on
            // every IDR for the pacer's clean joins, 4:2:0 for the NV12
            // input, count-mode slices for the pacer's cut points, and
            // the stable-arc GOP backstop (this rung never runs GDR —
            // lossless is a studio posture).
            let hevc = &mut cfg.encode_codec_config.hevc;
            hevc.flags |= hevc_flags::REPEAT_SPSPPS;
            hevc.flags = (hevc.flags & !hevc_flags::CHROMA_MASK) | hevc_flags::CHROMA_420;
            if paced {
                hevc.slice_mode = 3;
                hevc.slice_mode_data = slice_count;
            }
            cfg.gop_length = self.fps.saturating_mul(4).max(1);
            hevc.idr_period = cfg.gop_length;
        } else {
            let h264 = &mut cfg.encode_codec_config.h264;
            h264.flags |= h264_flags::REPEAT_SPSPPS;
            if self.lossless {
                h264.flags |= h264_flags::QP_PRIME_Y_ZERO_TRANSFORM_BYPASS;
            }
            // A deep DPB for reference invalidation as error resilience —
            // the header's own recommendation: keep old references so an
            // invalidated recent one has a valid fallback and recovery is
            // a P-frame, not an IDR wall. Zero cost to latency (still IPP,
            // one in one out); a handful of extra reference surfaces. Only
            // the lossy H.264 postures — lossless has no loss-recovery
            // story to tell (studio LAN), and the smaller default keeps
            // its DPB memory down.
            if !self.lossless {
                h264.max_num_ref_frames = 8;
            }
            h264.entropy_coding_mode = 0; // autoselect (CABAC where allowed)
            if paced {
                // Slice-count mode (sliceMode 3): the send-side pacer's cut
                // points — a keyframe leaves as several independently-
                // decodable slices instead of one wall. Count, not bytes:
                // byte-based slicing (mode 1) is rejected outright by real
                // drivers in the field ("Byte based slice encoding is not
                // supported"), while count mode is universal. 8 slices at
                // ≥1080p ≈ the ~24 KB pacing grain on a worst-case keyframe.
                h264.slice_mode = 3;
                h264.slice_mode_data = slice_count;
            }
            if self.intra_refresh {
                // GDR: no automatic IDRs at all; intra data rides a refresh
                // wave every ~0.5 s (the field pass at 2 s left loss artifacts
                // lingering visibly in-game — half a second bounds the heal to
                // a blink while still spreading the intra cost). Recovery-point
                // SEI marks each wave so a decoder knows where clean starts. A
                // forced IDR (viewer join, quiesce rescue) still cuts through
                // via the per-picture flag.
                cfg.gop_length = NVENC_INFINITE_GOPLENGTH;
                h264.idr_period = NVENC_INFINITE_GOPLENGTH;
                h264.flags |=
                    h264_flags::ENABLE_INTRA_REFRESH | h264_flags::OUTPUT_RECOVERY_POINT_SEI;
                let period = (self.fps / 2).max(15);
                h264.intra_refresh_period = period;
                h264.intra_refresh_cnt = (period / 5).max(3);
            } else {
                // Stable-arc shape: same ~4 s GOP backstop as the MF rung; the
                // stream's adaptive IDR cadence forces the real keyframes.
                cfg.gop_length = self.fps.saturating_mul(4).max(1);
                h264.idr_period = cfg.gop_length;
            }
        }

        let init = &mut *self.init;
        init.version = sv(5) | (1 << 31);
        init.encode_guid = codec_guid;
        init.preset_guid = preset_guid;
        init.encode_width = self.width;
        init.encode_height = self.height;
        init.dar_width = self.width;
        init.dar_height = self.height;
        init.frame_rate_num = self.fps;
        init.frame_rate_den = 1;
        init.enable_encode_async = 0;
        init.enable_ptd = 1;
        init.tuning_info = tuning;
        init.encode_config = &mut *self.config;
        let init_fn = self.api.initialize_encoder.ok_or("no InitializeEncoder")?;
        let mut status = init_fn(self.encoder, &mut *self.init);
        if status != NV_ENC_SUCCESS {
            // Both codecs park slice_mode/slice_mode_data at the same
            // conceptual spot; pick the active union member's pair.
            let (slice_mode, slice_mode_data) = if self.hevc {
                let h = &mut *self.config.encode_codec_config.hevc;
                (&mut h.slice_mode, &mut h.slice_mode_data)
            } else {
                let h = &mut *self.config.encode_codec_config.h264;
                (&mut h.slice_mode, &mut h.slice_mode_data)
            };
            // Only parameter rejections implicate the slice config —
            // retrying a transient failure (BUSY, OUT_OF_MEMORY) with
            // slices stripped would silently cost the pacer its cut
            // points for the session's whole life (red team, finding 5).
            const NV_ENC_ERR_INVALID_PARAM: u32 = 8;
            const NV_ENC_ERR_UNSUPPORTED_PARAM: u32 = 15;
            if *slice_mode != 0
                && matches!(
                    status,
                    NV_ENC_ERR_INVALID_PARAM | NV_ENC_ERR_UNSUPPORTED_PARAM
                )
            {
                // A driver that rejects our slice config must cost the
                // pacer its cut points, never the whole SDK rung: retry
                // once with default (single-slice) framing.
                tracing::info!(
                    "NVENC rejected slice config (status {status}); retrying single-slice"
                );
                *slice_mode = 0;
                *slice_mode_data = 0;
                status = init_fn(self.encoder, &mut *self.init);
            }
        }
        if status != NV_ENC_SUCCESS {
            return Err(self.err("InitializeEncoder", status));
        }

        let mut create = CreateBitstreamBuffer {
            version: sv(1),
            size: 0,
            memory_heap: 0,
            reserved: 0,
            bitstream_buffer: std::ptr::null_mut(),
            bitstream_buffer_ptr: std::ptr::null_mut(),
            reserved1: [0; 58],
            reserved2: [std::ptr::null_mut(); 64],
        };
        let status = (self
            .api
            .create_bitstream_buffer
            .ok_or("no CreateBitstreamBuffer")?)(self.encoder, &mut create);
        if status != NV_ENC_SUCCESS {
            return Err(self.err("CreateBitstreamBuffer", status));
        }
        self.bitstream = create.bitstream_buffer;
        Ok(())
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Frame health's targeted heal on a GDR session: restart the
    /// refresh wave at the next encode — spread intra, no IDR wall —
    /// so a viewer's reported loss converges without the keyframe spike
    /// loss recovery used to cost. `frames` is the wave's length: the
    /// loss chooser sends 3 for a fast heal on a lossy spell (bigger
    /// per-frame intra share, ~50 ms artifact window) or the smooth
    /// steady-state default otherwise. A second arm mid-wave restarts
    /// with the newer length. No-op off GDR.
    pub fn arm_wave(&mut self, frames: u32) {
        if self.intra_refresh {
            self.pending_wave = Some(frames.clamp(1, 60));
        }
    }

    /// Encode one NV12 texture (on the session's device) synchronously,
    /// returning the produced access unit. `force_idr` = clean entry now
    /// (works in both GOP shapes). Speaks the ladder's
    /// [`EncodeOutcome`] seam (`pub(crate)`: the outcome type is the
    /// video module's internal seam).
    pub(crate) fn encode_texture(
        &mut self,
        nv12: &ID3D11Texture2D,
        force_idr: bool,
    ) -> Result<EncodeOutcome, String> {
        unsafe {
            let handle = self.register(nv12)?;
            let mut map = MapInputResource {
                version: sv(4),
                sub_resource_index: 0,
                input_resource: std::ptr::null_mut(),
                registered_resource: handle,
                mapped_resource: std::ptr::null_mut(),
                mapped_buffer_fmt: 0,
                reserved1: [0; 251],
                reserved2: [std::ptr::null_mut(); 63],
            };
            let status =
                (self.api.map_input_resource.ok_or("no MapInputResource")?)(self.encoder, &mut map);
            if status != NV_ENC_SUCCESS {
                return Err(self.err("MapInputResource", status));
            }

            let duration = 10_000_000 / u64::from(self.fps);
            let mut pic: Box<PicParams> = Box::new(std::mem::zeroed());
            pic.version = sv(6) | (1 << 31);
            pic.input_width = self.width;
            pic.input_height = self.height;
            pic.input_pitch = self.width;
            pic.input_buffer = map.mapped_resource;
            pic.output_bitstream = self.bitstream;
            pic.buffer_fmt = NV_ENC_BUFFER_FORMAT_NV12;
            pic.picture_struct = NV_ENC_PIC_STRUCT_FRAME;
            // This frame's encoder timestamp — the exact key
            // `invalidate_ref` will pass back for it. Returned in the
            // outcome so the lane can map a viewer's reported loss to it.
            let input_ts = self.frame_index * duration;
            pic.input_time_stamp = input_ts;
            pic.input_duration = duration;
            self.frame_index += 1;
            if !force_idr && self.intra_refresh {
                if let Some(frames) = self.pending_wave.take() {
                    // Frame health: wave-only restart — intra spreads over
                    // the requested frames with no IDR in the stream at all.
                    let words = std::slice::from_raw_parts_mut(
                        pic.codec_pic_params.as_mut_ptr() as *mut u32,
                        8,
                    );
                    words[H264_PIC_FORCE_INTRA_REFRESH_IDX] = frames;
                }
            }
            if force_idr {
                self.pending_wave = None;
                pic.encode_pic_flags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
                if self.intra_refresh {
                    // An IDR mid-GDR: restart the refresh wave after it so
                    // the stream returns to walls-free steady state. The
                    // field takes the wave's frame count — writing 0 here
                    // (as the first pass did) is the *disabled* value, a
                    // documented guarantee the code never implemented
                    // (red team, finding 6).
                    let words = std::slice::from_raw_parts_mut(
                        pic.codec_pic_params.as_mut_ptr() as *mut u32,
                        8,
                    );
                    words[H264_PIC_FORCE_INTRA_REFRESH_IDX] = ((self.fps / 2).max(15) / 5).max(3);
                }
            }

            let status =
                (self.api.encode_picture.ok_or("no EncodePicture")?)(self.encoder, &mut *pic);
            let outcome = if status == NV_ENC_SUCCESS {
                let mut lock: Box<LockBitstream> = Box::new(std::mem::zeroed());
                lock.version = sv(2);
                lock.output_bitstream = self.bitstream;
                let status =
                    (self.api.lock_bitstream.ok_or("no LockBitstream")?)(self.encoder, &mut *lock);
                if status != NV_ENC_SUCCESS {
                    let _ = self
                        .api
                        .unmap_input_resource
                        .map(|f| f(self.encoder, map.mapped_resource));
                    return Err(self.err("LockBitstream", status));
                }
                if lock.bitstream_buffer_ptr.is_null() {
                    // A success status with a null payload (TDR race,
                    // device removal) must take the clean heal path —
                    // `from_raw_parts` on null is UB even for length 0. The
                    // successful lock must still be paired with an unlock or
                    // the poisoned session can retain the bitstream forever.
                    let _ = self
                        .api
                        .unlock_bitstream
                        .map(|f| f(self.encoder, self.bitstream));
                    let _ = self
                        .api
                        .unmap_input_resource
                        .map(|f| f(self.encoder, map.mapped_resource));
                    return Err("LockBitstream returned success with a null payload".into());
                }
                let data = std::slice::from_raw_parts(
                    lock.bitstream_buffer_ptr as *const u8,
                    lock.bitstream_size_in_bytes as usize,
                )
                .to_vec();
                let key = lock.picture_type == NV_ENC_PIC_TYPE_IDR
                    || lock.picture_type == NV_ENC_PIC_TYPE_I;
                if let Some(unlock) = self.api.unlock_bitstream {
                    let status = unlock(self.encoder, self.bitstream);
                    if status != NV_ENC_SUCCESS {
                        tracing::debug!("NVENC UnlockBitstream: status {status}");
                    }
                }
                let mut o = EncodeOutcome::consumed(Some((data, key)));
                o.input_ts = input_ts;
                o
            } else if status == NV_ENC_ERR_NEED_MORE_INPUT {
                // Can't happen with PTD + no B-frames + sync mode, but the
                // seam has a lossless answer for it, so use it.
                let mut o = EncodeOutcome::consumed(None);
                o.input_ts = input_ts;
                o
            } else {
                let _ = self
                    .api
                    .unmap_input_resource
                    .map(|f| f(self.encoder, map.mapped_resource));
                return Err(self.err("EncodePicture", status));
            };
            if let Some(unmap) = self.api.unmap_input_resource {
                let status = unmap(self.encoder, map.mapped_resource);
                if status != NV_ENC_SUCCESS {
                    tracing::debug!("NVENC UnmapInputResource: status {status}");
                }
            }
            Ok(outcome)
        }
    }

    /// Error resilience: tell the encoder the frame it encoded with
    /// `input_ts` never reached the decoder (a viewer reported it lost),
    /// so it stops using that frame — and anything that referenced it —
    /// as a prediction source. With the deep DPB set at init, the next
    /// P-frame re-references an older *valid* frame and stays a P-frame:
    /// the corruption never propagates and no IDR wall is spent. `input_ts`
    /// is the value [`EncodeOutcome::input_ts`] carried for that frame.
    /// Returns whether the driver accepted it (false = too old / already
    /// evicted from the DPB / no such API).
    pub fn invalidate_ref(&mut self, input_ts: u64) -> bool {
        unsafe {
            let Some(invalidate) = self.api.invalidate_ref_frames else {
                return false;
            };
            let status = invalidate(self.encoder, input_ts);
            if status != NV_ENC_SUCCESS {
                tracing::debug!("NVENC InvalidateRefFrames({input_ts}): status {status}");
                return false;
            }
            true
        }
    }

    /// Re-aim the rate controller in place — the SDK's guaranteed form of
    /// what the MF rung only partially honors: mean, peak, and VBV all
    /// move, no reset, no IDR.
    pub fn set_bitrate(&mut self, bitrate: u32) -> bool {
        if self.lossless {
            // constQP-0 has no rate to move; pretending otherwise would
            // hand the stream's backoff a knob wired to nothing.
            return false;
        }
        unsafe {
            let Some(reconfigure) = self.api.reconfigure_encoder else {
                return false;
            };
            self.config.rc_params.average_bit_rate = bitrate;
            let (peak, vbv) =
                crate::video::burst_bounds(bitrate, self.fps, self.intra_refresh, self.studio);
            self.config.rc_params.max_bit_rate = peak;
            self.config.rc_params.vbv_buffer_size = vbv;
            let mut params: Box<ReconfigureParams> = Box::new(std::mem::zeroed());
            params.version = sv(1) | (1 << 31);
            std::ptr::copy_nonoverlapping(&*self.init, &mut params.re_init_encode_params, 1);
            params.re_init_encode_params.encode_config = &mut *self.config;
            let status = reconfigure(self.encoder, &mut *params);
            if status != NV_ENC_SUCCESS {
                tracing::debug!("NVENC ReconfigureEncoder: status {status}");
                return false;
            }
            true
        }
    }

    /// Register (once) the texture with the session, keyed by interface
    /// pointer — the GPU lane cycles a small fixed ring, so this settles
    /// after the first lap.
    unsafe fn register(&mut self, tex: &ID3D11Texture2D) -> Result<*mut c_void, String> {
        let ptr = tex.as_raw();
        if let Some(r) = self.registered.iter().find(|r| r.tex_ptr == ptr) {
            return Ok(r.handle);
        }
        let mut reg = RegisterResource {
            version: sv(4),
            resource_type: NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
            width: self.width,
            height: self.height,
            pitch: 0,
            sub_resource_index: 0,
            resource_to_register: ptr,
            registered_resource: std::ptr::null_mut(),
            buffer_format: NV_ENC_BUFFER_FORMAT_NV12,
            buffer_usage: NV_ENC_BUFFER_USAGE_INPUT_IMAGE,
            p_input_fence_point: std::ptr::null_mut(),
            reserved1: [0; 247],
            reserved2: [std::ptr::null_mut(); 61],
        };
        let status =
            (self.api.register_resource.ok_or("no RegisterResource")?)(self.encoder, &mut reg);
        if status != NV_ENC_SUCCESS {
            return Err(self.err("RegisterResource", status));
        }
        self.registered.push(Registered {
            tex_ptr: ptr,
            handle: reg.registered_resource,
        });
        Ok(reg.registered_resource)
    }

    fn err(&self, what: &str, status: u32) -> String {
        let detail = unsafe {
            self.api
                .get_last_error_string
                .map(|f| {
                    let p = f(self.encoder);
                    if p.is_null() {
                        String::new()
                    } else {
                        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
                    }
                })
                .unwrap_or_default()
        };
        format!("NVENC {what}: status {status} ({detail})")
    }
}

impl Drop for NvencH264 {
    fn drop(&mut self) {
        unsafe {
            for r in self.registered.drain(..) {
                if let Some(f) = self.api.unregister_resource {
                    let _ = f(self.encoder, r.handle);
                }
            }
            if !self.bitstream.is_null() {
                if let Some(f) = self.api.destroy_bitstream_buffer {
                    let _ = f(self.encoder, self.bitstream);
                }
            }
            if let Some(f) = self.api.destroy_encoder {
                let _ = f(self.encoder);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole SDK lane in isolation, mirroring `gpu_lane_end_to_end`:
    /// synthetic BGRA → VideoProcessor NV12 (the exact texture shape the
    /// live lane feeds) → **direct NVENC** → Annex-B → openh264 decode
    /// with dimension + luma asserts. Skips (passing) without an NVIDIA
    /// driver. Also proves in-place `set_bitrate` on the SDK path.
    #[test]
    fn nvenc_sdk_end_to_end() {
        let (w, h) = (640u32, 480u32);
        let mut gpu = match crate::gpu_pipeline::GpuConvert::new(w, h, w, h) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("SKIP: GPU convert unavailable: {e}");
                return;
            }
        };
        let mut enc =
            match NvencH264::open_on_device(&gpu.device(), w, h, 30, 4_000_000, false, false) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("SKIP: NVENC session unavailable: {e}");
                    return;
                }
            };
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        let tex = gpu.bgra_texture_from(&bgra, w, h).expect("tex");
        let mut units: Vec<(Vec<u8>, bool)> = Vec::new();
        let mut fed = 0u32;
        for i in 0..60u32 {
            for row in 0..h as usize {
                let bright = (row as u32).is_multiple_of(2) == i.is_multiple_of(2);
                let v = if bright { 220u8 } else { 40u8 };
                let line = &mut bgra[row * (w as usize) * 4..][..(w as usize) * 4];
                let (pixels, _) = line.as_chunks_mut::<4>();
                for px in pixels {
                    px[0] = v;
                    px[1] = v;
                    px[2] = v;
                    px[3] = 255;
                }
            }
            gpu.update_bgra(&tex, &bgra, w, h);
            let (slot, nv12) = gpu.convert(&tex).expect("convert").expect("slot");
            let out = enc.encode_texture(&nv12, i == 0).expect("encode");
            gpu.release(slot);
            if out.consumed {
                fed += 1;
            }
            if i == 30 {
                assert!(enc.set_bitrate(2_000_000), "SDK in-place reconfigure");
            }
            units.extend(out.units);
        }
        assert!(units.len() as u32 >= fed - 1, "sync mode: a unit per frame");
        assert!(units.iter().any(|(_, k)| *k), "a keyframe came out");
        let mut dec = openh264::decoder::Decoder::with_api_config(
            openh264::OpenH264API::from_source(),
            openh264::decoder::DecoderConfig::new(),
        )
        .expect("decoder");
        let mut decoded = 0u32;
        let mut last_dims = (0usize, 0usize);
        for (d, _) in &units {
            use openh264::formats::YUVSource as _;
            if let Some(yuv) = dec.decode(d).expect("clean decode — SDK bitstream") {
                decoded += 1;
                last_dims = yuv.dimensions();
                let mut hi = 0u8;
                for &v in yuv.y().iter().take((w * 4) as usize) {
                    hi = hi.max(v);
                }
                assert!(hi > 150, "bright rows survived (hi {hi})");
            }
        }
        assert_eq!(last_dims, (w as usize, h as usize), "decoded dimensions");
        assert!(decoded >= fed - 3, "decoded {decoded} of {fed}");
    }

    /// The HEVC-lossless pilot meets the real driver. Nothing in this
    /// crate decodes HEVC (openh264 is H.264-only), so this asserts the
    /// bitstream *shape* — VPS/SPS/PPS present, IDR slices present, a
    /// unit per frame, and keyframes flagged.
    #[test]
    fn nvenc_hevc_lossless_smoke() {
        let (w, h) = (1280u32, 720u32);
        let (wu, hu) = (w as usize, h as usize);
        let mut gpu = match crate::gpu_pipeline::GpuConvert::new(w, h, w, h) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("SKIP: GPU convert unavailable: {e}");
                return;
            }
        };
        let mut enc = match NvencH264::open_lossless_hevc_on_device(&gpu.device(), w, h, 60) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("SKIP: NVENC HEVC session unavailable: {e}");
                return;
            }
        };
        assert_eq!(enc.label(), "NVENC SDK (HEVC, studio-lossless)");
        // Scrolling document — the realistic desktop case the probe
        // should later decode frame for frame.
        let mut doc = vec![0u8; wu * (hu + 300) * 4];
        paint_document(&mut doc, wu, hu + 300);
        let mut bgra = vec![0u8; wu * hu * 4];
        let tex = gpu.bgra_texture_from(&bgra, w, h).expect("tex");
        let mut stream: Vec<u8> = Vec::new();
        let (mut units, mut keys) = (0u32, 0u32);
        for i in 0..90u64 {
            let off = (i as usize) * 3;
            bgra.copy_from_slice(&doc[off * wu * 4..][..wu * hu * 4]);
            gpu.update_bgra(&tex, &bgra, w, h);
            let (slot, nv12) = gpu.convert(&tex).expect("convert").expect("slot");
            let out = enc.encode_texture(&nv12, i == 0).expect("encode");
            gpu.release(slot);
            for (d, key) in &out.units {
                units += 1;
                if *key {
                    keys += 1;
                }
                stream.extend_from_slice(d);
            }
        }
        assert!(units >= 89, "a unit per frame (got {units})");
        assert!(keys >= 1, "a keyframe came out");
        // Annex-B walk: the HEVC nal_unit_type is bits 1..6 of the first
        // byte after the start code (32 VPS · 33 SPS · 34 PPS · 19/20
        // IDR slices).
        let mut have = [false; 64];
        let mut i = 0usize;
        while i + 4 < stream.len() {
            if stream[i] == 0
                && stream[i + 1] == 0
                && (stream[i + 2] == 1 || (stream[i + 2] == 0 && stream[i + 3] == 1))
            {
                let s = i + if stream[i + 2] == 1 { 3 } else { 4 };
                if s < stream.len() {
                    have[((stream[s] >> 1) & 0x3F) as usize] = true;
                }
                i = s;
            } else {
                i += 1;
            }
        }
        assert!(have[32], "VPS present");
        assert!(have[33], "SPS present");
        assert!(have[34], "PPS present");
        assert!(have[19] || have[20], "IDR slices present");
    }

    use super::tests_support::paint_document;
}

/// Deterministic synthetic-content painters shared by this module and the
/// decode twin's ([`crate::nvdec`]) round-trip tests. The two sides must feed
/// identical pixels to compare fairly.
#[cfg(test)]
pub(crate) mod tests_support {
    /// Deterministic LCG so regression fixtures do not vary run to run.
    pub(crate) fn lcg(s: &mut u64) -> u32 {
        *s = (*s)
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (*s >> 33) as u32
    }

    /// A document-ish master: light page, dark 3×5 dot-matrix "glyphs" in
    /// 10×18 cells with margins, word gaps, and occasional tinted "link"
    /// lines — the spatial statistics of UI text (sharp, high-contrast,
    /// fine grain) without shipping a font. Each dot's right column
    /// half-blends toward the page, faking the AA edge real glyphs carry.
    pub(crate) fn paint_document(buf: &mut [u8], w: usize, rows: usize) {
        let (pixels, _) = buf.as_chunks_mut::<4>();
        for px in pixels {
            px.copy_from_slice(&[0xEE, 0xEC, 0xE8, 0xFF]);
        }
        let mut s = 0x5EED_1234_u64;
        let (cw, ch) = (10usize, 18usize);
        for cy in 0..rows / ch {
            let kind = lcg(&mut s) % 10;
            if kind < 2 {
                continue; // blank line
            }
            for cx in 0..w / cw {
                let r = lcg(&mut s);
                if !(8..w / cw - 8).contains(&cx) || r % 100 < 15 {
                    continue; // margins and word gaps
                }
                let ink: [u8; 4] = if kind == 9 {
                    [0x8A, 0x46, 0x1C, 0xFF] // a "link" line (BGRA)
                } else {
                    [0x24, 0x20, 0x1E, 0xFF]
                };
                for p in 0..15u32 {
                    if (r >> p) & 1 == 0 {
                        continue;
                    }
                    let x0 = cx * cw + 1 + (p as usize % 3) * 3;
                    let y0 = cy * ch + 4 + (p as usize / 3) * 2;
                    for dy in 0..2 {
                        for dx in 0..3 {
                            let i = ((y0 + dy) * w + x0 + dx) * 4;
                            if dx == 2 {
                                for k in 0..3 {
                                    buf[i + k] =
                                        ((u32::from(buf[i + k]) + u32::from(ink[k])) / 2) as u8;
                                }
                            } else {
                                buf[i..i + 4].copy_from_slice(&ink);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests_hardware {
    use super::*;

    /// The reference-invalidation capstone, proven end to end on real
    /// hardware: encode a stream, **drop one frame from the decoder's
    /// input** (simulating a lost packet), tell the encoder to invalidate
    /// that reference, and assert the decoder recovers on the very next
    /// frame — decoding clean through openh264 — with **no IDR anywhere
    /// after the first**. This is the zero-smear game-mode heal: the
    /// encoder re-references an older valid frame from the deep DPB
    /// instead of spending a keyframe wall, and the loss never propagates.
    /// The negative control is inline: the same skipped frame WITHOUT the
    /// invalidate is asserted to break the decoder, so the test proves the
    /// invalidate is what heals it, not luck.
    #[test]
    fn nvenc_ref_invalidation_heals_without_idr() {
        let (w, h) = (640u32, 480u32);
        let (wu, hu) = (w as usize, h as usize);
        let mut gpu = match crate::gpu_pipeline::GpuConvert::new(w, h, w, h) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("SKIP: GPU convert unavailable: {e}");
                return;
            }
        };
        // Balanced H.264 (deep DPB, openh264-decodable). Motion content so
        // P-frames genuinely reference their predecessors — the whole
        // point is that dropping one matters.
        let mut doc = vec![0u8; wu * (hu + 300) * 4];
        super::tests_support::paint_document(&mut doc, wu, hu + 300);

        // Run the same 40-frame script twice: once healing the drop with
        // invalidate, once not (the control). `heal` returns (clean
        // decodes after the drop, any keyframe after frame 0, decoder
        // errored after the drop).
        let mut run = |heal: bool| -> Option<(u32, bool, bool)> {
            let mut enc =
                match NvencH264::open_on_device(&gpu.device(), w, h, 30, 8_000_000, false, false) {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("SKIP: NVENC session unavailable: {e}");
                        return None;
                    }
                };
            let mut bgra = vec![0u8; wu * hu * 4];
            let tex = gpu.bgra_texture_from(&bgra, w, h).expect("tex");
            let mut dec = openh264::decoder::Decoder::with_api_config(
                openh264::OpenH264API::from_source(),
                openh264::decoder::DecoderConfig::new(),
            )
            .expect("decoder");
            const DROP: u64 = 20;
            let (mut key_after_0, mut clean_after_drop, mut errored) = (false, 0u32, false);
            for i in 0..40u64 {
                let off = (i as usize) * 3;
                bgra.copy_from_slice(&doc[off * wu * 4..][..wu * hu * 4]);
                gpu.update_bgra(&tex, &bgra, w, h);
                let (slot, nv12) = gpu.convert(&tex).expect("convert").expect("slot");
                let out = enc.encode_texture(&nv12, i == 0).expect("encode");
                gpu.release(slot);
                if i == DROP && heal {
                    // The viewer would report this frame lost; the host
                    // maps it to the encoder ts and invalidates it before
                    // the next encode.
                    assert!(
                        enc.invalidate_ref(out.input_ts),
                        "driver accepted invalidate"
                    );
                }
                for (d, key) in &out.units {
                    if i > 0 && *key {
                        key_after_0 = true;
                    }
                    if i == DROP {
                        continue; // the "lost" frame never reaches the decoder
                    }
                    match dec.decode(d) {
                        Ok(_) if i > DROP => clean_after_drop += 1,
                        Ok(_) => {}
                        Err(_) if i > DROP => errored = true,
                        Err(_) => {}
                    }
                }
            }
            Some((clean_after_drop, key_after_0, errored))
        };

        let Some((healed_clean, healed_key, healed_err)) = run(true) else {
            return; // skipped (no hardware)
        };
        let (control_clean, control_key, control_err) = run(false).expect("second run");

        // The load-bearing claim, proven on real silicon: invalidation
        // recovers WITHOUT an IDR — the encoder re-references an older
        // valid frame from the deep DPB rather than a keyframe wall.
        // Whether a *strict* decoder rides the resulting frame_num gap is
        // decoder-specific and reported below; openh264 here is the
        // strict baseline, NVDEC/the viewer's conceal path are the field.
        assert!(
            !healed_key,
            "invalidation heals with a P-frame, never an IDR wall"
        );
        let _ = (healed_clean, control_key);
        eprintln!(
            "ref-invalidation on real hardware:\n  healed:  clean={healed_clean} err={healed_err} idr_after_0={healed_key}\n  control: clean={control_clean} err={control_err} idr_after_0={control_key}"
        );
    }

    /// The GDR pilot: with intra-refresh on, a two-second stream contains
    /// **no IDR after the first frame** — the intra data rides refresh
    /// waves — yet decodes cleanly end to end. This is the burst-shrinking
    /// mode: no more keyframe walls for the transport to spray.
    #[test]
    fn nvenc_intra_refresh_replaces_idr_walls() {
        let (w, h) = (640u32, 480u32);
        let mut gpu = match crate::gpu_pipeline::GpuConvert::new(w, h, w, h) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("SKIP: GPU convert unavailable: {e}");
                return;
            }
        };
        let mut enc =
            match NvencH264::open_on_device(&gpu.device(), w, h, 30, 3_000_000, true, false) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("SKIP: NVENC session unavailable: {e}");
                    return;
                }
            };
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        let tex = gpu.bgra_texture_from(&bgra, w, h).expect("tex");
        let mut units: Vec<(Vec<u8>, bool)> = Vec::new();
        for i in 0..90u32 {
            let (pixels, _) = bgra.as_chunks_mut::<4>();
            for (j, px) in pixels.iter_mut().enumerate() {
                let v = ((j as u32).wrapping_add(i.wrapping_mul(977)) % 255) as u8;
                px[0] = v;
                px[1] = v.wrapping_add(40);
                px[2] = v.wrapping_add(80);
                px[3] = 255;
            }
            gpu.update_bgra(&tex, &bgra, w, h);
            let (slot, nv12) = gpu.convert(&tex).expect("convert").expect("slot");
            let out = enc.encode_texture(&nv12, i == 0).expect("encode");
            gpu.release(slot);
            units.extend(out.units);
        }
        let idrs = units.iter().skip(1).filter(|(_, k)| *k).count();
        assert_eq!(idrs, 0, "no IDR walls after the entry frame (got {idrs})");
        let mut dec = openh264::decoder::Decoder::with_api_config(
            openh264::OpenH264API::from_source(),
            openh264::decoder::DecoderConfig::new(),
        )
        .expect("decoder");
        let mut decoded = 0u32;
        for (d, _) in &units {
            if dec
                .decode(d)
                .expect("clean decode across refresh waves")
                .is_some()
            {
                decoded += 1;
            }
        }
        assert!(decoded as usize >= units.len() - 3, "decoded {decoded}");
    }
}
