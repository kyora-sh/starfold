//! VideoToolbox's hardware H.264 and HEVC encoders: real time, no frame reordering,
//! IDRs on request, the length-prefixed output rewritten to Annex B with the parameter
//! sets in front of every IDR.

mod ffi;

use self::ffi::*;
use super::caps::{
    self, Acceleration, Chroma, EncoderCapability, EncoderLimits, PixelFormat, ProbeStatus,
    ProfileCapability,
};
use super::h264::{nal_units, ParamSets};
use super::{
    hevc, AccessUnit, Backend, BitstreamFormat, Codec, ColorInfo, EncodeError, EncodeOptions,
    EncoderConfig, FrameView, RateConfig, StreamConfig, VideoEncoder,
};
use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use std::time::{Duration, Instant};

/// What an uncapped rate (`max_kbps` 0) asks for.
const UNCAPPED_BPS: i64 = 100_000_000;
/// How long one frame may take to come out.
const OUTPUT_TIMEOUT: Duration = Duration::from_secs(1);

/// An owned CoreFoundation reference.
struct Cf(CFTypeRef);

// CoreFoundation's reference counting is thread-safe, and nothing here mutates the objects.
unsafe impl Send for Cf {}

impl Drop for Cf {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}

fn err(what: &str, r: OSStatus) -> EncodeError {
    EncodeError(format!("VideoToolbox {what} failed ({r})"))
}

fn owned(r: CFTypeRef, what: &str) -> Result<Cf, EncodeError> {
    if r.is_null() {
        Err(EncodeError(format!("can't create {what}")))
    } else {
        Ok(Cf(r))
    }
}

fn number<T>(ty: isize, v: T) -> Result<Cf, EncodeError> {
    owned(
        unsafe { CFNumberCreate(ptr::null(), ty, &v as *const T as *const c_void) },
        "a CFNumber",
    )
}

fn dict(pairs: &[(CFStringRef, CFTypeRef)]) -> Result<Cf, EncodeError> {
    let keys: Vec<CFTypeRef> = pairs.iter().map(|p| p.0).collect();
    let values: Vec<CFTypeRef> = pairs.iter().map(|p| p.1).collect();
    owned(
        unsafe {
            CFDictionaryCreate(
                ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                pairs.len() as isize,
                &kCFTypeDictionaryKeyCallBacks as *const _ as *const c_void,
                &kCFTypeDictionaryValueCallBacks as *const _ as *const c_void,
            )
        },
        "a CFDictionary",
    )
}

unsafe fn get_bool(v: CFTypeRef) -> Option<bool> {
    (!v.is_null() && CFGetTypeID(v) == CFBooleanGetTypeID()).then(|| CFBooleanGetValue(v))
}

unsafe fn get_i64(v: CFTypeRef) -> Option<i64> {
    let mut n = 0i64;
    (!v.is_null()
        && CFGetTypeID(v) == CFNumberGetTypeID()
        && CFNumberGetValue(v, kCFNumberSInt64Type, &mut n as *mut i64 as *mut c_void))
    .then_some(n)
}

unsafe fn get_string(v: CFTypeRef) -> Option<String> {
    (!v.is_null() && CFGetTypeID(v) == CFStringGetTypeID()).then(|| cf_string(v))
}

/// One encoder output, as the callback got it.
struct Encoded {
    status: OSStatus,
    dropped: bool,
    pts: i64,
    sample: Option<Cf>,
}

#[derive(Default)]
struct Output {
    queue: Mutex<VecDeque<Encoded>>,
    ready: Condvar,
}

/// Runs on VideoToolbox's thread, or on ours inside `VTCompressionSessionEncodeFrame`.
unsafe extern "C" fn on_output(
    refcon: *mut c_void,
    frame: *mut c_void,
    status: OSStatus,
    flags: u32,
    sample: CMSampleBufferRef,
) {
    let out = &*(refcon as *const Output);
    let sample = (!sample.is_null()).then(|| Cf(CFRetain(sample)));
    out.queue.lock().push_back(Encoded {
        status,
        dropped: flags & kVTEncodeInfo_FrameDropped != 0,
        pts: frame as usize as i64,
        sample,
    });
    out.ready.notify_all();
}

