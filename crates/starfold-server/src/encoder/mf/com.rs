//! Media Foundation through the windows crate's COM bindings. mfplat.dll is loaded at
//! runtime (Windows N editions may not have it) and started once per process.

use super::driver::{Api, Driver, Format, Value};
use crate::encoder::hw::write_nv12;
use crate::encoder::{Codec, EncodeError, FrameView};
use libloading::Library;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::core::{Interface, GUID, HRESULT, PWSTR};
use windows::Win32::Foundation::{HMODULE, LUID, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Multithread, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory4};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;

/// Most MFTs read from one enumeration.
const MAX_MFTS: usize = 64;
/// How long one frame may take to go in and come out.
const FRAME_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(1);
/// The output buffer for an MFT that doesn't size its own.
const MIN_OUTPUT: u32 = 1 << 20;

type MfStartup = unsafe extern "system" fn(u32, u32) -> HRESULT;
type MftEnumEx = unsafe extern "system" fn(
    GUID,
    u32,
    *const MFT_REGISTER_TYPE_INFO,
    *const MFT_REGISTER_TYPE_INFO,
    *mut *mut *mut c_void,
    *mut u32,
) -> HRESULT;
type MfCreate = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
type MfCreateMemoryBuffer = unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT;
type MfCreateAlignedMemoryBuffer = unsafe extern "system" fn(u32, u32, *mut *mut c_void) -> HRESULT;
type MfCreateDxgiDeviceManager = unsafe extern "system" fn(*mut u32, *mut *mut c_void) -> HRESULT;

/// mfplat.dll, started for the life of the process.
pub struct Mf {
    _lib: Library,
    enum_ex: MftEnumEx,
    media_type: MfCreate,
    sample: MfCreate,
    memory_buffer: MfCreateMemoryBuffer,
    aligned_buffer: MfCreateAlignedMemoryBuffer,
    device_manager: MfCreateDxgiDeviceManager,
}

static MF: OnceLock<Result<Mf, String>> = OnceLock::new();

/// Joins this thread to the multithreaded apartment, once; a thread already in a
/// single-threaded one stays there.
pub fn com() -> Result<(), String> {
    thread_local!(static JOINED: Result<(), String> = unsafe {
        let r = CoInitializeEx(None, COINIT_MULTITHREADED);
        if r.is_ok() || r == RPC_E_CHANGED_MODE {
            Ok(())
        } else {
            Err(format!("CoInitializeEx: {r}"))
        }
    });
    JOINED.with(Clone::clone)
}

pub fn mf() -> Result<&'static Mf, String> {
    MF.get_or_init(|| unsafe { Mf::load() })
        .as_ref()
        .map_err(Clone::clone)
}

fn err(what: &str, e: windows::core::Error) -> EncodeError {
    EncodeError(format!("{what}: {e}"))
}

unsafe fn owned<T: Interface>(p: *mut c_void, what: &str) -> Result<T, EncodeError> {
    if p.is_null() {
        return Err(EncodeError(format!("{what} returned nothing")));
    }
    Ok(T::from_raw(p))
}

impl Mf {
    unsafe fn load() -> Result<Mf, String> {
        let lib = Library::new("mfplat.dll").map_err(|e| format!("mfplat.dll: {e}"))?;
        let get = |e: libloading::Error| format!("mfplat.dll: {e}");
        let startup: MfStartup = *lib.get(b"MFStartup\0").map_err(get)?;
        let mf = Mf {
            enum_ex: *lib.get(b"MFTEnumEx\0").map_err(get)?,
            media_type: *lib.get(b"MFCreateMediaType\0").map_err(get)?,
            sample: *lib.get(b"MFCreateSample\0").map_err(get)?,
            memory_buffer: *lib.get(b"MFCreateMemoryBuffer\0").map_err(get)?,
            aligned_buffer: *lib.get(b"MFCreateAlignedMemoryBuffer\0").map_err(get)?,
            device_manager: *lib.get(b"MFCreateDXGIDeviceManager\0").map_err(get)?,
            _lib: lib,
        };
        startup(MF_VERSION, MFSTARTUP_LITE)
            .ok()
            .map_err(|e| format!("MFStartup: {e}"))?;
        Ok(mf)
    }

    unsafe fn create<T: Interface>(f: MfCreate, what: &str) -> Result<T, EncodeError> {
        let mut p = std::ptr::null_mut();
        f(&mut p).ok().map_err(|e| err(what, e))?;
        owned(p, what)
    }

