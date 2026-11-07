//! The NVENC API 12.1 structs and entry points used here, laid out as nvEncodeAPI.h
//! (nv-codec-headers n12.1.14.0) has them, and the CUDA driver calls that give a session
//! its context. Both libraries are loaded at runtime.

use libloading::Library;
use std::ffi::{c_char, c_int, c_void};

#[cfg(windows)]
const LIB_ENCODE: &str = "nvEncodeAPI64.dll";
#[cfg(not(windows))]
const LIB_ENCODE: &str = "libnvidia-encode.so.1";
#[cfg(windows)]
const LIB_CUDA: &str = "nvcuda.dll";
#[cfg(not(windows))]
const LIB_CUDA: &str = "libcuda.so.1";

pub const API_MAJOR: u32 = 12;
pub const API_MINOR: u32 = 1;
pub const API_VERSION: u32 = API_MAJOR | (API_MINOR << 24);
pub const fn struct_version(v: u32) -> u32 {
    API_VERSION | (v << 16) | (0x7 << 28)
}
/// The versions whose header macro also sets the top bit.
const fn struct_version_ext(v: u32) -> u32 {
    struct_version(v) | (1 << 31)
}
pub const CONFIG_VER: u32 = struct_version_ext(8);
pub const INITIALIZE_PARAMS_VER: u32 = struct_version_ext(6);
pub const RECONFIGURE_PARAMS_VER: u32 = struct_version_ext(1);
pub const PRESET_CONFIG_VER: u32 = struct_version_ext(4);
pub const PIC_PARAMS_VER: u32 = struct_version_ext(6);
pub const LOCK_BITSTREAM_VER: u32 = struct_version_ext(1);
pub const LOCK_INPUT_BUFFER_VER: u32 = struct_version(1);
pub const CREATE_INPUT_BUFFER_VER: u32 = struct_version(1);
pub const CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
pub const RC_PARAMS_VER: u32 = struct_version(1);
pub const CAPS_PARAM_VER: u32 = struct_version(1);
const OPEN_PARAMS_VER: u32 = struct_version(1);
const FUNCTION_LIST_VER: u32 = struct_version(2);

const DEVICE_TYPE_CUDA: u32 = 1;
pub const INFINITE_GOP: u32 = u32::MAX;
pub const TUNING_ULTRA_LOW_LATENCY: u32 = 3;
pub const RC_CONSTQP: u32 = 0;
pub const RC_CBR: u32 = 2;
pub const PIC_STRUCT_FRAME: u32 = 1;
pub const PIC_FLAG_FORCEIDR: u32 = 0x2;
pub const PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x4;
pub const NUM_REF_FRAMES_1: u32 = 1;

pub const FMT_NV12: u32 = 0x1;
pub const FMT_IYUV: u32 = 0x100;
pub const FMT_YUV444: u32 = 0x1000;
pub const FMT_YUV420_10BIT: u32 = 0x10000;

/// Bits of `RcParams::flags`.
pub const RC_ENABLE_LOOKAHEAD: u32 = 1 << 5;
pub const RC_ZERO_REORDER_DELAY: u32 = 1 << 9;
/// Bits of `H264Config::flags`.
pub const H264_DISABLE_SPSPPS: u32 = 1 << 7;
pub const H264_INTRA_REFRESH: u32 = 1 << 10;
pub const H264_REPEAT_SPSPPS: u32 = 1 << 12;
/// Bits of `HevcConfig::flags`.
pub const HEVC_DISABLE_SPSPPS: u32 = 1 << 6;
pub const HEVC_REPEAT_SPSPPS: u32 = 1 << 7;
pub const HEVC_INTRA_REFRESH: u32 = 1 << 8;
/// Bits of `Av1Config::flags`.
pub const AV1_ANNEXB: u32 = 1 << 0;
pub const AV1_DISABLE_SEQ_HDR: u32 = 1 << 4;
pub const AV1_REPEAT_SEQ_HDR: u32 = 1 << 5;
pub const AV1_INTRA_REFRESH: u32 = 1 << 6;
/// Bits of `Reconfigure::flags`.
pub const RECONFIGURE_RESET: u32 = 1 << 0;
pub const RECONFIGURE_FORCE_IDR: u32 = 1 << 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Guid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

pub const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> Guid {
    Guid { d1, d2, d3, d4 }
}

