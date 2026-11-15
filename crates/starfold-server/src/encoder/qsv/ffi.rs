//! The oneVPL 2.x structs, constants and entry points used here, laid out as Intel's
//! libvpl API headers have them (tag v2.17.0, commit d77f9195cf495b937631607333288fd917ae8939,
//! MIT) for 64-bit targets. Only the fields this encoder reads or writes are named; the rest
//! are kept as reserved bytes of the same size. Every size and named offset is checked in
//! tests against what the headers compile to. The runtime is Intel's GPU runtime
//! (`libmfx64-gen.dll`), loaded at runtime by its full path; no dispatcher.

use libloading::Library;
use std::ffi::{c_char, c_void};

pub type Status = i32;
pub type Session = *mut c_void;
pub type SyncPoint = *mut c_void;
pub type Hdl = *mut c_void;

pub const ERR_NONE: Status = 0;
pub const ERR_MORE_DATA: Status = -10;
pub const ERR_DEVICE_LOST: Status = -13;
pub const ERR_DEVICE_FAILED: Status = -17;
pub const ERR_GPU_HANG: Status = -21;
pub const WRN_IN_EXECUTION: Status = 1;
pub const WRN_DEVICE_BUSY: Status = 2;
pub const WRN_PARTIAL_ACCELERATION: Status = 4;

/// mfxStatus names, by value.
pub fn status_name(s: Status) -> &'static str {
    match s {
        0 => "MFX_ERR_NONE",
        -1 => "MFX_ERR_UNKNOWN",
        -2 => "MFX_ERR_NULL_PTR",
        -3 => "MFX_ERR_UNSUPPORTED",
        -4 => "MFX_ERR_MEMORY_ALLOC",
        -5 => "MFX_ERR_NOT_ENOUGH_BUFFER",
        -6 => "MFX_ERR_INVALID_HANDLE",
        -7 => "MFX_ERR_LOCK_MEMORY",
        -8 => "MFX_ERR_NOT_INITIALIZED",
        -9 => "MFX_ERR_NOT_FOUND",
        -10 => "MFX_ERR_MORE_DATA",
        -11 => "MFX_ERR_MORE_SURFACE",
        -12 => "MFX_ERR_ABORTED",
        -13 => "MFX_ERR_DEVICE_LOST",
        -14 => "MFX_ERR_INCOMPATIBLE_VIDEO_PARAM",
        -15 => "MFX_ERR_INVALID_VIDEO_PARAM",
        -16 => "MFX_ERR_UNDEFINED_BEHAVIOR",
        -17 => "MFX_ERR_DEVICE_FAILED",
        -18 => "MFX_ERR_MORE_BITSTREAM",
        -21 => "MFX_ERR_GPU_HANG",
        -22 => "MFX_ERR_REALLOC_SURFACE",
        -23 => "MFX_ERR_RESOURCE_MAPPED",
        -24 => "MFX_ERR_NOT_IMPLEMENTED",
        -25 => "MFX_ERR_MORE_EXTBUFFER",
        1 => "MFX_WRN_IN_EXECUTION",
        2 => "MFX_WRN_DEVICE_BUSY",
        3 => "MFX_WRN_VIDEO_PARAM_CHANGED",
        4 => "MFX_WRN_PARTIAL_ACCELERATION",
        5 => "MFX_WRN_INCOMPATIBLE_VIDEO_PARAM",
        6 => "MFX_WRN_VALUE_NOT_CHANGED",
        7 => "MFX_WRN_OUT_OF_RANGE",
        10 => "MFX_WRN_FILTER_SKIPPED",
        12 => "MFX_ERR_NONE_PARTIAL_OUTPUT",
        13 => "MFX_WRN_ALLOC_TIMEOUT_EXPIRED",
        _ => "an unknown mfxStatus",
    }
}

const fn fourcc(c: [u8; 4]) -> u32 {
    u32::from_le_bytes(c)
}