/// A compression session on a hardware encoder; `out` outlives every callback.
struct Session {
    s: VTCompressionSessionRef,
    out: Box<Output>,
}

impl Session {
    fn open(codec: u32, width: u32, height: u32, low_latency: bool) -> Result<Session, OSStatus> {
        let mut spec = vec![(
            unsafe { kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder },
            unsafe { kCFBooleanTrue },
        )];
        if low_latency {
            spec.push(unsafe {
                (
                    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
                    kCFBooleanTrue,
                )
            });
        }
        let cf = |r: Result<Cf, EncodeError>| r.map_err(|_| -108);
        let spec = cf(dict(&spec))?;
        let fmt = cf(number(
            kCFNumberSInt32Type,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32,
        ))?;
        let (w, h) = (
            cf(number(kCFNumberSInt32Type, width as i32))?,
            cf(number(kCFNumberSInt32Type, height as i32))?,
        );
        let surface = cf(dict(&[]))?;
        let attrs = cf(dict(unsafe {
            &[
                (kCVPixelBufferPixelFormatTypeKey, fmt.0),
                (kCVPixelBufferWidthKey, w.0),
                (kCVPixelBufferHeightKey, h.0),
                (kCVPixelBufferIOSurfacePropertiesKey, surface.0),
            ]
        }))?;
        let out = Box::new(Output::default());
        let mut s = ptr::null_mut();
        let r = unsafe {
            VTCompressionSessionCreate(
                ptr::null(),
                width as i32,
                height as i32,
                codec,
                spec.0,
                attrs.0,
                ptr::null(),
                Some(on_output),
                &*out as *const Output as *mut c_void,
                &mut s,
            )
        };
        if r != 0 || s.is_null() {
            return Err(if r != 0 { r } else { -1 });
        }
        Ok(Session { s, out })
    }

    fn set(&self, key: CFStringRef, value: CFTypeRef) -> Result<(), OSStatus> {
        match unsafe { VTSessionSetProperty(self.s, key, value) } {
            0 => Ok(()),
            r => Err(r),
        }
    }

    fn require(&self, key: CFStringRef, value: CFTypeRef, what: &str) -> Result<(), EncodeError> {
        self.set(key, value).map_err(|r| err(what, r))
    }

    fn copy(&self, key: CFStringRef) -> Option<Cf> {
        let mut v = ptr::null();
        let r = unsafe { VTSessionCopyProperty(self.s, key, ptr::null(), &mut v) };
        (r == 0 && !v.is_null()).then(|| Cf(v))
    }

    /// Whether the session says it runs on hardware; paravirtualized encoders (a macOS
    /// guest's) don't say, and the session was created requiring hardware anyway.
    fn software(&self) -> bool {
        self.copy(unsafe { kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder })
            .and_then(|v| unsafe { get_bool(v.0) })
            == Some(false)
    }

    fn encoder_id(&self) -> Option<String> {
        self.copy(unsafe { kVTCompressionPropertyKey_EncoderID })
            .and_then(|v| unsafe { get_string(v.0) })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            VTCompressionSessionCompleteFrames(self.s, kCMTimeInvalid);
            VTCompressionSessionInvalidate(self.s);
            CFRelease(self.s);
        }
    }
}

pub struct VideoToolbox {
    session: Session,
    codec: Codec,
    width: u32,
    height: u32,
    h264: ParamSets,
    hevc_sps: Option<Vec<u8>>,
    pts: i64,
    bps: i64,
    /// A frame's output was lost: the next one has to be an IDR.
    resync: bool,
}

// The session is only ever driven from one thread at a time; its callback only touches
// `Output`, which is Sync.
unsafe impl Send for VideoToolbox {}

fn codec_type(codec: Codec) -> u32 {
    match codec {
        Codec::H264 => kCMVideoCodecType_H264,
        Codec::Hevc => kCMVideoCodecType_HEVC,
        Codec::Av1 => kCMVideoCodecType_AV1,
        #[allow(unreachable_patterns)]
        _ => 0,
    }
}

fn bps(max_kbps: u32) -> i64 {
    if max_kbps == 0 {
        UNCAPPED_BPS
    } else {
        (max_kbps as i64 * 1000).clamp(1000, UNCAPPED_BPS)
    }
}

