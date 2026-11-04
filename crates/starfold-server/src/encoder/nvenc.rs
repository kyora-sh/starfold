//! NVENC discovery through the driver's own libraries, loaded at runtime: the API
//! version, then per CUDA device an encode session and its codecs, profiles, input
//! formats and limits.

use super::caps::{
    Acceleration, Chroma, EncoderCapability, EncoderLimits, PixelFormat, Probe as Found,
    ProbeProvider, ProbeStatus, ProfileCapability,
};
use super::{Backend, Codec};
use libloading::Library;
use std::ffi::{c_char, c_int, c_void};
use std::mem::{offset_of, size_of};
use std::ptr;

#[cfg(windows)]
const LIB_ENCODE: &str = "nvEncodeAPI64.dll";
#[cfg(not(windows))]
const LIB_ENCODE: &str = "libnvidia-encode.so.1";
#[cfg(windows)]
const LIB_CUDA: &str = "nvcuda.dll";
#[cfg(not(windows))]
const LIB_CUDA: &str = "libcuda.so.1";

/// The SDK 12.1 headers these structs follow.
const API_MAJOR: u32 = 12;
const API_MINOR: u32 = 1;
const API_VERSION: u32 = API_MAJOR | (API_MINOR << 24);
const fn struct_version(v: u32) -> u32 {
    API_VERSION | (v << 16) | (0x7 << 28)
}
const DEVICE_TYPE_CUDA: u32 = 1;
/// More than any driver reports; bounds what we allocate for its lists.
const MAX_LIST: u32 = 64;
const MAX_DEVICES: c_int = 16;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Guid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> Guid {
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

/// Profile GUIDs and what they encode.
pub const PROFILES: [(Guid, &str, u8, Chroma); 10] = [
    (
        guid(
            0x0727bcaa,
            0x78c4,
            0x4c83,
            [0x8c, 0x2f, 0xef, 0x3d, 0xff, 0x26, 0x7c, 0x6a],
        ),
        "baseline",
        8,
        Chroma::Yuv420,
    ),
    (
        guid(
            0x60b5c1d4,
            0x67fe,
            0x4790,
            [0x94, 0xd5, 0xc4, 0x72, 0x6d, 0x7b, 0x6e, 0x6d],
        ),
        "main",
        8,
        Chroma::Yuv420,
    ),
    (
        guid(
            0xe7cbc309,
            0x4f7a,
            0x4b89,
            [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
        ),
        "high",
        8,
        Chroma::Yuv420,
    ),
    (
        guid(
            0x7ac663cb,
            0xa598,
            0x4960,
            [0xb8, 0x44, 0x33, 0x9b, 0x26, 0x1a, 0x7d, 0x52],
        ),
        "high444",
        8,
        Chroma::Yuv444,
    ),
    (
        guid(
            0xb405afac,
            0xf32b,
            0x417b,
            [0x89, 0xc4, 0x9a, 0xbe, 0xed, 0x3e, 0x59, 0x78],
        ),
        "progressive_high",
        8,
        Chroma::Yuv420,
    ),
    (
        guid(
            0xaec1bd87,
            0xe85b,
            0x48f2,
            [0x84, 0xc3, 0x98, 0xbc, 0xa6, 0x28, 0x50, 0x72],
        ),
        "constrained_high",
        8,
        Chroma::Yuv420,
    ),
    (
        guid(
            0xb514c39a,
            0xb55b,
            0x40fa,
            [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec],
        ),
        "main",
        8,
        Chroma::Yuv420,
    ),
    (
        guid(
            0xfa4d2b6c,
            0x3a5b,
            0x411a,
            [0x80, 0x18, 0x0a, 0x3f, 0x5e, 0x3c, 0x9b, 0xe5],
        ),
        "main10",
        10,
        Chroma::Yuv420,
    ),
    (
        guid(
            0x51ec32b5,
            0x1b4c,
            0x453c,
            [0x9c, 0xbd, 0xb6, 0x16, 0xbd, 0x62, 0x13, 0x41],
        ),
        "frext",
        8,
        Chroma::Yuv444,
    ),
    (
        guid(
            0x5f2a39f5,
            0xf14e,
            0x4f95,
            [0x9a, 0x9e, 0xb7, 0x6d, 0x56, 0x8f, 0xcf, 0x97],
        ),
        "main",
        8,
        Chroma::Yuv420,
    ),
];

const FMT_NV12: u32 = 0x1;
const FMT_IYUV: u32 = 0x100;
const FMT_YUV444: u32 = 0x1000;
const FMT_YUV420_10BIT: u32 = 0x10000;

const CAPS_WIDTH_MAX: u32 = 16;
const CAPS_HEIGHT_MAX: u32 = 17;
const CAPS_MB_NUM_MAX: u32 = 31;
const CAPS_MB_PER_SEC_MAX: u32 = 32;
const CAPS_10BIT_ENCODE: u32 = 39;
const CAPS_WIDTH_MIN: u32 = 45;
const CAPS_HEIGHT_MIN: u32 = 46;

/// The limits NVENC reported for one codec; `None` where the query failed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Limits {
    pub width: (Option<i32>, Option<i32>),
    pub height: (Option<i32>, Option<i32>),
    pub mbs: Option<i32>,
    pub mbs_per_sec: Option<i32>,
    pub ten_bit: Option<i32>,
}

/// One codec of one device, as NVENC reported it.
#[derive(Clone, Debug, PartialEq)]
pub struct CodecReport {
    pub codec: Guid,
    pub profiles: Vec<Guid>,
    pub formats: Vec<u32>,
    pub limits: Limits,
}

/// One capability per codec this build knows; unknown GUIDs are skipped.
pub fn caps(device: &str, reports: &[CodecReport]) -> Vec<EncoderCapability> {
    let pos = |v: Option<i32>| v.filter(|&v| v > 0).map(|v| v as u32);
    reports
        .iter()
        .filter_map(|r| {
            let codec = match r.codec {
                CODEC_H264 => Codec::H264,
                CODEC_HEVC => Codec::Hevc,
                CODEC_AV1 => Codec::Av1,
                _ => return None,
            };
            let mut c =
                EncoderCapability::new(codec, Backend::Nvenc, Acceleration::Hardware, device);
            for g in &r.profiles {
                if let Some(&(_, name, depth, chroma)) = PROFILES.iter().find(|p| p.0 == *g) {
                    c.profiles.push(ProfileCapability::new(name, depth, chroma));
                }
            }
            // AV1's main profile is 8- and 10-bit; the 10-bit cap says which.
            if codec == Codec::Av1 && r.limits.ten_bit.is_some_and(|v| v > 0) {
                c.profiles
                    .push(ProfileCapability::new("main", 10, Chroma::Yuv420));
            }
            let fmts: Vec<PixelFormat> = r
                .formats
                .iter()
                .filter_map(|&f| match f {
                    FMT_NV12 => Some(PixelFormat::Nv12),
                    FMT_IYUV => Some(PixelFormat::I420),
                    FMT_YUV444 => Some(PixelFormat::Yuv444),
                    FMT_YUV420_10BIT => Some(PixelFormat::P010),
                    _ => None,
                })
                .collect();
            let eight_bit_input = fmts
                .iter()
                .any(|f| matches!(f, PixelFormat::Nv12 | PixelFormat::I420));
            c.limits = EncoderLimits {
                min_width: pos(r.limits.width.0),
                max_width: pos(r.limits.width.1),
                min_height: pos(r.limits.height.0),
                max_height: pos(r.limits.height.1),
                max_mbs: pos(r.limits.mbs).map(u64::from),
                max_mbs_per_sec: pos(r.limits.mbs_per_sec).map(u64::from),
                pixel_formats: (!fmts.is_empty()).then_some(fmts),
                ..Default::default()
            };
            if !eight_bit_input || !c.profiles.iter().any(ProfileCapability::baseline) {
                c.status = ProbeStatus::ProbeOnly;
            }
            Some(c)
        })
        .collect()
}

type Status = c_int;
type Enc = *mut c_void;

#[repr(C)]
struct CapsParam {
    version: u32,
    caps: u32,
    reserved: [u32; 62],
}

#[repr(C)]
struct OpenParams {
    version: u32,
    device_type: u32,
    device: *mut c_void,
    reserved: *mut c_void,
    api_version: u32,
    reserved1: [u32; 253],
    reserved2: [*mut c_void; 64],
}

type Unused = Option<unsafe extern "system" fn()>;
type Count = Option<unsafe extern "system" fn(Enc, *mut u32) -> Status>;
type CodecCount = Option<unsafe extern "system" fn(Enc, Guid, *mut u32) -> Status>;
type Guids = Option<unsafe extern "system" fn(Enc, *mut Guid, u32, *mut u32) -> Status>;
type CodecGuids = Option<unsafe extern "system" fn(Enc, Guid, *mut Guid, u32, *mut u32) -> Status>;
type Formats = Option<unsafe extern "system" fn(Enc, Guid, *mut u32, u32, *mut u32) -> Status>;
type Caps = Option<unsafe extern "system" fn(Enc, Guid, *mut CapsParam, *mut c_int) -> Status>;
type Destroy = Option<unsafe extern "system" fn(Enc) -> Status>;
type OpenEx = Option<unsafe extern "system" fn(*mut OpenParams, *mut Enc) -> Status>;

/// NV_ENCODE_API_FUNCTION_LIST: the entries the probe calls, by name.
#[repr(C)]
struct FunctionList {
    version: u32,
    reserved: u32,
    open_encode_session: Unused,
    get_encode_guid_count: Count,
    get_encode_profile_guid_count: CodecCount,
    get_encode_profile_guids: CodecGuids,
    get_encode_guids: Guids,
    get_input_format_count: CodecCount,
    get_input_formats: Formats,
    get_encode_caps: Caps,
    unused1: [Unused; 19],
    destroy_encoder: Destroy,
    invalidate_ref_frames: Unused,
    open_encode_session_ex: OpenEx,
    rest: [Unused; 288],
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<CapsParam>() == 256);
    assert!(size_of::<OpenParams>() == 1552);
    assert!(size_of::<FunctionList>() == 2552);
    assert!(offset_of!(FunctionList, get_encode_caps) == 64);
    assert!(offset_of!(FunctionList, destroy_encoder) == 224);
    assert!(offset_of!(FunctionList, open_encode_session_ex) == 240);
};