    /// The hardware encoder MFTs for `subtype`, best first.
    pub unsafe fn encoders(&self, subtype: GUID) -> Result<Vec<IMFActivate>, String> {
        let out = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: subtype,
        };
        let mut arr: *mut *mut c_void = std::ptr::null_mut();
        let mut n = 0u32;
        let flags = (MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0) as u32;
        (self.enum_ex)(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            std::ptr::null(),
            &out,
            &mut arr,
            &mut n,
        )
        .ok()
        .map_err(|e| format!("MFTEnumEx: {e}"))?;
        if arr.is_null() {
            return Ok(Vec::new());
        }
        let all: Vec<IMFActivate> = (0..n as usize)
            .map(|i| arr.add(i).read())
            .filter(|p| !p.is_null())
            .map(|p| IMFActivate::from_raw(p))
            .collect();
        CoTaskMemFree(Some(arr as *const c_void));
        Ok(all.into_iter().take(MAX_MFTS).collect())
    }
}

pub fn subtype(codec: Codec) -> Option<GUID> {
    match codec {
        Codec::H264 => Some(MFVideoFormat_H264),
        Codec::Hevc => Some(MFVideoFormat_HEVC),
        Codec::Av1 => Some(MFVideoFormat_AV1),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

pub unsafe fn string(a: &IMFActivate, key: &GUID) -> Option<String> {
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

/// Names one MFT across processes: its CLSID and, when it has one, its adapter's LUID.
pub unsafe fn device(a: &IMFActivate) -> String {
    let clsid = a
        .GetGUID(&MFT_TRANSFORM_CLSID_Attribute)
        .map(|g| format!("{g:?}"))
        .unwrap_or_default();
    match a.GetUINT64(&MFT_ENUM_ADAPTER_LUID) {
        Ok(luid) => format!("{clsid}@{luid:016x}"),
        Err(_) => clsid,
    }
}

fn pack(hi: u32, lo: u32) -> u64 {
    (hi as u64) << 32 | lo as u64
}

fn api_guid(api: Api) -> GUID {
    match api {
        Api::LowLatency => CODECAPI_AVLowLatencyMode,
        Api::RateControl => CODECAPI_AVEncCommonRateControlMode,
        Api::MeanBitRate => CODECAPI_AVEncCommonMeanBitRate,
        Api::MaxBitRate => CODECAPI_AVEncCommonMaxBitRate,
        Api::BufferSize => CODECAPI_AVEncCommonBufferSize,
        Api::Qp => CODECAPI_AVEncVideoEncodeQP,
        Api::GopSize => CODECAPI_AVEncMPVGOPSize,
        Api::BFrames => CODECAPI_AVEncMPVDefaultBPictureCount,
        Api::RefFrames => CODECAPI_AVEncVideoMaxNumRefFrame,
        Api::ForceKeyFrame => CODECAPI_AVEncVideoForceKeyFrame,
    }
}

/// A D3D11 device with video support on the adapter `luid` names (the default one
/// without), behind a DXGI device manager for the MFT.
unsafe fn d3d(mf: &Mf, luid: Option<u64>) -> Result<IMFDXGIDeviceManager, EncodeError> {
    let adapter: Option<IDXGIAdapter> = match luid {
        Some(l) => {
            let f: IDXGIFactory4 =
                CreateDXGIFactory1().map_err(|e| err("CreateDXGIFactory1", e))?;
            let luid = LUID {
                LowPart: l as u32,
                HighPart: (l >> 32) as i32,
            };
            Some(
                f.EnumAdapterByLuid(luid)
                    .map_err(|e| err("EnumAdapterByLuid", e))?,
            )
        }
        None => None,
    };
    let driver = if adapter.is_some() {
        D3D_DRIVER_TYPE_UNKNOWN
    } else {
        D3D_DRIVER_TYPE_HARDWARE
    };
    let mut device: Option<ID3D11Device> = None;
    D3D11CreateDevice(
        adapter.as_ref(),
        driver,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        None,
    )
    .map_err(|e| err("D3D11CreateDevice", e))?;
    let device = device.ok_or_else(|| EncodeError("D3D11CreateDevice returned nothing".into()))?;
    let mt: ID3D11Multithread = device
        .GetImmediateContext()
        .and_then(|c| c.cast())
        .map_err(|e| err("ID3D11Multithread", e))?;
    let _ = mt.SetMultithreadProtected(true);
    let mut token = 0u32;
    let mut p = std::ptr::null_mut();
    (mf.device_manager)(&mut token, &mut p)
        .ok()
        .map_err(|e| err("MFCreateDXGIDeviceManager", e))?;
    let manager: IMFDXGIDeviceManager = owned(p, "MFCreateDXGIDeviceManager")?;
    manager
        .ResetDevice(&device, token)
        .map_err(|e| err("IMFDXGIDeviceManager::ResetDevice", e))?;
    Ok(manager)
}

/// One output sample: its time, where the MFT kept it, and its bytes.
type Output = (Option<i64>, Vec<u8>);

/// One hardware MFT, unlocked for asynchronous use and fed system-memory NV12 samples
/// one frame at a time.
pub struct MfDriver {
    mf: &'static Mf,
    mft: Mft,
    events: IMFMediaEventGenerator,
    codec_api: ICodecAPI,
    _manager: IMFDXGIDeviceManager,
    ids: (u32, u32),
    size: (u32, u32),
    fps: u32,
    frame: i64,
    /// `METransformNeedInput` events not yet answered.
    need_input: u32,
    streaming: bool,
}

// The MFT, its D3D device (multithread-protected) and the session are used from one
// thread at a time, each joined to the multithreaded apartment first.
unsafe impl Send for MfDriver {}

impl MfDriver {
    pub fn open(a: &IMFActivate) -> Result<MfDriver, EncodeError> {
        com().map_err(EncodeError)?;
        let mf = mf().map_err(EncodeError)?;
        unsafe {
            let luid = a.GetUINT64(&MFT_ENUM_ADAPTER_LUID).ok();
            let mft = Mft(a
                .ActivateObject()
                .map_err(|e| err("IMFActivate::ActivateObject", e))?);
            let mut d = MfDriver {
                mf,
                events: mft.cast().map_err(|e| err("IMFMediaEventGenerator", e))?,
                codec_api: mft.cast().map_err(|e| err("ICodecAPI", e))?,
                _manager: d3d(mf, luid)?,
                mft,
                ids: (0, 0),
                size: (0, 0),
                fps: 1,
                frame: 0,
                need_input: 0,
                streaming: false,
            };
            let attrs = d
                .mft
                .GetAttributes()
                .map_err(|e| err("IMFTransform::GetAttributes", e))?;
            if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
                return Err(EncodeError("not an asynchronous MFT".into()));
            }
            attrs
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .map_err(|e| err("MF_TRANSFORM_ASYNC_UNLOCK", e))?;
            d.mft
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, d._manager.as_raw() as usize)
                .map_err(|e| err("MFT_MESSAGE_SET_D3D_MANAGER", e))?;
            let (mut i, mut o) = ([0u32], [0u32]);
            if d.mft.GetStreamIDs(&mut i, &mut o).is_ok() {
                d.ids = (i[0], o[0]);
            }
            Ok(d)
        }
    }

    unsafe fn video_type(&self, subtype: &GUID, f: &Format) -> Result<IMFMediaType, EncodeError> {
        let t: IMFMediaType = Mf::create(self.mf.media_type, "MFCreateMediaType")?;
        let set = |r: windows::core::Result<()>| r.map_err(|e| err("IMFMediaType", e));
        set(t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video))?;
        set(t.SetGUID(&MF_MT_SUBTYPE, subtype))?;
        set(t.SetUINT64(&MF_MT_FRAME_SIZE, pack(f.width, f.height)))?;
        set(t.SetUINT64(&MF_MT_FRAME_RATE, pack(f.fps, 1)))?;
        set(t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)))?;
        set(t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32))?;
        set(t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32))?;
        set(t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_sRGB.0 as u32))?;
        set(t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT601.0 as u32))?;
        set(t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32))?;
        Ok(t)
    }

    /// Handles the next transform event: a need-input credit, or the output one
    /// announces with its sample time. An error once `deadline` passes.
    unsafe fn pump(&mut self, deadline: Instant) -> Result<Option<Output>, EncodeError> {
        loop {
            if Instant::now() >= deadline {
                return Err(EncodeError(format!(
                    "Media Foundation MFT took longer than {FRAME_TIMEOUT:?} for a frame"
                )));
            }
            let ev = match self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                Ok(ev) => ev,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    std::thread::sleep(POLL);
                    continue;
                }
                Err(e) => return Err(err("IMFMediaEventGenerator::GetEvent", e)),
            };
            let kind = ev.GetType().map_err(|e| err("IMFMediaEvent::GetType", e))?;
            if let Ok(status) = ev.GetStatus() {
                status
                    .ok()
                    .map_err(|e| EncodeError(format!("MFT event {kind}: {e}")))?;
            }
            let kind = MF_EVENT_TYPE(kind as i32);
            if kind == METransformNeedInput {
                self.need_input += 1;
                return Ok(None);
            } else if kind == METransformHaveOutput {
                return self.output().map(Some);
            } else if kind == MEError {
                return Err(EncodeError("MFT raised MEError".into()));
            }
        }
    }

    unsafe fn output(&mut self) -> Result<Output, EncodeError> {
        let info = self
            .mft
            .GetOutputStreamInfo(self.ids.1)
            .map_err(|e| err("IMFTransform::GetOutputStreamInfo", e))?;
        let provides =
            (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
        let sample = if info.dwFlags & provides != 0 {
            None
        } else {
            Some(
                self.sample(info.cbSize.max(MIN_OUTPUT), info.cbAlignment)?
                    .0,
            )
        };
        let mut out = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.ids.1,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let r = self.mft.ProcessOutput(0, &mut out, &mut status);
        let sample = ManuallyDrop::take(&mut out[0].pSample);
        drop(ManuallyDrop::take(&mut out[0].pEvents));
        match r {
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                let t = self
                    .mft
                    .GetOutputAvailableType(self.ids.1, 0)
                    .map_err(|e| err("IMFTransform::GetOutputAvailableType", e))?;
                self.mft
                    .SetOutputType(self.ids.1, &t, 0)
                    .map_err(|e| err("IMFTransform::SetOutputType", e))?;
                return Ok((None, Vec::new()));
            }
            Err(e) => return Err(err("IMFTransform::ProcessOutput", e)),
            Ok(()) => {}
        }
        let sample = sample
            .ok_or_else(|| EncodeError("IMFTransform::ProcessOutput gave no sample".into()))?;
        let buf = sample
            .ConvertToContiguousBuffer()
            .map_err(|e| err("IMFSample::ConvertToContiguousBuffer", e))?;
        let mut p = std::ptr::null_mut();
        let mut n = 0u32;
        buf.Lock(&mut p, None, Some(&mut n as *mut u32))
            .map_err(|e| err("IMFMediaBuffer::Lock", e))?;
        let data = if p.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(p, n as usize).to_vec()
        };
        let _ = buf.Unlock();
        Ok((sample.GetSampleTime().ok(), data))
    }

    /// A sample with one `size`-byte memory buffer aligned to `align` bytes.
    unsafe fn sample(
        &self,
        size: u32,
        align: u32,
    ) -> Result<(IMFSample, IMFMediaBuffer), EncodeError> {
        let mut p = std::ptr::null_mut();
        let r = if align > 1 {
            (self.mf.aligned_buffer)(size, align - 1, &mut p)
        } else {
            (self.mf.memory_buffer)(size, &mut p)
        };
        r.ok().map_err(|e| err("MFCreateMemoryBuffer", e))?;
        let buf: IMFMediaBuffer = owned(p, "MFCreateMemoryBuffer")?;
        let s: IMFSample = Mf::create(self.mf.sample, "MFCreateSample")?;
        s.AddBuffer(&buf)
            .map_err(|e| err("IMFSample::AddBuffer", e))?;
        Ok((s, buf))
    }

    unsafe fn input(&self, f: &FrameView<'_>) -> Result<IMFSample, EncodeError> {
        let (w, h) = (self.size.0 as usize, self.size.1 as usize);
        let len = w * h + w * h.div_ceil(2);
        let (s, buf) = self.sample(len as u32, 0)?;
        let mut p = std::ptr::null_mut();
        buf.Lock(&mut p, None, None)
            .map_err(|e| err("IMFMediaBuffer::Lock", e))?;
        let r = if p.is_null() {
            Err(EncodeError(
                "IMFMediaBuffer::Lock returned no memory".into(),
            ))
        } else {
            let dst = std::slice::from_raw_parts_mut(p, len);
            write_nv12(dst, [w, w], [0, w * h], self.size, f)
        };
        let _ = buf.Unlock();
        r?;
        buf.SetCurrentLength(len as u32)
            .map_err(|e| err("IMFMediaBuffer::SetCurrentLength", e))?;
        let tick = 10_000_000 / self.fps as i64;
        s.SetSampleTime(self.frame * tick)
            .map_err(|e| err("IMFSample::SetSampleTime", e))?;
        s.SetSampleDuration(tick)
            .map_err(|e| err("IMFSample::SetSampleDuration", e))?;
        Ok(s)
    }
}