impl VideoToolbox {
    pub fn new(cfg: &EncoderConfig) -> Result<Self, EncodeError> {
        let profile = match cfg.codec {
            Codec::H264 => unsafe { kVTProfileLevel_H264_High_AutoLevel },
            Codec::Hevc => unsafe { kVTProfileLevel_HEVC_Main_AutoLevel },
            other => return Err(EncodeError(format!("no VideoToolbox {other:?} encoder"))),
        };
        let ty = codec_type(cfg.codec);
        // Low-latency rate control: no lookahead or reordering, one IDR then P frames.
        let session = Session::open(ty, cfg.width, cfg.height, true).map_err(|r| {
            EncodeError(format!(
                "no low-latency hardware VideoToolbox {:?} encoder for {}x{}: {}",
                cfg.codec,
                cfg.width,
                cfg.height,
                session_error(r)
            ))
        })?;
        let s = &session;
        unsafe {
            s.require(
                kVTCompressionPropertyKey_RealTime,
                kCFBooleanTrue,
                "real-time mode",
            )?;
            s.require(
                kVTCompressionPropertyKey_AllowFrameReordering,
                kCFBooleanFalse,
                "turning frame reordering off",
            )?;
            s.require(
                kVTCompressionPropertyKey_ProfileLevel,
                profile,
                "setting the profile",
            )?;
            s.require(
                kVTCompressionPropertyKey_ColorPrimaries,
                kCVImageBufferColorPrimaries_ITU_R_709_2,
                "setting the colour primaries",
            )?;
            s.require(
                kVTCompressionPropertyKey_TransferFunction,
                kCVImageBufferTransferFunction_sRGB,
                "setting the transfer function",
            )?;
            s.require(
                kVTCompressionPropertyKey_YCbCrMatrix,
                kCVImageBufferYCbCrMatrix_ITU_R_601_4,
                "setting the YCbCr matrix",
            )?;
            let fps = number(kCFNumberDoubleType, cfg.fps.max(1) as f64)?;
            if let Err(r) = s.set(kVTCompressionPropertyKey_ExpectedFrameRate, fps.0) {
                log::debug!("VideoToolbox ignores the expected frame rate ({r})");
            }
        }
        let mut me = VideoToolbox {
            session,
            codec: cfg.codec,
            width: cfg.width,
            height: cfg.height,
            h264: ParamSets::default(),
            hevc_sps: None,
            pts: -1,
            bps: 0,
            resync: false,
        };
        me.set_bps(bps(cfg.rate.max_kbps))?;
        let r = unsafe { VTCompressionSessionPrepareToEncodeFrames(me.session.s) };
        if r != 0 {
            return Err(err("preparing to encode", r));
        }
        if me.session.software() {
            return Err(EncodeError(
                "VideoToolbox opened a software encoder, not the hardware one".into(),
            ));
        }
        Ok(me)
    }

    fn set_bps(&mut self, bps: i64) -> Result<(), EncodeError> {
        let n = number(kCFNumberSInt64Type, bps)?;
        self.session
            .require(
                unsafe { kVTCompressionPropertyKey_AverageBitRate },
                n.0,
                "setting the bitrate",
            )
            .map(|_| self.bps = bps)
    }

    /// Copies an I420 picture into a pooled NV12 buffer.
    fn upload(&self, f: &FrameView<'_>) -> Result<Cf, EncodeError> {
        let pool = unsafe { VTCompressionSessionGetPixelBufferPool(self.session.s) };
        if pool.is_null() {
            return Err(EncodeError("VideoToolbox has no pixel buffer pool".into()));
        }
        let mut pb = ptr::null_mut();
        let r = unsafe { CVPixelBufferPoolCreatePixelBuffer(ptr::null(), pool, &mut pb) };
        if r != 0 || pb.is_null() {
            return Err(err("allocating a pixel buffer", r));
        }
        let pb = Cf(pb as CFTypeRef);
        let raw = pb.0 as CVPixelBufferRef;
        if unsafe { CVPixelBufferLockBaseAddress(raw, 0) } != 0 {
            return Err(EncodeError("can't lock the pixel buffer".into()));
        }
        let copied = unsafe { copy_nv12(raw, f) };
        unsafe { CVPixelBufferUnlockBaseAddress(raw, 0) };
        copied.map(|_| pb)
    }