struct Cuda {
    init: unsafe extern "system" fn(u32) -> c_int,
    device_get_count: unsafe extern "system" fn(*mut c_int) -> c_int,
    device_get: unsafe extern "system" fn(*mut c_int, c_int) -> c_int,
    device_get_name: unsafe extern "system" fn(*mut c_char, c_int, c_int) -> c_int,
    ctx_create: unsafe extern "system" fn(*mut *mut c_void, u32, c_int) -> c_int,
    ctx_destroy: unsafe extern "system" fn(*mut c_void) -> c_int,
    _lib: Library,
}

macro_rules! sym {
    ($lib:expr, $name:literal) => {
        *$lib
            .get(concat!($name, "\0").as_bytes())
            .map_err(|e| format!("{}: {e}", $name))?
    };
}

impl Cuda {
    fn load() -> Result<Cuda, String> {
        unsafe {
            let lib = Library::new(LIB_CUDA).map_err(|e| format!("{LIB_CUDA}: {e}"))?;
            Ok(Cuda {
                init: sym!(lib, "cuInit"),
                device_get_count: sym!(lib, "cuDeviceGetCount"),
                device_get: sym!(lib, "cuDeviceGet"),
                device_get_name: sym!(lib, "cuDeviceGetName"),
                ctx_create: sym!(lib, "cuCtxCreate_v2"),
                ctx_destroy: sym!(lib, "cuCtxDestroy_v2"),
                _lib: lib,
            })
        }
    }

