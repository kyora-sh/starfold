//! Media Foundation discovery: the hardware encoder MFTs for each output format.
//! Listing alone doesn't show an MFT works, so these stay discovered. mfplat.dll is
//! loaded at runtime: Windows N editions may not have it.

use super::caps::{Acceleration, EncoderCapability, Probe as Found, ProbeProvider};
use super::{Backend, Codec};
use libloading::Library;
use std::ffi::c_void;
use windows::core::{Interface, GUID, HRESULT, PWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, MFMediaType_Video, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
    MFT_FRIENDLY_NAME_Attribute, MFVideoFormat_AV1, MFVideoFormat_H264, MFVideoFormat_HEVC,
    MFSTARTUP_LITE, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_REGISTER_TYPE_INFO, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};

/// Most MFTs read from one enumeration.
const MAX_MFTS: usize = 64;

type MfStartup = unsafe extern "system" fn(u32, u32) -> HRESULT;
type MfShutdown = unsafe extern "system" fn() -> HRESULT;
type MftEnumEx = unsafe extern "system" fn(
    GUID,
    u32,
    *const MFT_REGISTER_TYPE_INFO,
    *const MFT_REGISTER_TYPE_INFO,
    *mut *mut *mut c_void,
    *mut u32,
) -> HRESULT;

/// The MFTs of one enumeration, each owned (released on drop), the array freed.
unsafe fn take(arr: *mut *mut c_void, n: u32) -> Vec<IMFActivate> {
    if arr.is_null() {
        return Vec::new();
    }
    let all: Vec<IMFActivate> = (0..n as usize)
        .map(|i| arr.add(i).read())
        .filter(|p| !p.is_null())
        .map(|p| IMFActivate::from_raw(p))
        .collect();
    CoTaskMemFree(Some(arr as *const c_void));
    all.into_iter().take(MAX_MFTS).collect()
}

unsafe fn string(a: &IMFActivate, key: &GUID) -> Option<String> {
    let mut p = PWSTR::null();
    let mut n = 0u32;
    a.GetAllocatedString(key, &mut p, &mut n).ok()?;
    if p.is_null() {
        return None;
    }
    let s = String::from_utf16_lossy(std::slice::from_raw_parts(p.0, n as usize));
    CoTaskMemFree(Some(p.0 as *const c_void));
    Some(s)
}

pub struct Probe;

impl ProbeProvider for Probe {
    fn backend(&self) -> Backend {
        Backend::MediaFoundation
    }

    fn probe(&self) -> Found {
        unsafe {
            let lib = match Library::new("mfplat.dll") {
                Ok(l) => l,
                Err(e) => {
                    return Found::failed(Backend::MediaFoundation, format!("mfplat.dll: {e}"))
                }
            };
            let syms = (|| -> Result<(MfStartup, MfShutdown, MftEnumEx), libloading::Error> {
                Ok((
                    *lib.get(b"MFStartup\0")?,
                    *lib.get(b"MFShutdown\0")?,
                    *lib.get(b"MFTEnumEx\0")?,
                ))
            })();
            let (startup, shutdown, enum_ex) = match syms {
                Ok(s) => s,
                Err(e) => {
                    return Found::failed(Backend::MediaFoundation, format!("mfplat.dll: {e}"))
                }
            };
            if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
                return Found::failed(Backend::MediaFoundation, format!("CoInitializeEx: {e}"));
            }
            let found = match startup(MF_VERSION, MFSTARTUP_LITE).ok() {
                Ok(()) => {
                    let f = enumerate(enum_ex);
                    let _ = shutdown();
                    f
                }
                Err(e) => Found::failed(Backend::MediaFoundation, format!("MFStartup: {e}")),
            };
            CoUninitialize();
            found
        }
    }
}

unsafe fn enumerate(enum_ex: MftEnumEx) -> Found {
    let mut found = Found::default();
    for (codec, subtype) in [
        (Codec::H264, MFVideoFormat_H264),
        (Codec::Hevc, MFVideoFormat_HEVC),
        (Codec::Av1, MFVideoFormat_AV1),
    ] {
        let out = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: subtype,
        };
        let mut arr: *mut *mut c_void = std::ptr::null_mut();
        let mut n = 0u32;
        let flags = (MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0) as u32;
        let r = enum_ex(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            std::ptr::null(),
            &out,
            &mut arr,
            &mut n,
        );
        if let Err(e) = r.ok() {
            found.note(
                Backend::MediaFoundation,
                None,
                format!("MFTEnumEx({codec:?}): {e}"),
            );
            continue;
        }
        for a in take(arr, n) {
            let name = string(&a, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_default();
            let id = match string(&a, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute) {
                Some(v) => format!("{name} ({v})"),
                None => name,
            };
            found.caps.push(EncoderCapability::new(
                codec,
                Backend::MediaFoundation,
                Acceleration::Hardware,
                id,
            ));
        }
    }
    found
}