    fn collect(&mut self, items: Vec<Encoded>) -> Result<Vec<AccessUnit>, EncodeError> {
        let mut aus = Vec::new();
        for e in items {
            if e.status != 0 {
                return Err(err("encoding", e.status));
            }
            let Some(sample) = e.sample.filter(|_| !e.dropped) else {
                continue;
            };
            if let Some(au) = unsafe { self.unit(sample.0, e.pts)? } {
                aus.push(au);
            }
        }
        Ok(aus)
    }

    /// One sample buffer as an Annex-B access unit, `None` if it holds no picture.
    unsafe fn unit(
        &mut self,
        sample: CMSampleBufferRef,
        pts: i64,
    ) -> Result<Option<AccessUnit>, EncodeError> {
        let bb = CMSampleBufferGetDataBuffer(sample);
        let desc = CMSampleBufferGetFormatDescription(sample);
        if bb.is_null() || desc.is_null() {
            return Ok(None);
        }
        let len = CMBlockBufferGetDataLength(bb);
        let mut raw = vec![0u8; len];
        if len > 0 {
            let r = CMBlockBufferCopyDataBytes(bb, 0, len, raw.as_mut_ptr() as *mut c_void);
            if r != 0 {
                return Err(err("reading the encoded frame", r));
            }
        }
        let sync = !not_sync(sample);
        let (sets, nal_len) = parameter_sets(self.codec, desc, sync)?;
        let mut data = Vec::with_capacity(len + sets.iter().map(|s| s.len() + 4).sum::<usize>());
        for s in &sets {
            data.extend_from_slice(&[0, 0, 0, 1]);
            data.extend_from_slice(s);
        }
        annex_b(&raw, nal_len, &mut data)?;
        let coded = (self.width, self.height);
        let color = ColorInfo::SRGB_BT601_LIMITED;
        Ok(match self.codec {
            Codec::H264 => self.h264.unit(data, pts, coded, color),
            _ => self.hevc_unit(data, pts, coded, color),
        })
    }

    fn hevc_unit(
        &mut self,
        data: Vec<u8>,
        pts_ms: i64,
        coded: (u32, u32),
        color: ColorInfo,
    ) -> Option<AccessUnit> {
        let (mut sps, mut vps, mut pps, mut key, mut slice) = (None, false, false, false, false);
        for n in nal_units(&data) {
            match hevc::nal_type(n) {
                hevc::NAL_VPS => vps = true,
                hevc::NAL_SPS => sps = Some(n.to_vec()),
                hevc::NAL_PPS => pps = true,
                t if hevc::is_random_access(t) => (key, slice) = (true, true),
                t if hevc::is_slice(t) => slice = true,
                _ => {}
            }
        }
        if !slice {
            return None;
        }
        if key && !(vps && pps && sps.is_some()) {
            log::error!("HEVC random-access picture without parameter sets; dropped");
            return None;
        }
        let changed = sps.is_some() && sps != self.hevc_sps;
        if sps.is_some() {
            self.hevc_sps = sps;
        }
        let config = changed
            .then(|| self.hevc_sps.as_deref().and_then(hevc::codec_string))
            .flatten()
            .map(|codec_string| StreamConfig {
                codec: Codec::Hevc,
                codec_string,
                coded,
                display: coded,
                format: BitstreamFormat::AnnexB,
                color,
            });
        Some(AccessUnit {
            pts_ms,
            keyframe: key,
            data,
            config,
        })
    }
}

/// Whether the sample is marked as depending on others.
unsafe fn not_sync(sample: CMSampleBufferRef) -> bool {
    let a = CMSampleBufferGetSampleAttachmentsArray(sample, false);
    if a.is_null() || CFArrayGetCount(a) < 1 {
        return false;
    }
    let d = CFArrayGetValueAtIndex(a, 0);
    if d.is_null() || CFGetTypeID(d) != CFDictionaryGetTypeID() {
        return false;
    }
    get_bool(CFDictionaryGetValue(d, kCMSampleAttachmentKey_NotSync)) == Some(true)
}