pub const CODEC_AVC: u32 = fourcc(*b"AVC ");
pub const CODEC_HEVC: u32 = fourcc(*b"HEVC");
pub const FOURCC_NV12: u32 = fourcc(*b"NV12");
pub const EXTBUFF_VIDEO_SIGNAL_INFO: u32 = fourcc(*b"VSIN");

/// mfxImplType, mfxAccelerationMode, mfxImplCapsDeliveryFormat, mfxResourceType.
pub const IMPL_TYPE_HARDWARE: u32 = 2;
pub const IMPLCAPS_IMPLDESCSTRUCTURE: u32 = 1;
pub const IMPLCAPS_DEVICE_ID_EXTENDED: u32 = 4;
pub const RESOURCE_SYSTEM_SURFACE: u32 = 1;
pub const VENDOR_INTEL: u32 = 0x8086;

pub const PROFILE_AVC_BASELINE: u32 = 66;
pub const PROFILE_AVC_MAIN: u32 = 77;
pub const PROFILE_AVC_HIGH: u32 = 100;
pub const PROFILE_AVC_CONSTRAINED_BASELINE: u32 = 66 + (0x100 << 1);
pub const PROFILE_HEVC_MAIN: u32 = 1;
pub const PROFILE_HEVC_MAIN10: u32 = 2;

pub const PICSTRUCT_PROGRESSIVE: u16 = 0x01;
pub const CHROMAFORMAT_YUV420: u16 = 1;
pub const IOPATTERN_IN_SYSTEM_MEMORY: u16 = 0x02;
pub const CODINGOPTION_ON: u16 = 0x10;
pub const CODINGOPTION_OFF: u16 = 0x20;
pub const TARGETUSAGE_BEST_SPEED: u16 = 7;
pub const GOP_CLOSED: u16 = 1;
pub const RATECONTROL_CBR: u16 = 1;
pub const RATECONTROL_CQP: u16 = 3;
pub const FRAMETYPE_I: u16 = 0x0001;
pub const FRAMETYPE_REF: u16 = 0x0040;
pub const FRAMETYPE_IDR: u16 = 0x0080;
pub const MAP_WRITE: u32 = 0x2;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Version {
    pub minor: u16,
    pub major: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ExtBuffer {
    pub buffer_id: u32,
    pub buffer_sz: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Range32U {
    pub min: u32,
    pub max: u32,
    pub step: u32,
}

/// mfxInitializationParam.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InitializationParam {
    pub acceleration_mode: u32,
    pub reserved: [u16; 3],
    pub num_ext_param: u16,
    pub ext_param: *mut *mut ExtBuffer,
    pub vendor_impl_id: u32,
    pub reserved2: [u32; 3],
}

/// mfxEncoderDescription's `encoder::encprofile::encmemdesc`.
#[repr(C)]
pub struct EncMemDesc {
    pub mem_handle_type: u32,
    pub width: Range32U,
    pub height: Range32U,
    pub reserved: [u16; 7],
    pub num_color_formats: u16,
    pub color_formats: *mut u32,
}

/// mfxEncoderDescription's `encoder::encprofile`.
#[repr(C)]
pub struct EncProfile {
    pub profile: u32,
    pub reserved: [u16; 7],
    pub num_mem_types: u16,
    pub mem_desc: *mut EncMemDesc,
}

/// mfxEncoderDescription's `encoder`.
#[repr(C)]
pub struct EncCodec {
    pub codec_id: u32,
    pub max_codec_level: u16,
    pub bidirectional_prediction: u16,
    pub reserved: [u16; 7],
    pub num_profiles: u16,
    pub profiles: *mut EncProfile,
}

/// mfxEncoderDescription.
#[repr(C)]
pub struct EncoderDescription {
    pub version: u16,
    pub reserved: [u16; 7],
    pub num_codecs: u16,
    pub codecs: *mut EncCodec,
}

/// mfxImplDescription: the device, decoder and VPP descriptions are kept opaque.
#[repr(C)]
pub struct ImplDescription {
    pub version: u16,
    pub impl_type: u32,
    pub acceleration_mode: u32,
    pub api_version: Version,
    pub impl_name: [c_char; 32],
    pub license: [c_char; 128],
    pub keywords: [c_char; 128],
    pub vendor_id: u32,
    pub vendor_impl_id: u32,
    /// mfxDeviceDescription and mfxDecoderDescription.
    pub dev_dec: [u64; 24],
    pub enc: EncoderDescription,
    /// mfxVPPDescription, the acceleration modes, the pool policies, reserved words and
    /// the extension buffers.
    pub rest: [u64; 14],
}

/// mfxExtendedDeviceId.
#[repr(C)]
pub struct ExtendedDeviceId {
    pub version: u16,
    pub vendor_id: u16,
    pub device_id: u16,
    pub pci_domain: u32,
    pub pci_bus: u32,
    pub pci_device: u32,
    pub pci_function: u32,
    pub device_luid: [u8; 8],
    pub luid_device_node_mask: u32,
    pub luid_valid: u32,
    pub drm_render_node_num: u32,
    pub drm_primary_node_num: u32,
    pub revision_id: u16,
    pub reserved1: [u8; 18],
    pub device_name: [c_char; 128],
}

/// mfxFrameInfo with the frame-parameter arm of its size union.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameInfo {
    pub reserved: [u32; 4],
    pub channel_id: u16,
    pub bit_depth_luma: u16,
    pub bit_depth_chroma: u16,
    pub shift: u16,
    /// mfxFrameId.
    pub frame_id: [u16; 4],
    pub fourcc: u32,
    pub width: u16,
    pub height: u16,
    pub crop_x: u16,
    pub crop_y: u16,
    pub crop_w: u16,
    pub crop_h: u16,
    pub frame_rate_ext_n: u32,
    pub frame_rate_ext_d: u32,
    pub reserved3: u16,
    pub aspect_ratio_w: u16,
    pub aspect_ratio_h: u16,
    pub pic_struct: u16,
    pub chroma_format: u16,
    pub reserved2: u16,
}