impl Drop for MfDriver {
    fn drop(&mut self) {
        unsafe {
            if self.streaming {
                let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            }
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
        }
    }
}

/// An activated MFT, shut down when dropped.
struct Mft(IMFTransform);

impl std::ops::Deref for Mft {
    type Target = IMFTransform;

    fn deref(&self) -> &IMFTransform {
        &self.0
    }
}

impl Drop for Mft {
    fn drop(&mut self) {
        if let Ok(s) = self.0.cast::<IMFShutdown>() {
            let _ = unsafe { s.Shutdown() };
        }
    }
}

impl Driver for MfDriver {
    fn set_types(&mut self, f: &Format) -> Result<(), EncodeError> {
        com().map_err(EncodeError)?;
        let subtype = subtype(f.codec)
            .ok_or_else(|| EncodeError(format!("Media Foundation has no {:?}", f.codec)))?;
        unsafe {
            let out = self.video_type(&subtype, f)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, f.bps)
                .map_err(|e| err("MF_MT_AVG_BITRATE", e))?;
            let profile = match f.codec {
                Codec::H264 => eAVEncH264VProfile_High.0,
                Codec::Hevc => eAVEncH265VProfile_Main_420_8.0,
                _ => eAVEncAV1VProfile_Main_420_8.0,
            };
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, profile as u32)
                .map_err(|e| err("MF_MT_MPEG2_PROFILE", e))?;
            self.mft
                .SetOutputType(self.ids.1, &out, 0)
                .map_err(|e| err("IMFTransform::SetOutputType", e))?;
            let input = self.video_type(&MFVideoFormat_NV12, f)?;
            input
                .SetUINT32(&MF_MT_DEFAULT_STRIDE, f.width)
                .map_err(|e| err("MF_MT_DEFAULT_STRIDE", e))?;
            self.mft
                .SetInputType(self.ids.0, &input, 0)
                .map_err(|e| err("IMFTransform::SetInputType", e))?;
        }
        self.size = (f.width, f.height);
        self.fps = f.fps.max(1);
        Ok(())
    }

    fn set(&mut self, api: Api, v: Value) -> Result<(), EncodeError> {
        com().map_err(EncodeError)?;
        let v = match v {
            Value::Bool(v) => VARIANT::from(v),
            Value::U32(v) => VARIANT::from(v),
            Value::U64(v) => VARIANT::from(v),
        };
        unsafe { self.codec_api.SetValue(&api_guid(api), &v) }
            .map_err(|e| err("ICodecAPI::SetValue", e))
    }

    fn begin(&mut self) -> Result<(), EncodeError> {
        com().map_err(EncodeError)?;
        unsafe {
            self.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| err("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING", e))?;
            self.streaming = true;
            self.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| err("MFT_MESSAGE_NOTIFY_START_OF_STREAM", e))
        }
    }

    /// One frame in, its output back: the MFT asks for the input, codes it and announces
    /// the output within [`FRAME_TIMEOUT`], or the frame fails. Output left over from a
    /// failed frame (another or no sample time) is dropped.
    fn encode(&mut self, f: &FrameView<'_>) -> Result<Vec<u8>, EncodeError> {
        if (f.width, f.height) != self.size {
            return Err(EncodeError(format!(
                "Media Foundation input is {}x{}, got {}x{}",
                self.size.0, self.size.1, f.width, f.height
            )));
        }
        com().map_err(EncodeError)?;
        let deadline = Instant::now() + FRAME_TIMEOUT;
        unsafe {
            let sample = self.input(f)?;
            let time = sample
                .GetSampleTime()
                .map_err(|e| err("IMFSample::GetSampleTime", e))?;
            while self.need_input == 0 {
                self.pump(deadline)?;
            }
            self.mft
                .ProcessInput(self.ids.0, &sample, 0)
                .map_err(|e| err("IMFTransform::ProcessInput", e))?;
            self.need_input -= 1;
            self.frame += 1;
            loop {
                if let Some((t, d)) = self.pump(deadline)? {
                    if !d.is_empty() && t == Some(time) {
                        return Ok(d);
                    }
                }
            }
        }
    }
}