type GetParameterSet = unsafe extern "C" fn(
    CFTypeRef,
    usize,
    *mut *const u8,
    *mut usize,
    *mut usize,
    *mut i32,
) -> OSStatus;

/// The NAL length size of the stream and, with `want`, its parameter sets (SPS and
/// PPS; VPS, SPS and PPS for HEVC).
unsafe fn parameter_sets(
    codec: Codec,
    desc: CFTypeRef,
    want: bool,
) -> Result<(Vec<Vec<u8>>, usize), EncodeError> {
    let get: GetParameterSet = match codec {
        Codec::H264 => CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
        _ => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
    };
    let (mut count, mut nal_len) = (0usize, 0i32);
    let r = get(
        desc,
        0,
        ptr::null_mut(),
        ptr::null_mut(),
        &mut count,
        &mut nal_len,
    );
    if r != 0 {
        return Err(err("reading the parameter sets", r));
    }
    if !matches!(nal_len, 1 | 2 | 4) || count > 16 {
        return Err(EncodeError(format!(
            "VideoToolbox format with {count} parameter sets and {nal_len}-byte NAL lengths"
        )));
    }
    let mut sets = Vec::new();
    for i in 0..if want { count } else { 0 } {
        let (mut p, mut n) = (ptr::null(), 0usize);
        let r = get(desc, i, &mut p, &mut n, ptr::null_mut(), ptr::null_mut());
        if r != 0 || p.is_null() || n == 0 {
            return Err(err("reading a parameter set", r));
        }
        sets.push(std::slice::from_raw_parts(p, n).to_vec());
    }
    Ok((sets, nal_len as usize))
}

/// Appends length-prefixed NAL units to `out` with start codes instead.
fn annex_b(raw: &[u8], nal_len: usize, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    let bad = || EncodeError("VideoToolbox output with a truncated NAL unit".into());
    let mut i = 0;
    while i < raw.len() {
        let head = raw.get(i..i + nal_len).ok_or_else(bad)?;
        let n = head.iter().fold(0usize, |a, &b| a << 8 | b as usize);
        let body = raw[i + nal_len..].get(..n).ok_or_else(bad)?;
        if n > 0 {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(body);
        }
        i += nal_len + n;
    }
    Ok(())
}

/// Plane `i` of a locked buffer, if it holds `w`x`h` samples of `bytes` a row.
unsafe fn plane<'a>(
    pb: CVPixelBufferRef,
    i: usize,
    w: usize,
    h: usize,
    bytes: usize,
) -> Result<&'a mut [u8], EncodeError> {
    let base = CVPixelBufferGetBaseAddressOfPlane(pb, i) as *mut u8;
    let bpr = CVPixelBufferGetBytesPerRowOfPlane(pb, i);
    let (pw, ph) = (
        CVPixelBufferGetWidthOfPlane(pb, i),
        CVPixelBufferGetHeightOfPlane(pb, i),
    );
    match bpr.checked_mul(ph) {
        Some(len) if !base.is_null() && pw >= w && ph >= h && bpr >= bytes => {
            Ok(std::slice::from_raw_parts_mut(base, len))
        }
        _ => Err(EncodeError(format!(
            "VideoToolbox plane {i} is {pw}x{ph} ({bpr} bytes a row), need {w}x{h}"
        ))),
    }
}