/// mfxFrameData.
#[repr(C)]
pub struct FrameData {
    pub ext_param: *mut *mut ExtBuffer,
    pub num_ext_param: u16,
    pub reserved: [u16; 9],
    pub mem_type: u16,
    pub pitch_high: u16,
    pub time_stamp: u64,
    pub frame_order: u32,
    pub locked: u16,
    pub pitch_low: u16,
    pub y: *mut u8,
    pub uv: *mut u8,
    pub v: *mut u8,
    pub a: *mut u8,
    pub mem_id: *mut c_void,
    pub corrupted: u16,
    pub data_flag: u16,
}

/// mfxFrameSurface1.
#[repr(C)]
pub struct FrameSurface1 {
    pub frame_interface: *mut FrameSurfaceInterface,
    pub version: u16,
    pub reserved1: [u16; 3],
    pub info: FrameInfo,
    pub data: FrameData,
}

type SurfaceFn = Option<unsafe extern "C" fn(*mut FrameSurface1) -> Status>;

/// mfxFrameSurfaceInterface.
#[repr(C)]
pub struct FrameSurfaceInterface {
    pub context: *mut c_void,
    pub version: u16,
    pub reserved1: [u16; 3],
    pub add_ref: SurfaceFn,
    pub release: SurfaceFn,
    pub get_ref_counter: Option<unsafe extern "C" fn(*mut FrameSurface1, *mut u32) -> Status>,
    pub map: Option<unsafe extern "C" fn(*mut FrameSurface1, u32) -> Status>,
    pub unmap: SurfaceFn,
    pub get_native_handle: *mut c_void,
    pub get_device_handle: *mut c_void,
    pub synchronize: Option<unsafe extern "C" fn(*mut FrameSurface1, u32) -> Status>,
    pub on_complete: *mut c_void,
    pub query_interface: *mut c_void,
    pub reserved2: [*mut c_void; 2],
}