    unsafe fn name(&self, dev: c_int) -> String {
        let mut buf = [0 as c_char; 256];
        if (self.device_get_name)(buf.as_mut_ptr(), buf.len() as c_int, dev) != 0 {
            return String::new();
        }
        let bytes: Vec<u8> = buf
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// An open encode session on one CUDA context, closed on drop.
struct Session<'a> {
    nv: &'a FunctionList,
    enc: Enc,
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        if let Some(f) = self.nv.destroy_encoder {
            unsafe { f(self.enc) };
        }
    }
}

impl Session<'_> {
    /// A bounded list: `count` says how many, `fill` writes at most that many.
    unsafe fn list<T: Copy + Default>(
        count: impl FnOnce(*mut u32) -> Status,
        fill: impl FnOnce(*mut T, u32, *mut u32) -> Status,
    ) -> Result<Vec<T>, Status> {
        let mut n = 0u32;
        let r = count(&mut n);
        if r != 0 {
            return Err(r);
        }
        let mut v = vec![T::default(); n.min(MAX_LIST) as usize];
        if v.is_empty() {
            return Ok(v);
        }
        let mut got = 0u32;
        let r = fill(v.as_mut_ptr(), v.len() as u32, &mut got);
        if r != 0 {
            return Err(r);
        }
        v.truncate((got as usize).min(v.len()));
        Ok(v)
    }

    unsafe fn cap(&self, codec: Guid, which: u32) -> Option<i32> {
        let f = self.nv.get_encode_caps?;
        let mut p = CapsParam {
            version: struct_version(1),
            caps: which,
            reserved: [0; 62],
        };
        let mut v: c_int = 0;
        (f(self.enc, codec, &mut p, &mut v) == 0).then_some(v)
    }

    unsafe fn report(&self) -> Result<Vec<CodecReport>, String> {
        let nv = self.nv;
        let (Some(count), Some(guids), Some(pcount), Some(pguids), Some(fcount), Some(fmts)) = (
            nv.get_encode_guid_count,
            nv.get_encode_guids,
            nv.get_encode_profile_guid_count,
            nv.get_encode_profile_guids,
            nv.get_input_format_count,
            nv.get_input_formats,
        ) else {
            return Err("the driver's function list is incomplete".into());
        };
        let enc = self.enc;
        let codecs = Self::list(|n| count(enc, n), |g, cap, n| guids(enc, g, cap, n))
            .map_err(|r| format!("nvEncGetEncodeGUIDs failed ({r})"))?;
        Ok(codecs
            .into_iter()
            .map(|c| CodecReport {
                codec: c,
                profiles: Self::list(|n| pcount(enc, c, n), |g, cap, n| pguids(enc, c, g, cap, n))
                    .unwrap_or_default(),
                formats: Self::list(|n| fcount(enc, c, n), |f, cap, n| fmts(enc, c, f, cap, n))
                    .unwrap_or_default(),
                limits: Limits {
                    width: (self.cap(c, CAPS_WIDTH_MIN), self.cap(c, CAPS_WIDTH_MAX)),
                    height: (self.cap(c, CAPS_HEIGHT_MIN), self.cap(c, CAPS_HEIGHT_MAX)),
                    mbs: self.cap(c, CAPS_MB_NUM_MAX),
                    mbs_per_sec: self.cap(c, CAPS_MB_PER_SEC_MAX),
                    ten_bit: self.cap(c, CAPS_10BIT_ENCODE),
                },
            })
            .collect())
    }
}