/// Interleaves `f`'s chroma into a locked NV12 buffer at least as large as the picture.
unsafe fn copy_nv12(pb: CVPixelBufferRef, f: &FrameView<'_>) -> Result<(), EncodeError> {
    if CVPixelBufferGetPlaneCount(pb) != 2 {
        return Err(EncodeError("VideoToolbox pixel buffer is not NV12".into()));
    }
    let (w, h) = (f.width as usize, f.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let y = plane(pb, 0, w, h, w)?;
    let bpr = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
    for r in 0..h {
        y[r * bpr..][..w].copy_from_slice(&f.y[r * f.stride_y..][..w]);
    }
    let uv = plane(pb, 1, cw, ch, 2 * cw)?;
    let bpr = CVPixelBufferGetBytesPerRowOfPlane(pb, 1);
    for r in 0..ch {
        let (u, v) = (&f.u[r * f.stride_uv..][..cw], &f.v[r * f.stride_uv..][..cw]);
        for (d, (&u, &v)) in uv[r * bpr..][..2 * cw]
            .chunks_exact_mut(2)
            .zip(u.iter().zip(v))
        {
            d[0] = u;
            d[1] = v;
        }
    }
    Ok(())
}

impl VideoEncoder for VideoToolbox {
    fn set_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError> {
        let b = bps(rate.max_kbps);
        if b == self.bps {
            return Ok(());
        }
        self.set_bps(b)
    }

    fn encode(
        &mut self,
        f: &FrameView<'_>,
        opts: EncodeOptions<'_>,
    ) -> Result<Vec<AccessUnit>, EncodeError> {
        if (f.width, f.height) != (self.width, self.height) {
            return Err(EncodeError(format!(
                "VideoToolbox opened at {}x{}, got {}x{}",
                self.width, self.height, f.width, f.height
            )));
        }
        f.check()?;
        self.pts = opts.pts_ms.max(self.pts + 1);
        let pb = self.upload(f)?;
        let props = if opts.force_keyframe || self.resync {
            Some(dict(unsafe {
                &[(kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue)]
            })?)
        } else {
            None
        };
        let pts = CMTime {
            value: self.pts,
            timescale: 1000,
            flags: kCMTimeFlags_Valid,
            epoch: 0,
        };
        let mut info = 0u32;
        let r = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session.s,
                pb.0 as CVPixelBufferRef,
                pts,
                kCMTimeInvalid,
                props.as_ref().map_or(ptr::null(), |p| p.0),
                self.pts as usize as *mut c_void,
                &mut info,
            )
        };
        drop(pb);
        if r != 0 {
            return Err(err("submitting a frame", r));
        }
        self.resync = false;
        let deadline = Instant::now() + OUTPUT_TIMEOUT;
        let out = &self.session.out;
        let mut q = out.queue.lock();
        while !q.iter().any(|e| e.pts == self.pts) {
            if out.ready.wait_until(&mut q, deadline).timed_out() {
                break;
            }
        }
        let late = !q.iter().any(|e| e.pts == self.pts);
        drop(q);
        if late {
            // Blocks until the frame is out, so it can't turn up in a later call.
            let r = unsafe { VTCompressionSessionCompleteFrames(self.session.s, pts) };
            if r != 0 {
                log::warn!("VideoToolbox: completing a slow frame failed ({r})");
            }
        }
        let items: Vec<Encoded> = out.queue.lock().drain(..).collect();
        // One picture per call: anything else is from a frame already given up on, and
        // what follows it can't be decoded without it.
        let (mine, stale): (Vec<Encoded>, Vec<Encoded>) =
            items.into_iter().partition(|e| e.pts == self.pts);
        if !stale.is_empty() || mine.is_empty() {
            self.resync = true;
        }
        if !stale.is_empty() {
            log::warn!(
                "VideoToolbox: dropped {} late frame(s); the next frame is an IDR",
                stale.len()
            );
        }
        if mine.is_empty() {
            return Err(EncodeError(format!(
                "VideoToolbox returned no frame within {} ms",
                OUTPUT_TIMEOUT.as_millis()
            )));
        }
        let aus = self.collect(mine);
        if aus.is_err() {
            self.resync = true;
        }
        aus
    }

    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        let r = unsafe { VTCompressionSessionCompleteFrames(self.session.s, kCMTimeInvalid) };
        if r != 0 {
            return Err(err("draining", r));
        }
        let items: Vec<Encoded> = self.session.out.queue.lock().drain(..).collect();
        self.collect(items)
    }
}

/// A listed encoder.
struct Listed {
    codec: u32,
    id: String,
    hardware: bool,
}

fn encoder_list() -> Result<Vec<Listed>, OSStatus> {
    let mut a = ptr::null();
    let r = unsafe { VTCopyVideoEncoderList(ptr::null(), &mut a) };
    if r != 0 || a.is_null() {
        return Err(r);
    }
    let a = Cf(a);
    let n = unsafe { CFArrayGetCount(a.0) }.clamp(0, 1024);
    Ok((0..n)
        .filter_map(|i| unsafe {
            let d = CFArrayGetValueAtIndex(a.0, i);
            if d.is_null() || CFGetTypeID(d) != CFDictionaryGetTypeID() {
                return None;
            }
            let get = |k: CFStringRef| CFDictionaryGetValue(d, k);
            Some(Listed {
                codec: get_i64(get(kVTVideoEncoderList_CodecType))? as u32,
                id: get_string(get(kVTVideoEncoderList_EncoderID)).unwrap_or_default(),
                hardware: get_bool(get(kVTVideoEncoderList_IsHardwareAccelerated)) == Some(true),
            })
        })
        .collect())
}