/// mfxInfoMFX with the encoder arm of its union.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct InfoMfx {
    pub reserved: [u32; 7],
    pub low_power: u16,
    pub brc_param_multiplier: u16,
    pub frame_info: FrameInfo,
    pub codec_id: u32,
    pub codec_profile: u16,
    pub codec_level: u16,
    pub num_thread: u16,
    pub target_usage: u16,
    pub gop_pic_size: u16,
    pub gop_ref_dist: u16,
    pub gop_opt_flag: u16,
    pub idr_interval: u16,
    pub rate_control_method: u16,
    /// InitialDelayInKB, or QPI under constant QP.
    pub initial_delay_in_kb: u16,
    pub buffer_size_in_kb: u16,
    /// TargetKbps, or QPP under constant QP.
    pub target_kbps: u16,
    /// MaxKbps, or QPB under constant QP.
    pub max_kbps: u16,
    pub num_slice: u16,
    pub num_ref_frame: u16,
    pub encoded_order: u16,
}

/// mfxVideoParam; the bytes after `mfx` are the rest of the mfxInfoVPP arm.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VideoParam {
    pub alloc_id: u32,
    pub reserved: [u32; 2],
    pub reserved3: u16,
    pub async_depth: u16,
    pub mfx: InfoMfx,
    pub vpp_rest: [u32; 8],
    pub protected: u16,
    pub io_pattern: u16,
    pub ext_param: *mut *mut ExtBuffer,
    pub num_ext_param: u16,
    pub reserved2: u16,
}

impl Default for VideoParam {
    fn default() -> Self {
        VideoParam {
            alloc_id: 0,
            reserved: [0; 2],
            reserved3: 0,
            async_depth: 0,
            mfx: InfoMfx::default(),
            vpp_rest: [0; 8],
            protected: 0,
            io_pattern: 0,
            ext_param: std::ptr::null_mut(),
            num_ext_param: 0,
            reserved2: 0,
        }
    }
}

/// mfxExtVideoSignalInfo.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ExtVideoSignalInfo {
    pub header: ExtBuffer,
    pub video_format: u16,
    pub video_full_range: u16,
    pub colour_description_present: u16,
    pub colour_primaries: u16,
    pub transfer_characteristics: u16,
    pub matrix_coefficients: u16,
}

/// mfxEncodeCtrl.
#[repr(C)]
pub struct EncodeCtrl {
    pub header: ExtBuffer,
    pub reserved: [u32; 4],
    pub reserved1: u16,
    pub mfx_nal_unit_type: u16,
    pub skip_frame: u16,
    pub qp: u16,
    pub frame_type: u16,
    pub num_ext_param: u16,
    pub num_payload: u16,
    pub reserved2: u16,
    pub ext_param: *mut *mut ExtBuffer,
    pub payload: *mut *mut c_void,
}

impl Default for EncodeCtrl {
    fn default() -> Self {
        EncodeCtrl {
            header: ExtBuffer::default(),
            reserved: [0; 4],
            reserved1: 0,
            mfx_nal_unit_type: 0,
            skip_frame: 0,
            qp: 0,
            frame_type: 0,
            num_ext_param: 0,
            num_payload: 0,
            reserved2: 0,
            ext_param: std::ptr::null_mut(),
            payload: std::ptr::null_mut(),
        }
    }
}

/// mfxBitstream.
#[repr(C)]
pub struct Bitstream {
    pub encrypted_data: *mut c_void,
    pub ext_param: *mut *mut ExtBuffer,
    pub num_ext_param: u16,
    pub reserved1: u16,
    pub codec_id: u32,
    pub decode_time_stamp: i64,
    pub time_stamp: u64,
    pub data: *mut u8,
    pub data_offset: u32,
    pub data_length: u32,
    pub max_length: u32,
    pub pic_struct: u16,
    pub frame_type: u16,
    pub data_flag: u16,
    pub reserved2: u16,
}

