//! Media Foundation discovery: the hardware encoder MFTs for each output format.
//! Listing alone doesn't show an MFT works, so these stay discovered.

use super::caps::{Acceleration, EncoderCapability, Probe as Found, ProbeProvider};
use super::{Backend, Codec};
use windows::core::{GUID, PWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, MFMediaType_Video, MFShutdown, MFStartup, MFTEnumEx,
    MFT_ENUM_HARDWARE_VENDOR_ID_Attribute, MFT_FRIENDLY_NAME_Attribute, MFVideoFormat_AV1,
    MFVideoFormat_H264, MFVideoFormat_HEVC, MFSTARTUP_LITE, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_REGISTER_TYPE_INFO, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};

/// Most MFTs read from one enumeration.
const MAX_MFTS: u32 = 64;

/// The MFTs of one enumeration, released as they are read.
unsafe fn take(arr: *mut Option<IMFActivate>, n: u32) -> Vec<IMFActivate> {
    if arr.is_null() {
        return Vec::new();
    }
    let all: Vec<Option<IMFActivate>> = (0..n as usize).map(|i| arr.add(i).read()).collect();
    CoTaskMemFree(Some(arr as *const _));
    all.into_iter().flatten().take(MAX_MFTS as usize).collect()
}

unsafe fn string(a: &IMFActivate, key: &GUID) -> Option<String> {
    let mut p = PWSTR::null();
    let mut n = 0u32;
    a.GetAllocatedString(key, &mut p, &mut n).ok()?;
    if p.is_null() {
        return None;
    }
    let s = String::from_utf16_lossy(std::slice::from_raw_parts(p.0, n as usize));
    CoTaskMemFree(Some(p.0 as *const _));
    Some(s)
}

pub struct Probe;

impl ProbeProvider for Probe {
    fn backend(&self) -> Backend {
        Backend::MediaFoundation
    }

    fn probe(&self) -> Found {
        unsafe {
            if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
                return Found::failed(Backend::MediaFoundation, format!("CoInitializeEx: {e}"));
            }
            let found = match MFStartup(MF_VERSION, MFSTARTUP_LITE) {
                Ok(()) => {
                    let f = enumerate();
                    let _ = MFShutdown();
                    f
                }
                Err(e) => Found::failed(Backend::MediaFoundation, format!("MFStartup: {e}")),
            };
            CoUninitialize();
            found
        }
    }
}

unsafe fn enumerate() -> Found {
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
        let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut n = 0u32;
        if let Err(e) = MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&out),
            &mut arr,
            &mut n,
        ) {
            found.note(
                Backend::MediaFoundation,
                None,
                format!("MFTEnumEx({codec:?}): {e}"),
            );
            continue;
        }
        for a in take(arr, n) {
            let name = string(&a, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_default();
            let vendor = string(&a, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute);
            let id = match vendor {
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