pub const CODEC_H264: Guid = guid(
    0x6bc82762,
    0x4e63,
    0x4ca4,
    [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
);
pub const CODEC_HEVC: Guid = guid(
    0x790cdc88,
    0x4522,
    0x4d7b,
    [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
);
pub const CODEC_AV1: Guid = guid(
    0x0a352289,
    0x0aa7,
    0x4759,
    [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54],
);
pub const PRESET_P4: Guid = guid(
    0x90a7b826,
    0xdf06,
    0x4862,
    [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81],
);

pub type Status = c_int;
pub type Enc = *mut c_void;
type Ptr = *mut c_void;

/// NVENCSTATUS names, by value.
const STATUS: [&str; 27] = [
    "SUCCESS",
    "NO_ENCODE_DEVICE",
    "UNSUPPORTED_DEVICE",
    "INVALID_ENCODERDEVICE",
    "INVALID_DEVICE",
    "DEVICE_NOT_EXIST",
    "INVALID_PTR",
    "INVALID_EVENT",
    "INVALID_PARAM",
    "INVALID_CALL",
    "OUT_OF_MEMORY",
    "ENCODER_NOT_INITIALIZED",
    "UNSUPPORTED_PARAM",
    "LOCK_BUSY",
    "NOT_ENOUGH_BUFFER",
    "INVALID_VERSION",
    "MAP_FAILED",
    "NEED_MORE_INPUT",
    "ENCODER_BUSY",
    "EVENT_NOT_REGISTERD",
    "GENERIC",
    "INCOMPATIBLE_CLIENT_KEY",
    "UNIMPLEMENTED",
    "RESOURCE_REGISTER_FAILED",
    "RESOURCE_NOT_REGISTERED",
    "RESOURCE_NOT_MAPPED",
    "NEED_MORE_OUTPUT",
];

pub fn status_name(s: Status) -> &'static str {
    usize::try_from(s)
        .ok()
        .and_then(|i| STATUS.get(i))
        .copied()
        .unwrap_or("unknown status")
}

/// A C string of at most `max` bytes.
pub unsafe fn c_string(s: *const c_char, max: usize) -> String {
    let mut n = 0;
    while n < max && *s.add(n) != 0 {
        n += 1;
    }
    String::from_utf8_lossy(std::slice::from_raw_parts(s as *const u8, n)).into_owned()
}

#[repr(C)]
pub struct CapsParam {
    pub version: u32,
    pub caps: u32,
    pub reserved: [u32; 62],
}

#[repr(C)]
pub struct OpenParams {
    pub version: u32,
    pub device_type: u32,
    pub device: Ptr,
    pub reserved: Ptr,
    pub api_version: u32,
    pub reserved1: [u32; 253],
    pub reserved2: [Ptr; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Qp {
    pub inter_p: u32,
    pub inter_b: u32,
    pub intra: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RcParams {
    pub version: u32,
    pub mode: u32,
    pub const_qp: Qp,
    pub average_bit_rate: u32,
    pub max_bit_rate: u32,
    pub vbv_buffer_size: u32,
    pub vbv_initial_delay: u32,
    pub flags: u32,
    pub min_qp: Qp,
    pub max_qp: Qp,
    pub initial_rc_qp: Qp,
    pub temporal_layer_idx_mask: u32,
    pub temporal_layer_qp: [u8; 8],
    pub target_quality: u8,
    pub target_quality_lsb: u8,
    pub lookahead_depth: u16,
    pub low_delay_key_frame_scale: u8,
    pub y_dc_qp_index_offset: i8,
    pub u_dc_qp_index_offset: i8,
    pub v_dc_qp_index_offset: i8,
    pub qp_map_mode: u32,
    pub multi_pass: u32,
    pub alpha_layer_bitrate_ratio: u32,
    pub cb_qp_index_offset: i8,
    pub cr_qp_index_offset: i8,
    pub reserved2: u16,
    pub reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Vui {
    pub overscan_info_present_flag: u32,
    pub overscan_info: u32,
    pub video_signal_type_present_flag: u32,
    pub video_format: u32,
    pub video_full_range_flag: u32,
    pub colour_description_present_flag: u32,
    pub colour_primaries: u32,
    pub transfer_characteristics: u32,
    pub colour_matrix: u32,
    pub chroma_sample_location_flag: u32,
    pub chroma_sample_location_top: u32,
    pub chroma_sample_location_bot: u32,
    pub bitstream_restriction_flag: u32,
    pub timing_info_present_flag: u32,
    pub num_unit_in_ticks: u32,
    pub time_scale: u32,
    pub reserved: [u32; 12],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct H264Config {
    pub flags: u32,
    pub level: u32,
    pub idr_period: u32,
    pub separate_colour_plane_flag: u32,
    pub disable_deblocking_filter_idc: u32,
    pub num_temporal_layers: u32,
    pub sps_id: u32,
    pub pps_id: u32,
    pub adaptive_transform_mode: u32,
    pub fmo_mode: u32,
    pub bdirect_mode: u32,
    pub entropy_coding_mode: u32,
    pub stereo_mode: u32,
    pub intra_refresh_period: u32,
    pub intra_refresh_cnt: u32,
    pub max_num_ref_frames: u32,
    pub slice_mode: u32,
    pub slice_mode_data: u32,
    pub vui: Vui,
    pub ltr_num_frames: u32,
    pub ltr_trust_mode: u32,
    pub chroma_format_idc: u32,
    pub max_temporal_layers: u32,
    pub use_bframes_as_ref: u32,
    pub num_ref_l0: u32,
    pub num_ref_l1: u32,
    pub reserved1: [u32; 267],
    pub reserved2: [Ptr; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HevcConfig {
    pub level: u32,
    pub tier: u32,
    pub min_cu_size: u32,
    pub max_cu_size: u32,
    pub flags: u32,
    pub idr_period: u32,
    pub intra_refresh_period: u32,
    pub intra_refresh_cnt: u32,
    pub max_num_ref_frames_in_dpb: u32,
    pub ltr_num_frames: u32,
    pub vps_id: u32,
    pub sps_id: u32,
    pub pps_id: u32,
    pub slice_mode: u32,
    pub slice_mode_data: u32,
    pub max_temporal_layers_minus1: u32,
    pub vui: Vui,
    pub ltr_trust_mode: u32,
    pub use_bframes_as_ref: u32,
    pub num_ref_l0: u32,
    pub num_ref_l1: u32,
    pub reserved1: [u32; 214],
    pub reserved2: [Ptr; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Av1Config {
    pub level: u32,
    pub tier: u32,
    pub min_part_size: u32,
    pub max_part_size: u32,
    pub flags: u32,
    pub idr_period: u32,
    pub intra_refresh_period: u32,
    pub intra_refresh_cnt: u32,
    pub max_num_ref_frames_in_dpb: u32,
    pub num_tile_columns: u32,
    pub num_tile_rows: u32,
    pub tile_widths: *mut u32,
    pub tile_heights: *mut u32,
    pub max_temporal_layers_minus1: u32,
    pub color_primaries: u32,
    pub transfer_characteristics: u32,
    pub matrix_coefficients: u32,
    pub color_range: u32,
    pub chroma_sample_position: u32,
    pub use_bframes_as_ref: u32,
    pub film_grain_params: Ptr,
    pub num_fwd_refs: u32,
    pub num_bwd_refs: u32,
    pub reserved1: [u32; 235],
    pub reserved2: [Ptr; 62],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union CodecConfig {
    pub h264: H264Config,
    pub hevc: HevcConfig,
    pub av1: Av1Config,
    pub reserved: [u32; 320],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Config {
    pub version: u32,
    pub profile_guid: Guid,
    pub gop_length: u32,
    pub frame_interval_p: i32,
    pub mono_chrome_encoding: u32,
    pub frame_field_mode: u32,
    pub mv_precision: u32,
    pub rc: RcParams,
    pub codec: CodecConfig,
    pub reserved: [u32; 278],
    pub reserved2: [Ptr; 64],
}

/// NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE.
type MeHintCounts = [u32; 4];

#[repr(C)]
#[derive(Clone, Copy)]
pub struct InitParams {
    pub version: u32,
    pub encode_guid: Guid,
    pub preset_guid: Guid,
    pub encode_width: u32,
    pub encode_height: u32,
    pub dar_width: u32,
    pub dar_height: u32,
    pub frame_rate_num: u32,
    pub frame_rate_den: u32,
    pub enable_encode_async: u32,
    pub enable_ptd: u32,
    pub flags: u32,
    pub priv_data_size: u32,
    pub priv_data: Ptr,
    pub encode_config: *mut Config,
    pub max_encode_width: u32,
    pub max_encode_height: u32,
    pub max_me_hint_counts_per_block: [MeHintCounts; 2],
    pub tuning_info: u32,
    pub buffer_format: u32,
    pub num_state_buffers: u32,
    pub output_stats_level: u32,
    pub reserved: [u32; 285],
    pub reserved2: [Ptr; 64],
}

// Their pointers stay null except inside a driver call, which points them at its own copies.
unsafe impl Send for Config {}
unsafe impl Send for InitParams {}

#[repr(C)]
pub struct Reconfigure {
    pub version: u32,
    pub reinit: InitParams,
    pub flags: u32,
}

#[repr(C)]
pub struct PresetConfig {
    pub version: u32,
    pub preset_cfg: Config,
    pub reserved1: [u32; 255],
    pub reserved2: [Ptr; 64],
}

#[repr(C)]
pub struct CreateInputBuffer {
    pub version: u32,
    pub width: u32,
    pub height: u32,
    pub memory_heap: u32,
    pub buffer_fmt: u32,
    pub reserved: u32,
    pub input_buffer: Ptr,
    pub sys_mem_buffer: Ptr,
    pub reserved1: [u32; 57],
    pub reserved2: [Ptr; 63],
}

#[repr(C)]
pub struct CreateBitstreamBuffer {
    pub version: u32,
    pub size: u32,
    pub memory_heap: u32,
    pub reserved: u32,
    pub bitstream_buffer: Ptr,
    pub bitstream_buffer_ptr: Ptr,
    pub reserved1: [u32; 58],
    pub reserved2: [Ptr; 64],
}

#[repr(C)]
pub struct PicParams {
    pub version: u32,
    pub input_width: u32,
    pub input_height: u32,
    pub input_pitch: u32,
    pub encode_pic_flags: u32,
    pub frame_idx: u32,
    pub input_time_stamp: u64,
    pub input_duration: u64,
    pub input_buffer: Ptr,
    pub output_bitstream: Ptr,
    pub completion_event: Ptr,
    pub buffer_fmt: u32,
    pub picture_struct: u32,
    pub picture_type: u32,
    pub codec_pic_params: [u64; 194],
    pub me_hint_counts_per_block: [MeHintCounts; 2],
    pub me_external_hints: Ptr,
    pub reserved1: [u32; 6],
    pub reserved2: [Ptr; 2],
    pub qp_delta_map: *mut i8,
    pub qp_delta_map_size: u32,
    pub reserved_bit_fields: u32,
    pub me_hint_ref_pic_dist: [u16; 2],
    pub alpha_buffer: Ptr,
    pub me_external_sb_hints: Ptr,
    pub me_sb_hints_count: u32,
    pub state_buffer_idx: u32,
    pub output_recon_buffer: Ptr,
    pub reserved3: [u32; 284],
    pub reserved4: [Ptr; 57],
}

#[repr(C)]
pub struct LockBitstream {
    pub version: u32,
    pub flags: u32,
    pub output_bitstream: Ptr,
    pub slice_offsets: *mut u32,
    pub frame_idx: u32,
    pub hw_encode_status: u32,
    pub num_slices: u32,
    pub bitstream_size_in_bytes: u32,
    pub output_time_stamp: u64,
    pub output_duration: u64,
    pub bitstream_buffer_ptr: Ptr,
    pub picture_type: u32,
    pub picture_struct: u32,
    pub frame_avg_qp: u32,
    pub frame_satd: u32,
    pub ltr_frame_idx: u32,
    pub ltr_frame_bitmap: u32,
    pub temporal_id: u32,
    pub intra_mb_count: u32,
    pub inter_mb_count: u32,
    pub average_mvx: i32,
    pub average_mvy: i32,
    pub alpha_layer_size_in_bytes: u32,
    pub output_stats_ptr_size: u32,
    pub output_stats_ptr: Ptr,
    pub frame_idx_display: u32,
    pub reserved1: [u32; 220],
    pub reserved2: [Ptr; 63],
    pub reserved_internal: [u32; 8],
}

#[repr(C)]
pub struct LockInputBuffer {
    pub version: u32,
    pub flags: u32,
    pub input_buffer: Ptr,
    pub buffer_data_ptr: Ptr,
    pub pitch: u32,
    pub reserved1: [u32; 251],
    pub reserved2: [Ptr; 64],
}

type Unused = Option<unsafe extern "system" fn()>;
type Call<T> = Option<unsafe extern "system" fn(Enc, T) -> Status>;
type CodecCount = Option<unsafe extern "system" fn(Enc, Guid, *mut u32) -> Status>;
type Guids = Option<unsafe extern "system" fn(Enc, *mut Guid, u32, *mut u32) -> Status>;
type CodecGuids = Option<unsafe extern "system" fn(Enc, Guid, *mut Guid, u32, *mut u32) -> Status>;
type Formats = Option<unsafe extern "system" fn(Enc, Guid, *mut u32, u32, *mut u32) -> Status>;
type Caps = Option<unsafe extern "system" fn(Enc, Guid, *mut CapsParam, *mut c_int) -> Status>;
type OpenEx = Option<unsafe extern "system" fn(*mut OpenParams, *mut Enc) -> Status>;
type LastError = Option<unsafe extern "system" fn(Enc) -> *const c_char>;
type PresetEx =
    Option<unsafe extern "system" fn(Enc, Guid, Guid, u32, *mut PresetConfig) -> Status>;

/// NV_ENCODE_API_FUNCTION_LIST: the entries called here, by name.
#[repr(C)]
pub struct FunctionList {
    pub version: u32,
    pub reserved: u32,
    pub open_encode_session: Unused,
    pub get_encode_guid_count: Call<*mut u32>,
    pub get_encode_profile_guid_count: CodecCount,
    pub get_encode_profile_guids: CodecGuids,
    pub get_encode_guids: Guids,
    pub get_input_format_count: CodecCount,
    pub get_input_formats: Formats,
    pub get_encode_caps: Caps,
    pub unused1: [Unused; 3],
    pub initialize_encoder: Call<*mut InitParams>,
    pub create_input_buffer: Call<*mut CreateInputBuffer>,
    pub destroy_input_buffer: Call<Ptr>,
    pub create_bitstream_buffer: Call<*mut CreateBitstreamBuffer>,
    pub destroy_bitstream_buffer: Call<Ptr>,
    pub encode_picture: Call<*mut PicParams>,
    pub lock_bitstream: Call<*mut LockBitstream>,
    pub unlock_bitstream: Call<Ptr>,
    pub lock_input_buffer: Call<*mut LockInputBuffer>,
    pub unlock_input_buffer: Call<Ptr>,
    pub unused2: [Unused; 6],
    pub destroy_encoder: Option<unsafe extern "system" fn(Enc) -> Status>,
    pub invalidate_ref_frames: Unused,
    pub open_encode_session_ex: OpenEx,
    pub unused3: [Unused; 2],
    pub reconfigure_encoder: Call<*mut Reconfigure>,
    pub unused4: [Unused; 4],
    pub get_last_error_string: LastError,
    pub unused5: Unused,
    pub get_encode_preset_config_ex: PresetEx,
    pub rest: [Unused; 278],
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<CapsParam>() == 256);
    assert!(size_of::<OpenParams>() == 1552);
    assert!(size_of::<FunctionList>() == 2552);
    assert!(offset_of!(FunctionList, get_encode_caps) == 64);
    assert!(offset_of!(FunctionList, initialize_encoder) == 96);
    assert!(offset_of!(FunctionList, encode_picture) == 136);
    assert!(offset_of!(FunctionList, destroy_encoder) == 224);
    assert!(offset_of!(FunctionList, open_encode_session_ex) == 240);
    assert!(offset_of!(FunctionList, reconfigure_encoder) == 264);
    assert!(offset_of!(FunctionList, get_last_error_string) == 304);
    assert!(offset_of!(FunctionList, get_encode_preset_config_ex) == 320);
};

macro_rules! sym {
    ($lib:expr, $name:literal) => {
        *$lib
            .get(concat!($name, "\0").as_bytes())
            .map_err(|e| format!("{}: {e}", $name))?
    };
}

/// The encode library and its function list.
pub struct Api {
    pub nv: Box<FunctionList>,
    _lib: Library,
}

impl Api {
    /// Loads the library; fails unless its driver has API 12.1 or later.
    pub fn load() -> Result<Api, String> {
        unsafe {
            let lib = Library::new(LIB_ENCODE).map_err(|e| format!("{LIB_ENCODE}: {e}"))?;
            let max_version: unsafe extern "system" fn(*mut u32) -> Status =
                sym!(lib, "NvEncodeAPIGetMaxSupportedVersion");
            let create: unsafe extern "system" fn(*mut FunctionList) -> Status =
                sym!(lib, "NvEncodeAPICreateInstance");
            let mut v = 0u32;
            let r = max_version(&mut v);
            if r != 0 {
                return Err(format!("NvEncodeAPIGetMaxSupportedVersion failed ({r})"));
            }
            if (v >> 4, v & 0xf) < (API_MAJOR, API_MINOR) {
                return Err(format!(
                    "the driver has NVENC API {}.{}, this build needs {API_MAJOR}.{API_MINOR}",
                    v >> 4,
                    v & 0xf
                ));
            }
            let mut nv: Box<FunctionList> = Box::new(std::mem::zeroed());
            nv.version = FUNCTION_LIST_VER;
            let r = create(&mut *nv);
            if r != 0 {
                return Err(format!("NvEncodeAPICreateInstance failed ({r})"));
            }
            Ok(Api { nv, _lib: lib })
        }
    }

    /// Opens an encode session on a CUDA context.
    pub fn open(&self, ctx: *mut c_void) -> Result<Enc, String> {
        let Some(open) = self.nv.open_encode_session_ex else {
            return Err("the driver's function list is incomplete".into());
        };
        let mut enc = std::ptr::null_mut();
        let r = unsafe {
            let mut params: Box<OpenParams> = Box::new(std::mem::zeroed());
            params.version = OPEN_PARAMS_VER;
            params.device_type = DEVICE_TYPE_CUDA;
            params.device = ctx;
            params.api_version = API_VERSION;
            open(&mut *params, &mut enc)
        };
        if r != 0 || enc.is_null() {
            return Err(format!(
                "nvEncOpenEncodeSessionEx failed: {} ({r}; all sessions in use?)",
                status_name(r)
            ));
        }
        Ok(enc)
    }
}

pub struct Cuda {
    device_get_count: unsafe extern "system" fn(*mut c_int) -> c_int,
    device_get: unsafe extern "system" fn(*mut c_int, c_int) -> c_int,
    device_get_name: unsafe extern "system" fn(*mut c_char, c_int, c_int) -> c_int,
    ctx_create: unsafe extern "system" fn(*mut *mut c_void, u32, c_int) -> c_int,
    ctx_pop_current: unsafe extern "system" fn(*mut *mut c_void) -> c_int,
    pub ctx_destroy: unsafe extern "system" fn(*mut c_void) -> c_int,
    _lib: Library,
}

impl Cuda {
    /// Loads the CUDA driver and initializes it.
    pub fn load() -> Result<Cuda, String> {
        unsafe {
            let lib = Library::new(LIB_CUDA).map_err(|e| format!("{LIB_CUDA}: {e}"))?;
            let init: unsafe extern "system" fn(u32) -> c_int = sym!(lib, "cuInit");
            let cuda = Cuda {
                device_get_count: sym!(lib, "cuDeviceGetCount"),
                device_get: sym!(lib, "cuDeviceGet"),
                device_get_name: sym!(lib, "cuDeviceGetName"),
                ctx_create: sym!(lib, "cuCtxCreate_v2"),
                ctx_pop_current: sym!(lib, "cuCtxPopCurrent_v2"),
                ctx_destroy: sym!(lib, "cuCtxDestroy_v2"),
                _lib: lib,
            };
            let r = init(0);
            if r != 0 {
                return Err(format!("cuInit failed ({r})"));
            }
            Ok(cuda)
        }
    }

    pub fn count(&self) -> Result<c_int, String> {
        let mut n: c_int = 0;
        let r = unsafe { (self.device_get_count)(&mut n) };
        if r != 0 {
            return Err(format!("cuDeviceGetCount failed ({r})"));
        }
        Ok(n)
    }

    /// A new context on device `i`, not left current on this thread.
    pub fn context(&self, i: c_int) -> Result<*mut c_void, String> {
        let mut dev: c_int = 0;
        let r = unsafe { (self.device_get)(&mut dev, i) };
        if r != 0 {
            return Err(format!("cuDeviceGet failed ({r})"));
        }
        let mut ctx = std::ptr::null_mut();
        let r = unsafe { (self.ctx_create)(&mut ctx, 0, dev) };
        if r != 0 || ctx.is_null() {
            return Err(format!("cuCtxCreate failed ({r})"));
        }
        let mut popped = std::ptr::null_mut();
        unsafe { (self.ctx_pop_current)(&mut popped) };
        Ok(ctx)
    }

    pub fn name(&self, i: c_int) -> String {
        let mut dev: c_int = 0;
        let mut buf = [0 as c_char; 256];
        unsafe {
            if (self.device_get)(&mut dev, i) != 0
                || (self.device_get_name)(buf.as_mut_ptr(), buf.len() as c_int, dev) != 0
            {
                return String::new();
            }
        }
        unsafe { c_string(buf.as_ptr(), buf.len()) }
    }
}