impl Bitstream {
    /// An empty bitstream over `buf`.
    pub fn over(buf: &mut [u8]) -> Bitstream {
        Bitstream {
            encrypted_data: std::ptr::null_mut(),
            ext_param: std::ptr::null_mut(),
            num_ext_param: 0,
            reserved1: 0,
            codec_id: 0,
            decode_time_stamp: 0,
            time_stamp: 0,
            data: buf.as_mut_ptr(),
            data_offset: 0,
            data_length: 0,
            max_length: buf.len().min(u32::MAX as usize) as u32,
            pic_struct: 0,
            frame_type: 0,
            data_flag: 0,
            reserved2: 0,
        }
    }
}

/// The runtime library and the entry points a session uses.
pub struct Runtime {
    pub query_impls: unsafe extern "C" fn(u32, *mut u32) -> *mut Hdl,
    pub release_impl: unsafe extern "C" fn(Hdl) -> Status,
    pub initialize: unsafe extern "C" fn(InitializationParam, *mut Session) -> Status,
    pub close: unsafe extern "C" fn(Session) -> Status,
    pub encode_query: unsafe extern "C" fn(Session, *mut VideoParam, *mut VideoParam) -> Status,
    pub encode_init: unsafe extern "C" fn(Session, *mut VideoParam) -> Status,
    pub encode_reset: unsafe extern "C" fn(Session, *mut VideoParam) -> Status,
    pub encode_close: unsafe extern "C" fn(Session) -> Status,
    pub encode_get_video_param: unsafe extern "C" fn(Session, *mut VideoParam) -> Status,
    pub encode_frame_async: unsafe extern "C" fn(
        Session,
        *mut EncodeCtrl,
        *mut FrameSurface1,
        *mut Bitstream,
        *mut SyncPoint,
    ) -> Status,
    pub sync_operation: unsafe extern "C" fn(Session, SyncPoint, u32) -> Status,
    pub get_surface_for_encode: unsafe extern "C" fn(Session, *mut *mut FrameSurface1) -> Status,
    /// Where it was loaded from.
    pub path: String,
    _lib: Library,
}

impl Runtime {
    /// Loads the runtime at `path`; fails unless it exports the oneVPL 2.x entry points.
    pub fn load(path: &str) -> Result<Runtime, String> {
        unsafe {
            let lib = Library::new(path).map_err(|e| format!("{path}: {e}"))?;
            macro_rules! f {
                ($name:literal) => {
                    *lib.get(concat!($name, "\0").as_bytes()).map_err(|e| {
                        format!("{path} has no {} (not a oneVPL 2.x runtime): {e}", $name)
                    })?
                };
            }
            Ok(Runtime {
                query_impls: f!("MFXQueryImplsDescription"),
                release_impl: f!("MFXReleaseImplDescription"),
                initialize: f!("MFXInitialize"),
                close: f!("MFXClose"),
                encode_query: f!("MFXVideoENCODE_Query"),
                encode_init: f!("MFXVideoENCODE_Init"),
                encode_reset: f!("MFXVideoENCODE_Reset"),
                encode_close: f!("MFXVideoENCODE_Close"),
                encode_get_video_param: f!("MFXVideoENCODE_GetVideoParam"),
                encode_frame_async: f!("MFXVideoENCODE_EncodeFrameAsync"),
                sync_operation: f!("MFXVideoCORE_SyncOperation"),
                get_surface_for_encode: f!("MFXMemory_GetSurfaceForEncode"),
                path: path.to_string(),
                _lib: lib,
            })
        }
    }
}

/// The NUL-terminated string in `s`.
pub fn c_string(s: &[c_char]) -> String {
    let bytes: Vec<u8> = s
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