pub struct Probe;

impl ProbeProvider for Probe {
    fn backend(&self) -> Backend {
        Backend::Nvenc
    }

    fn probe(&self) -> Found {
        match unsafe { probe() } {
            Ok(f) => f,
            Err(e) => Found::failed(Backend::Nvenc, e),
        }
    }
}

unsafe fn probe() -> Result<Found, String> {
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
            "the driver has NVENC API {}.{}, the probe needs {API_MAJOR}.{API_MINOR}",
            v >> 4,
            v & 0xf
        ));
    }
    let mut nv: Box<FunctionList> = Box::new(std::mem::zeroed());
    nv.version = struct_version(2);
    let r = create(&mut *nv);
    if r != 0 {
        return Err(format!("NvEncodeAPICreateInstance failed ({r})"));
    }
    let Some(open) = nv.open_encode_session_ex else {
        return Err("the driver's function list is incomplete".into());
    };
    let cuda = Cuda::load()?;
    let r = (cuda.init)(0);
    if r != 0 {
        return Err(format!("cuInit failed ({r})"));
    }
    let mut n: c_int = 0;
    let r = (cuda.device_get_count)(&mut n);
    if r != 0 {
        return Err(format!("cuDeviceGetCount failed ({r})"));
    }
    let mut found = Found::default();
    if n <= 0 {
        found.note(Backend::Nvenc, None, "no CUDA devices");
    }
    for i in 0..n.min(MAX_DEVICES) {
        let mut dev: c_int = 0;
        if (cuda.device_get)(&mut dev, i) != 0 {
            continue;
        }
        let id = format!("cuda:{i} {}", cuda.name(dev));
        let mut ctx = ptr::null_mut();
        let r = (cuda.ctx_create)(&mut ctx, 0, dev);
        if r != 0 || ctx.is_null() {
            found.note(
                Backend::Nvenc,
                Some(&id),
                format!("cuCtxCreate failed ({r})"),
            );
            continue;
        }
        let mut params: Box<OpenParams> = Box::new(std::mem::zeroed());
        params.version = struct_version(1);
        params.device_type = DEVICE_TYPE_CUDA;
        params.device = ctx;
        params.api_version = API_VERSION;
        let mut enc = ptr::null_mut();
        let r = open(&mut *params, &mut enc);
        if r != 0 || enc.is_null() {
            found.note(
                Backend::Nvenc,
                Some(&id),
                format!("nvEncOpenEncodeSessionEx failed ({r}; all sessions in use?)"),
            );
        } else {
            let s = Session { nv: &nv, enc };
            match s.report() {
                Ok(r) => found.caps.extend(caps(&id, &r)),
                Err(e) => found.note(Backend::Nvenc, Some(&id), e),
            }
        }
        (cuda.ctx_destroy)(ctx);
    }
    Ok(found)
}