/// What VideoToolbox says the probe's sample session ran on.
struct Sampled {
    encoder_id: Option<String>,
}

/// Opens the codec's hardware encoder and, where this build has one, encodes a few
/// frames through it.
fn verify(codec: Codec) -> Result<Sampled, String> {
    let (w, h) = (640, 360);
    if !Backend::VideoToolbox.implements(codec) {
        let s = Session::open(codec_type(codec), w, h, false).map_err(session_error)?;
        if s.software() {
            return Err("the session is not on the hardware encoder".into());
        }
        return Ok(Sampled {
            encoder_id: s.encoder_id(),
        });
    }
    let mut enc = VideoToolbox::new(&EncoderConfig {
        backend: Backend::VideoToolbox,
        codec,
        width: w,
        height: h,
        fps: 30,
        threads: 1,
        rate: RateConfig::new(24.0, 4000, 30),
        best_quality: 24.0,
        preset: String::new(),
    })?;
    let (y, uv) = (
        vec![128u8; (w * h) as usize],
        vec![128u8; (w * h / 4) as usize],
    );
    let frame = FrameView {
        width: w,
        height: h,
        y: &y,
        u: &uv,
        v: &uv,
        stride_y: w as usize,
        stride_uv: w as usize / 2,
    };
    let mut aus = Vec::new();
    for f in 0..3 {
        aus.extend(enc.encode(
            &frame,
            EncodeOptions {
                pts_ms: f * 33,
                force_keyframe: f == 0,
                static_mbs: None,
            },
        )?);
    }
    aus.extend(enc.drain()?);
    if !aus
        .first()
        .is_some_and(|a| a.keyframe && a.config.is_some())
    {
        return Err("the sample encode produced no IDR".into());
    }
    Ok(Sampled {
        encoder_id: enc.session.encoder_id(),
    })
}

fn session_error(r: OSStatus) -> String {
    match r {
        -12908 => "no hardware encoder (kVTCouldNotFindVideoEncoderErr)".into(),
        r => format!("VTCompressionSessionCreate failed ({r})"),
    }
}

pub struct Probe;

impl caps::ProbeProvider for Probe {
    fn backend(&self) -> Backend {
        Backend::VideoToolbox
    }

    fn probe(&self) -> caps::Probe {
        let mut out = caps::Probe::default();
        let listed = encoder_list().unwrap_or_else(|r| {
            out.note(
                Backend::VideoToolbox,
                None,
                format!("VTCopyVideoEncoderList failed ({r})"),
            );
            Vec::new()
        });
        for (codec, profile) in [
            (Codec::H264, "high"),
            (Codec::Hevc, "main"),
            (Codec::Av1, "main"),
        ] {
            let ty = codec_type(codec);
            let listed_id = listed
                .iter()
                .find(|l| l.codec == ty && l.hardware)
                .map(|l| l.id.clone());
            let mut c = EncoderCapability::new(
                codec,
                Backend::VideoToolbox,
                Acceleration::Hardware,
                listed_id.unwrap_or_else(|| "none listed".into()),
            );
            c.profiles = vec![ProfileCapability::new(profile, 8, Chroma::Yuv420)];
            c.limits = EncoderLimits {
                alignment: Some(2),
                pixel_formats: Some(vec![PixelFormat::Nv12]),
                ..Default::default()
            };
            match verify(codec) {
                Ok(v) => {
                    if let Some(id) = v.encoder_id {
                        c.device_id = id;
                    }
                    if Backend::VideoToolbox.implements(codec) {
                        c.low_latency = Some(true);
                        c.keyframe_control = Some(true);
                    }
                    c.status = ProbeStatus::Verified;
                }
                Err(e) => c.status = ProbeStatus::Unavailable(e),
            }
            out.caps.push(c);
        }
        out
    }
}
