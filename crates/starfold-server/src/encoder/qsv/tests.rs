use super::driver::{Fault, Runtime};
use super::ffi::{
    self, Bitstream, EncCodec, EncMemDesc, EncProfile, EncodeCtrl, EncoderDescription, ExtBuffer,
    ExtVideoSignalInfo, ExtendedDeviceId, FrameData, FrameInfo, FrameSurface1,
    FrameSurfaceInterface, ImplDescription, InfoMfx, InitializationParam, Range32U, Status,
    SyncPoint, Version, VideoParam,
};
use super::session::SYNC_WAIT_MS;
use super::*;
use crate::encoder::caps::{Diagnostic, Registry};
use crate::encoder::hw::{self, sample_config};
use crate::encoder::{AccessUnit, EncodeOptions, FrameView, RateConfig, Selection};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

macro_rules! size {
    ($t:ty, $n:expr) => {
        (stringify!($t), std::mem::size_of::<$t>(), $n)
    };
}

macro_rules! off {
    ($t:ty, $f:ident, $n:expr) => {
        (
            concat!(stringify!($t), ".", stringify!($f)),
            std::mem::offset_of!($t, $f),
            $n,
        )
    };
}

/// Every size and offset the FFI relies on, as Intel's libvpl API headers (v2.17.0,
/// d77f9195cf495b937631607333288fd917ae8939) lay them out for x86_64 Windows: a C file of
/// `sizeof`/`offsetof` constants over those headers, compiled with x86_64-w64-mingw32-gcc
/// 13 (`-std=c11 -S`), each number read from the `.quad` it compiled to. The structs'
/// first fields, at offset 0, are left out.
#[test]
fn layouts_match_the_libvpl_headers() {
    let got: &[(&str, usize, usize)] = &[
        size!(Version, 4),
        off!(Version, major, 2),
        size!(ExtBuffer, 8),
        off!(ExtBuffer, buffer_sz, 4),
        size!(Range32U, 12),
        off!(Range32U, max, 4),
        off!(Range32U, step, 8),
        size!(InitializationParam, 40),
        off!(InitializationParam, num_ext_param, 10),
        off!(InitializationParam, ext_param, 16),
        off!(InitializationParam, vendor_impl_id, 24),
        size!(EncMemDesc, 56),
        off!(EncMemDesc, width, 4),
        off!(EncMemDesc, height, 16),
        off!(EncMemDesc, num_color_formats, 42),
        off!(EncMemDesc, color_formats, 48),
        size!(EncProfile, 32),
        off!(EncProfile, num_mem_types, 18),
        off!(EncProfile, mem_desc, 24),
        size!(EncCodec, 32),
        off!(EncCodec, max_codec_level, 4),
        off!(EncCodec, num_profiles, 22),
        off!(EncCodec, profiles, 24),
        size!(EncoderDescription, 32),
        off!(EncoderDescription, num_codecs, 16),
        off!(EncoderDescription, codecs, 24),
        size!(ImplDescription, 648),
        off!(ImplDescription, impl_type, 4),
        off!(ImplDescription, acceleration_mode, 8),
        off!(ImplDescription, api_version, 12),
        off!(ImplDescription, impl_name, 16),
        off!(ImplDescription, license, 48),
        off!(ImplDescription, keywords, 176),
        off!(ImplDescription, vendor_id, 304),
        off!(ImplDescription, vendor_impl_id, 308),
        off!(ImplDescription, dev_dec, 312),
        off!(ImplDescription, enc, 504),
        off!(ImplDescription, rest, 536),
        size!(ExtendedDeviceId, 196),
        off!(ExtendedDeviceId, vendor_id, 2),
        off!(ExtendedDeviceId, device_id, 4),
        off!(ExtendedDeviceId, device_luid, 24),
        off!(ExtendedDeviceId, luid_valid, 36),
        off!(ExtendedDeviceId, device_name, 68),
        size!(FrameInfo, 68),
        off!(FrameInfo, bit_depth_luma, 18),
        off!(FrameInfo, bit_depth_chroma, 20),
        off!(FrameInfo, frame_id, 24),
        off!(FrameInfo, fourcc, 32),
        off!(FrameInfo, width, 36),
        off!(FrameInfo, height, 38),
        off!(FrameInfo, crop_x, 40),
        off!(FrameInfo, crop_y, 42),
        off!(FrameInfo, crop_w, 44),
        off!(FrameInfo, crop_h, 46),
        off!(FrameInfo, frame_rate_ext_n, 48),
        off!(FrameInfo, frame_rate_ext_d, 52),
        off!(FrameInfo, pic_struct, 62),
        off!(FrameInfo, chroma_format, 64),
        size!(FrameData, 96),
        off!(FrameData, pitch_high, 30),
        off!(FrameData, time_stamp, 32),
        off!(FrameData, pitch_low, 46),
        off!(FrameData, y, 48),
        off!(FrameData, uv, 56),
        off!(FrameData, mem_id, 80),
        off!(FrameData, data_flag, 90),
        size!(FrameSurface1, 184),
        off!(FrameSurface1, version, 8),
        off!(FrameSurface1, info, 16),
        off!(FrameSurface1, data, 88),
        size!(FrameSurfaceInterface, 112),
        off!(FrameSurfaceInterface, add_ref, 16),
        off!(FrameSurfaceInterface, release, 24),
        off!(FrameSurfaceInterface, map, 40),
        off!(FrameSurfaceInterface, unmap, 48),
        off!(FrameSurfaceInterface, synchronize, 72),
        size!(InfoMfx, 136),
        off!(InfoMfx, low_power, 28),
        off!(InfoMfx, brc_param_multiplier, 30),
        off!(InfoMfx, frame_info, 32),
        off!(InfoMfx, codec_id, 100),
        off!(InfoMfx, codec_profile, 104),
        off!(InfoMfx, codec_level, 106),
        off!(InfoMfx, target_usage, 110),
        off!(InfoMfx, gop_pic_size, 112),
        off!(InfoMfx, gop_ref_dist, 114),
        off!(InfoMfx, gop_opt_flag, 116),
        off!(InfoMfx, idr_interval, 118),
        off!(InfoMfx, rate_control_method, 120),
        off!(InfoMfx, initial_delay_in_kb, 122),
        off!(InfoMfx, buffer_size_in_kb, 124),
        off!(InfoMfx, target_kbps, 126),
        off!(InfoMfx, max_kbps, 128),
        off!(InfoMfx, num_ref_frame, 132),
        size!(VideoParam, 208),
        off!(VideoParam, async_depth, 14),
        off!(VideoParam, mfx, 16),
        off!(VideoParam, protected, 184),
        off!(VideoParam, io_pattern, 186),
        off!(VideoParam, ext_param, 192),
        off!(VideoParam, num_ext_param, 200),
        size!(ExtVideoSignalInfo, 20),
        off!(ExtVideoSignalInfo, video_format, 8),
        off!(ExtVideoSignalInfo, video_full_range, 10),
        off!(ExtVideoSignalInfo, colour_description_present, 12),
        off!(ExtVideoSignalInfo, colour_primaries, 14),
        off!(ExtVideoSignalInfo, transfer_characteristics, 16),
        off!(ExtVideoSignalInfo, matrix_coefficients, 18),
        size!(EncodeCtrl, 56),
        off!(EncodeCtrl, frame_type, 32),
        off!(EncodeCtrl, num_ext_param, 34),
        off!(EncodeCtrl, ext_param, 40),
        off!(EncodeCtrl, payload, 48),
        size!(Bitstream, 72),
        off!(Bitstream, num_ext_param, 16),
        off!(Bitstream, codec_id, 20),
        off!(Bitstream, time_stamp, 32),
        off!(Bitstream, data, 40),
        off!(Bitstream, data_offset, 48),
        off!(Bitstream, data_length, 52),
        off!(Bitstream, max_length, 56),
        off!(Bitstream, frame_type, 62),
        ("MFX_CODEC_AVC", ffi::CODEC_AVC as usize, 541283905),
        ("MFX_CODEC_HEVC", ffi::CODEC_HEVC as usize, 1129727304),
        ("MFX_FOURCC_NV12", ffi::FOURCC_NV12 as usize, 842094158),
        (
            "MFX_EXTBUFF_VIDEO_SIGNAL_INFO",
            ffi::EXTBUFF_VIDEO_SIGNAL_INFO as usize,
            1313428310,
        ),
        (
            "MFX_PROFILE_AVC_CONSTRAINED_BASELINE",
            ffi::PROFILE_AVC_CONSTRAINED_BASELINE as usize,
            578,
        ),
    ];
    let wrong: Vec<String> = got
        .iter()
        .filter(|(_, rust, c)| rust != c)
        .map(|(name, rust, c)| format!("{name}: {rust} here, {c} in the headers"))
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// mfxExtCodingOption against the same headers (sizeof/offsetof compiled from them with
/// x86_64-w64-mingw32-gcc), and the fake's raw view of it.
#[test]
fn the_coding_option_matches_the_libvpl_headers() {
    use super::ffi::ExtCodingOption;
    let got = [
        size!(ExtCodingOption, 64),
        off!(ExtCodingOption, nal_hrd_conformance, 34),
        off!(ExtCodingOption, vui_vcl_hrd_parameters, 38),
        off!(ExtCodingOption, vui_nal_hrd_parameters, 62),
        (
            "MFX_EXTBUFF_CODING_OPTION",
            ffi::EXTBUFF_CODING_OPTION as usize,
            1347372099,
        ),
        ("MFX_CODINGOPTION_OFF", ffi::CODINGOPTION_OFF as usize, 32),
        (
            "the fake's coding option id",
            CODING_OPTION as usize,
            1347372099,
        ),
        ("the fake's NalHrdConformance", NAL_HRD_CONFORMANCE, 34),
        ("the fake's VuiNalHrdParameters", VUI_NAL_HRD_PARAMETERS, 62),
        (
            "the fake's INCOMPATIBLE_VIDEO_PARAM (mfxdefs.h)",
            (-INCOMPATIBLE_VIDEO_PARAM) as usize,
            14,
        ),
    ];
    let wrong: Vec<String> = got
        .iter()
        .filter(|(_, rust, c)| rust != c)
        .map(|(name, rust, c)| format!("{name}: {rust} here, {c} in the headers"))
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}

const H264_SPS: &[u8] = &[0x67, 0x42, 0xc0, 0x1f, 0xda];
const H264_PPS: &[u8] = &[0x68, 0xce, 0x3c, 0x80];
const HEVC_VPS: &[u8] = &[0x40, 0x01, 0x0c];
const HEVC_SPS: &[u8] = &[
    0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03,
    0x00, 0x5d, 0xa0,
];
const HEVC_PPS: &[u8] = &[0x44, 0x01, 0xc1];

fn annexb(nals: &[&[u8]]) -> Vec<u8> {
    nals.iter()
        .flat_map(|n| [&[0, 0, 0, 1][..], *n].concat())
        .collect()
}

/// What the runtime would code: parameter sets and an IDR when asked, else a P frame.
pub(super) fn bitstream(codec: Codec, idr: bool, sets: bool) -> Vec<u8> {
    match (codec, idr, sets) {
        (Codec::Hevc, true, true) => annexb(&[HEVC_VPS, HEVC_SPS, HEVC_PPS, &[0x26, 0x01, 0xaf]]),
        (Codec::Hevc, true, false) => annexb(&[&[0x26, 0x01, 0xaf]]),
        (Codec::Hevc, false, _) => annexb(&[&[0x02, 0x01, 0xd0]]),
        (_, true, true) => annexb(&[H264_SPS, H264_PPS, &[0x65, 0x88, 0x84]]),
        (_, true, false) => annexb(&[&[0x65, 0x88, 0x84]]),
        (_, false, _) => annexb(&[&[0x41, 0x9a, 0x02]]),
    }
}

/// mfxExtCodingOption's buffer id and its HRD fields' offsets, from the libvpl headers (the
/// layout test checks them), and MFX_ERR_INCOMPATIBLE_VIDEO_PARAM: the fake reads them raw.
const CODING_OPTION: u32 = u32::from_le_bytes(*b"CDOP");
const NAL_HRD_CONFORMANCE: usize = 34;
const VUI_NAL_HRD_PARAMETERS: usize = 62;
pub(super) const INCOMPATIBLE_VIDEO_PARAM: Status = -14;
/// MFX_WRN_INCOMPATIBLE_VIDEO_PARAM: the runtime took the call and changed what it asked.
const WRN_INCOMPATIBLE_VIDEO_PARAM: Status = 5;

#[derive(Default)]
pub(super) struct Log {
    pub(super) queried: Vec<VideoParam>,
    pub(super) inited: Vec<VideoParam>,
    signal: Vec<ExtVideoSignalInfo>,
    pub(super) reset: Vec<VideoParam>,
    /// Each Init's NalHrdConformance and VuiNalHrdParameters, if it set them.
    pub(super) hrd: Vec<Option<(u16, u16)>>,
    /// The frame type asked of each encode call, if it asked for one.
    pub(super) frame_types: Vec<Option<u16>>,
    waits: Vec<u32>,
    surfaces: Vec<u64>,
    pub(super) live: i32,
    /// Every call into the runtime.
    calls: usize,
    /// The runtime was dropped: in a real one, the session closed and the library released.
    pub(super) dropped: bool,
}

// The logged parameters' extension list pointers are only compared, never followed.
unsafe impl Send for Log {}

/// A runtime that answers from scripts (status 0 once a script runs out).
pub(super) struct Fake {
    codec: Codec,
    log: Arc<Mutex<Log>>,
    pub(super) query: VecDeque<Status>,
    pub(super) init: VecDeque<Status>,
    pub(super) reset: VecDeque<Status>,
    encode: VecDeque<Status>,
    pub(super) sync: VecDeque<Status>,
    surface: VecDeque<Option<Fault>>,
    /// What GetVideoParam says; else what the encoder runs with.
    pub(super) got: Option<VideoParam>,
    /// What the encoder runs with: the last Init or Reset it took.
    running: Option<VideoParam>,
    /// A Reset above this TargetKbps runs at it instead, answered
    /// MFX_WRN_INCOMPATIBLE_VIDEO_PARAM.
    pub(super) correct: Option<u16>,
    /// The frame rate Init sets up, as FrameRateExtN/FrameRateExtD, whatever it was asked.
    pub(super) frame_rate: Option<(u32, u32)>,
    /// What GetVideoParam calls answer, in turn (MFX_ERR_NONE once it runs out).
    pub(super) video_param: VecDeque<Status>,
    /// GetVideoParam reports the smallest BRCParamMultiplier that holds the rates, as
    /// Intel's H.264 runtime does, rather than the one it was given.
    pub(super) normalize: bool,
    /// Its Init turned NalHrdConformance off.
    hrd_off: bool,
    /// What GetVideoParam reads back as NalHrdConformance, over what Init set.
    pub(super) hrd_report: Option<u16>,
    /// A Reset it takes leaves NalHrdConformance reading back as this.
    pub(super) hrd_after_reset: Option<u16>,
    /// From its n-th GetVideoParam call on (the first is Init's), it says this, over `got`.
    pub(super) got_from: Option<(usize, VideoParam)>,
    /// GetVideoParam calls so far.
    video_params: usize,
    /// IDRs after the first come without parameter sets.
    sets_once: bool,
    idrs: usize,
}

// `got` holds a null extension list pointer and nothing else.
unsafe impl Send for Fake {}

impl Fake {
    pub(super) fn new(codec: Codec) -> (Fake, Arc<Mutex<Log>>) {
        let log = Arc::new(Mutex::new(Log::default()));
        (
            Fake {
                codec,
                log: log.clone(),
                query: VecDeque::new(),
                init: VecDeque::new(),
                reset: VecDeque::new(),
                encode: VecDeque::new(),
                sync: VecDeque::new(),
                surface: VecDeque::new(),
                got: None,
                running: None,
                correct: None,
                frame_rate: None,
                video_param: VecDeque::new(),
                normalize: false,
                hrd_off: false,
                hrd_report: None,
                hrd_after_reset: None,
                got_from: None,
                video_params: 0,
                sets_once: false,
                idrs: 0,
            },
            log,
        )
    }

    /// The log, counting one call into the runtime.
    fn call(&self) -> std::sync::MutexGuard<'_, Log> {
        let mut log = self.log.lock().unwrap();
        log.calls += 1;
        log
    }
}

impl Runtime for Fake {
    fn query(&mut self, par: &mut VideoParam) -> Status {
        self.call().queried.push(*par);
        self.query.pop_front().unwrap_or(0)
    }

    fn init(&mut self, par: &mut VideoParam) -> Status {
        let mut log = self.call();
        log.inited.push(*par);
        let mut hrd = None;
        for i in 0..par.num_ext_param as usize {
            let b = unsafe { *par.ext_param.add(i) };
            match unsafe { (*b).buffer_id } {
                ffi::EXTBUFF_VIDEO_SIGNAL_INFO => {
                    log.signal
                        .push(unsafe { *(b as *const ExtVideoSignalInfo) });
                }
                CODING_OPTION => {
                    let at = |off: usize| unsafe { *((b as *const u8).add(off) as *const u16) };
                    hrd = Some((at(NAL_HRD_CONFORMANCE), at(VUI_NAL_HRD_PARAMETERS)));
                }
                _ => {}
            }
        }
        log.hrd.push(hrd);
        drop(log);
        let r = self.init.pop_front().unwrap_or(0);
        if r >= 0 {
            let mut runs = *par;
            if let Some((n, d)) = self.frame_rate {
                runs.mfx.frame_info.frame_rate_ext_n = n;
                runs.mfx.frame_info.frame_rate_ext_d = d;
            }
            self.running = Some(runs);
            self.hrd_off = hrd.is_some_and(|h| h.0 == ffi::CODINGOPTION_OFF);
        }
        r
    }

    /// As the runtime documents ("Dynamic Bitrate Change"): a constant-bitrate encoder held
    /// to HRD conformance can't change its bitrate through Reset.
    fn reset(&mut self, par: &mut VideoParam) -> Status {
        self.call().reset.push(*par);
        if let Some(r) = self.reset.pop_front() {
            return r;
        }
        let Some(now) = self.running else {
            return -8;
        };
        let kbps = |p: &VideoParam| {
            let m = p.mfx.brc_param_multiplier.max(1) as u32;
            (p.mfx.target_kbps as u32 * m, p.mfx.max_kbps as u32 * m)
        };
        let cbr = now.mfx.rate_control_method == ffi::RATECONTROL_CBR;
        if cbr && kbps(par) != kbps(&now) && !self.hrd_off {
            return INCOMPATIBLE_VIDEO_PARAM;
        }
        let mut runs = *par;
        let corrected = self.correct.filter(|&c| runs.mfx.target_kbps > c);
        if let Some(c) = corrected {
            runs.mfx.target_kbps = c;
            runs.mfx.max_kbps = c;
        }
        self.running = Some(runs);
        if let Some(v) = self.hrd_after_reset {
            self.hrd_report = Some(v);
        }
        if corrected.is_some() {
            WRN_INCOMPATIBLE_VIDEO_PARAM
        } else {
            0
        }
    }

    /// Fills what the encoder runs with into `par`, and NalHrdConformance into a coding
    /// option attached to it, as the runtime does.
    fn video_param(&mut self, par: &mut VideoParam) -> Status {
        drop(self.call());
        self.video_params += 1;
        let r = self.video_param.pop_front().unwrap_or(0);
        if r < 0 {
            return r;
        }
        let (ext, n) = (par.ext_param, par.num_ext_param);
        let hrd = self.hrd_report.unwrap_or(if self.hrd_off {
            ffi::CODINGOPTION_OFF
        } else {
            ffi::CODINGOPTION_ON
        });
        for i in 0..n as usize {
            let b = unsafe { *ext.add(i) };
            if unsafe { (*b).buffer_id } == CODING_OPTION {
                unsafe { *((b as *mut u8).add(NAL_HRD_CONFORMANCE) as *mut u16) = hrd };
            }
        }
        let later = self
            .got_from
            .filter(|(from, _)| self.video_params >= *from)
            .map(|(_, p)| p);
        *par = later.or(self.got).or(self.running).unwrap_or_default();
        if self.normalize && par.mfx.rate_control_method == ffi::RATECONTROL_CBR {
            let m = &mut par.mfx;
            let k = m.brc_param_multiplier.max(1) as u32;
            let [t, x, b, d] = [
                m.target_kbps,
                m.max_kbps,
                m.buffer_size_in_kb,
                m.initial_delay_in_kb,
            ]
            .map(|v| v as u32 * k);
            let n = t.max(x).max(b).max(d).div_ceil(u16::MAX as u32).max(1);
            m.brc_param_multiplier = n as u16;
            [
                m.target_kbps,
                m.max_kbps,
                m.buffer_size_in_kb,
                m.initial_delay_in_kb,
            ] = [t, x, b, d].map(|v| (v / n) as u16);
        }
        par.ext_param = std::ptr::null_mut();
        par.num_ext_param = 0;
        0
    }

    fn surface(&mut self, f: &FrameView<'_>, ts: u64) -> Result<*mut FrameSurface1, Fault> {
        drop(self.call());
        if let Some(Some(fault)) = self.surface.pop_front() {
            return Err(fault);
        }
        f.check().map_err(|e| Fault::Other(e.0))?;
        let mut log = self.log.lock().unwrap();
        log.live += 1;
        log.surfaces.push(ts);
        let s: Box<FrameSurface1> = Box::new(unsafe { std::mem::zeroed() });
        Ok(Box::into_raw(s))
    }

    fn release(&mut self, s: *mut FrameSurface1) {
        self.call().live -= 1;
        drop(unsafe { Box::from_raw(s) });
    }

    fn encode(
        &mut self,
        ctrl: Option<&mut EncodeCtrl>,
        _s: *mut FrameSurface1,
        bs: &mut Bitstream,
        sync: &mut SyncPoint,
    ) -> Status {
        let ft = ctrl.map(|c| c.frame_type);
        self.call().frame_types.push(ft);
        let r = self.encode.pop_front().unwrap_or(0);
        if r < 0 || r == ffi::WRN_DEVICE_BUSY {
            return r;
        }
        let idr = ft.is_some_and(|t| t & ffi::FRAMETYPE_IDR != 0);
        if idr {
            self.idrs += 1;
        }
        let data = bitstream(self.codec, idr, !self.sets_once || self.idrs <= 1);
        assert!(data.len() <= bs.max_length as usize);
        let out = unsafe { std::slice::from_raw_parts_mut(bs.data, bs.max_length as usize) };
        out[..data.len()].copy_from_slice(&data);
        bs.data_offset = 0;
        bs.data_length = data.len() as u32;
        *sync = std::ptr::dangling_mut();
        r
    }

    fn sync(&mut self, _sync: SyncPoint, wait_ms: u32) -> Status {
        self.call().waits.push(wait_ms);
        self.sync.pop_front().unwrap_or(0)
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.log.lock().unwrap().dropped = true;
    }
}

fn cfg(codec: Codec, w: u32, h: u32, kbps: u32) -> EncoderConfig {
    let mut c = sample_config(Backend::Qsv, codec, w, h, 30);
    c.rate = RateConfig::new(24.0, kbps, 30);
    c
}

fn try_encode(
    enc: &mut dyn VideoEncoder,
    w: u32,
    h: u32,
    key: bool,
) -> Result<Vec<AccessUnit>, EncodeError> {
    let (y, uv) = (
        vec![16u8; (w * h) as usize],
        vec![128u8; (w.div_ceil(2) * h.div_ceil(2)) as usize],
    );
    let f = FrameView {
        width: w,
        height: h,
        y: &y,
        u: &uv,
        v: &uv,
        stride_y: w as usize,
        stride_uv: w.div_ceil(2) as usize,
    };
    enc.encode(
        &f,
        EncodeOptions {
            pts_ms: 10,
            force_keyframe: key,
            ..Default::default()
        },
    )
}

fn encode(enc: &mut dyn VideoEncoder, w: u32, h: u32, key: bool) -> Vec<AccessUnit> {
    try_encode(enc, w, h, key).unwrap()
}

/// The error a 640x480 encode gives.
fn fails(enc: &mut dyn VideoEncoder, key: bool) -> EncodeError {
    match try_encode(enc, 640, 480, key) {
        Ok(_) => panic!("the encode passed"),
        Err(e) => e,
    }
}

const IDR: Option<u16> = Some(ffi::FRAMETYPE_I | ffi::FRAMETYPE_REF | ffi::FRAMETYPE_IDR);

#[test]
fn h264_cbr_with_an_idr_on_request() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 1920, 1080, 6000)).unwrap();
    {
        let l = log.lock().unwrap();
        assert_eq!((l.queried.len(), l.inited.len()), (1, 1));
        let p = l.inited[0];
        assert_eq!(
            (p.async_depth, p.io_pattern),
            (1, ffi::IOPATTERN_IN_SYSTEM_MEMORY)
        );
        let m = p.mfx;
        assert_eq!(m.codec_id, ffi::CODEC_AVC);
        assert_eq!(m.low_power, ffi::CODINGOPTION_OFF);
        assert_eq!(m.target_usage, ffi::TARGETUSAGE_BEST_SPEED);
        assert_eq!((m.gop_ref_dist, m.num_ref_frame), (1, 1), "no B frames");
        assert_eq!((m.gop_pic_size, m.idr_interval), (u16::MAX, 0));
        assert_eq!(m.rate_control_method, ffi::RATECONTROL_CBR);
        let mult = m.brc_param_multiplier as u32;
        let k = |v: u16| v as u32 * mult;
        let rate = (k(m.target_kbps), k(m.max_kbps), k(m.buffer_size_in_kb));
        assert_eq!(
            rate,
            (6000, 6000, 798),
            "the buffer an IDR at 4x the cap needs"
        );
        assert_eq!(
            mult, 3,
            "room for a Reset to a keyframe's boost of a stripe's 2x share at 10 fps"
        );
        let fi = m.frame_info;
        assert_eq!((fi.width, fi.height), (1920, 1088), "16-aligned");
        assert_eq!((fi.crop_w, fi.crop_h), (1920, 1080));
        assert_eq!((fi.frame_rate_ext_n, fi.frame_rate_ext_d), (30, 1));
        assert_eq!(
            (fi.fourcc, fi.chroma_format, fi.pic_struct),
            (
                ffi::FOURCC_NV12,
                ffi::CHROMAFORMAT_YUV420,
                ffi::PICSTRUCT_PROGRESSIVE
            )
        );
        let s = l.signal[0];
        assert_eq!(s.header.buffer_id, ffi::EXTBUFF_VIDEO_SIGNAL_INFO);
        assert_eq!(
            (
                s.colour_description_present,
                s.colour_primaries,
                s.transfer_characteristics,
                s.matrix_coefficients,
                s.video_full_range
            ),
            (1, 1, 13, 6, 0)
        );
    }
    let first = encode(enc.as_mut(), 1920, 1080, false);
    assert!(first[0].keyframe, "the first frame is an IDR");
    let c = first[0].config.as_ref().unwrap();
    assert_eq!(
        (c.codec_string.as_str(), c.coded),
        ("avc1.42c01f", (1920, 1080))
    );
    let p = encode(enc.as_mut(), 1920, 1080, false);
    assert!(!p[0].keyframe && p[0].config.is_none());
    let k = encode(enc.as_mut(), 1920, 1080, true);
    assert!(k[0].keyframe);
    let l = log.lock().unwrap();
    assert_eq!(l.frame_types, vec![IDR, None, IDR]);
    assert_eq!(
        l.waits,
        vec![SYNC_WAIT_MS; 3],
        "bounded, never MFX_INFINITE"
    );
    assert_eq!(l.surfaces, vec![900; 3], "pts in 90 kHz");
    assert_eq!(l.live, 0, "every surface goes back");
    drop(l);
    assert!(try_encode(enc.as_mut(), 1280, 720, false).is_err());
}

#[test]
fn hevc_constant_qp_when_uncapped() {
    let (fake, log) = Fake::new(Codec::Hevc);
    let mut enc = session(Box::new(fake), &cfg(Codec::Hevc, 1280, 720, 0)).unwrap();
    {
        let m = log.lock().unwrap().inited[0].mfx;
        assert_eq!(m.codec_id, ffi::CODEC_HEVC);
        assert_eq!(m.rate_control_method, ffi::RATECONTROL_CQP);
        assert_eq!(
            (m.initial_delay_in_kb, m.target_kbps, m.max_kbps),
            (24, 24, 24),
            "QPI, QPP, QPB"
        );
        assert_eq!(m.idr_interval, 1, "every HEVC I frame is an IDR");
        assert_eq!((m.frame_info.width, m.frame_info.height), (1280, 736));
    }
    let first = encode(enc.as_mut(), 1280, 720, false);
    assert!(first[0].keyframe);
    assert_eq!(
        first[0].config.as_ref().unwrap().codec_string,
        "hvc1.1.6.L93.90"
    );
    assert!(!encode(enc.as_mut(), 1280, 720, false)[0].keyframe);
}

#[test]
fn av1_and_others_are_refused() {
    let (fake, _) = Fake::new(Codec::Av1);
    let e = session(Box::new(fake), &cfg(Codec::Av1, 640, 480, 0))
        .err()
        .unwrap();
    assert!(e.0.contains("H.264 and HEVC"), "{e}");
}

#[test]
fn the_refusal_that_turned_low_power_on_is_kept() {
    let c = cfg(Codec::H264, 640, 480, 2000);
    let (mut fake, _) = Fake::new(Codec::H264);
    fake.query = VecDeque::from([-3]);
    let s = super::session::Session::new(Box::new(fake), &c).unwrap();
    assert!(s.low_power);
    assert_eq!(
        s.low_power_why.as_deref(),
        Some("LowPower off: MFXVideoENCODE_Query: MFX_ERR_UNSUPPORTED")
    );
    let (fake, _) = Fake::new(Codec::H264);
    let s = super::session::Session::new(Box::new(fake), &c).unwrap();
    assert!(!s.low_power && s.low_power_why.is_none());
}

#[test]
fn partial_acceleration_and_b_frames_are_refused() {
    let (mut fake, _) = Fake::new(Codec::H264);
    fake.init = VecDeque::from([ffi::WRN_PARTIAL_ACCELERATION]);
    let e = session(Box::new(fake), &cfg(Codec::H264, 640, 480, 2000))
        .err()
        .unwrap();
    assert!(e.0.contains("partly in software"), "{e}");

    let (mut fake, _) = Fake::new(Codec::H264);
    let rc = hw::Rc::new(Codec::H264, &RateConfig::new(24.0, 2000, 30), None);
    let mut got = super::session::params(Codec::H264, (640, 480), &rc, true);
    got.mfx.gop_ref_dist = 3;
    fake.got = Some(got);
    let e = session(Box::new(fake), &cfg(Codec::H264, 640, 480, 2000))
        .err()
        .unwrap();
    assert!(e.0.contains("B frames"), "{e}");
}

#[test]
fn warnings_and_a_busy_device_pass() {
    let (mut fake, log) = Fake::new(Codec::H264);
    fake.encode = VecDeque::from([5, ffi::WRN_DEVICE_BUSY, ffi::WRN_DEVICE_BUSY, 0]);
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 640, 480, 2000)).unwrap();
    assert!(encode(enc.as_mut(), 640, 480, false)[0].keyframe);
    assert!(!encode(enc.as_mut(), 640, 480, false)[0].keyframe);
    let l = log.lock().unwrap();
    let retried = vec![IDR, None, None, None];
    assert_eq!(l.frame_types, retried, "busy calls are retried");
    assert_eq!(l.live, 0);
}

#[test]
fn more_data_and_plain_errors_fail_the_frame_only() {
    let (mut fake, log) = Fake::new(Codec::H264);
    fake.encode = VecDeque::from([0, ffi::ERR_MORE_DATA, -1]);
    fake.surface = VecDeque::from([None, None, None, Some(Fault::Status("Map", -7))]);
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 640, 480, 2000)).unwrap();
    encode(enc.as_mut(), 640, 480, false);
    let e = fails(enc.as_mut(), false);
    assert!(e.0.contains("MFX_ERR_MORE_DATA"), "{e}");
    let e = fails(enc.as_mut(), false);
    assert!(e.0.contains("Async failed: MFX_ERR_UNKNOWN"), "{e}");
    let e = fails(enc.as_mut(), false);
    assert!(e.0.contains("Map failed: MFX_ERR_LOCK_MEMORY"), "{e}");
    let k = encode(enc.as_mut(), 640, 480, false);
    assert!(k[0].keyframe, "a failed encode restarts with an IDR");
    let l = log.lock().unwrap();
    assert_eq!(l.frame_types, vec![IDR, None, IDR, IDR]);
    assert_eq!(l.live, 0);
    drop(l);
    drop(enc);
    let l = log.lock().unwrap();
    assert!(l.dropped, "a live session closes its runtime");
}

#[test]
fn a_hung_or_lost_device_stops_the_session_without_another_call() {
    let hung = "did not finish a frame in 1000 ms";
    for (encode_status, sync_status, said) in [
        (0, ffi::WRN_IN_EXECUTION, hung),
        (0, ffi::ERR_GPU_HANG, "MFX_ERR_GPU_HANG"),
        (ffi::ERR_DEVICE_LOST, 0, "MFX_ERR_DEVICE_LOST"),
        (ffi::ERR_DEVICE_FAILED, 0, "MFX_ERR_DEVICE_FAILED"),
    ] {
        let (mut fake, log) = Fake::new(Codec::H264);
        fake.encode = VecDeque::from([0, encode_status]);
        fake.sync = VecDeque::from([0, sync_status]);
        let c = cfg(Codec::H264, 640, 480, 2000);
        let mut s = super::session::Session::new(Box::new(fake), &c).unwrap();
        encode(&mut s, 640, 480, false);
        let e = fails(&mut s, false);
        let stopped = e.0.starts_with(super::driver::STOPPED);
        assert!(stopped && e.0.contains(said), "{e}");
        assert_eq!(s.stopped(), Some(e.0.as_str()));
        let calls = log.lock().unwrap().calls;
        assert_eq!(fails(&mut s, true), e, "the device is not asked again");
        assert!(s.set_rate(RateConfig::new(24.0, 9000, 30)).is_err());
        drop(s);
        let l = log.lock().unwrap();
        assert_eq!(
            l.calls, calls,
            "no runtime call after the stop, not even a close"
        );
        assert!(!l.dropped, "the runtime is left for the process to end");
        assert_eq!(l.live, 1, "the frame's surface is never released");
        assert!(l.waits.iter().all(|&w| w == SYNC_WAIT_MS));
    }
}

#[test]
fn a_fatal_query_stops_instead_of_trying_with_low_power() {
    for status in [
        ffi::ERR_DEVICE_LOST,
        ffi::ERR_DEVICE_FAILED,
        ffi::ERR_GPU_HANG,
    ] {
        let (mut fake, log) = Fake::new(Codec::H264);
        fake.query = VecDeque::from([status]);
        let e = session(Box::new(fake), &cfg(Codec::H264, 640, 480, 2000))
            .err()
            .unwrap();
        let name = ffi::status_name(status);
        assert!(
            e.0.contains(&format!("MFXVideoENCODE_Query failed: {name}")),
            "{e}"
        );
        assert!(e.0.starts_with(super::driver::STOPPED), "{e}");
        let l = log.lock().unwrap();
        assert_eq!(l.queried.len(), 1, "no second Query with LowPower on");
        assert!(l.inited.is_empty() && !l.dropped);
    }
}

#[test]
fn the_runtime_loads_only_from_its_driver_store_path() {
    let a = Adapter {
        luid: 0x1234,
        device_id: 0x46d1,
        name: "Intel(R) UHD Graphics".into(),
    };
    let dir = r"C:\Windows\System32\DriverStore\FileRepository\iigd_dch.inf_amd64_0123\";
    let path = format!("{dir}libmfx64-gen.dll");
    assert_eq!(runtime_path(Ok(path.clone()), &a), Ok(path));
    let e = runtime_path(Err("the VEN_8086&DEV_46D1 driver names none".into()), &a).unwrap_err();
    assert!(
        e.starts_with("no DriverStorePathForVPL for luid:0000000000001234 Intel(R) UHD Graphics"),
        "{e}"
    );
    for relative in [
        "libmfx64-gen.dll",
        r"drivers\libmfx64-gen.dll",
        r"C:libmfx64-gen.dll",
    ] {
        let e = runtime_path(Ok(relative.into()), &a).unwrap_err();
        assert!(
            e.contains("is unusable") && e.contains("not an absolute DLL path"),
            "{e}"
        );
    }
    let unc = runtime_path(Ok(r"\\host\share\libmfx64-gen.dll".into()), &a).unwrap_err();
    assert!(unc.contains("a network or device path"), "{unc}");
}

#[test]
fn rate_changes_reset_the_encoder() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 640, 480, 2000)).unwrap();
    encode(enc.as_mut(), 640, 480, false);
    enc.set_rate(RateConfig::new(24.0, 2000, 30)).unwrap();
    let resets = log.lock().unwrap().reset.len();
    assert_eq!(resets, 0, "the same rate is no reset");
    enc.set_rate(RateConfig::new(24.0, 1000, 30)).unwrap();
    assert!(!encode(enc.as_mut(), 640, 480, false)[0].keyframe);
    enc.set_rate(RateConfig::new(24.0, 40_000, 30)).unwrap();
    assert!(
        !encode(enc.as_mut(), 640, 480, false)[0].keyframe,
        "a raised rate needs no IDR"
    );
    enc.set_rate(RateConfig::new(24.0, 100_000, 30)).unwrap();
    let l = log.lock().unwrap();
    let m = |p: &VideoParam| p.mfx;
    let r: Vec<(u16, u16, u16)> = l
        .reset
        .iter()
        .map(m)
        .map(|m| (m.target_kbps, m.max_kbps, m.brc_param_multiplier))
        .collect();
    assert_eq!(
        r,
        vec![(1000, 1000, 1), (40_000, 40_000, 1), (48_000, 48_000, 1)],
        "100 Mbit/s clamped to what a 2 Mbit/s cap at 30 fps was sized for"
    );
    assert!(l.reset.iter().all(|p| p.num_ext_param == 2));
}

#[test]
fn key_frames_without_parameter_sets_get_them() {
    let (mut fake, _) = Fake::new(Codec::H264);
    fake.sets_once = true;
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 640, 480, 2000)).unwrap();
    encode(enc.as_mut(), 640, 480, false);
    let k = encode(enc.as_mut(), 640, 480, true);
    assert!(k[0].keyframe);
    let has = |nal: &[u8]| k[0].data.windows(nal.len()).any(|w| w == nal);
    assert!(has(H264_SPS) && has(H264_PPS), "{:?}", k[0].data);
}

#[test]
fn the_sample_encode_verdict() {
    let (fake, _) = Fake::new(Codec::Hevc);
    let mut enc = session(Box::new(fake), &cfg(Codec::Hevc, 160, 160, 4000)).unwrap();
    assert_eq!(hw::sample(enc.as_mut(), 160, 160), Ok(()));
    let (mut fake, _) = Fake::new(Codec::H264);
    fake.encode = VecDeque::from([0, -17]);
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 160, 160, 4000)).unwrap();
    assert!(hw::sample(enc.as_mut(), 160, 160).is_err());
}

fn report() -> ImplInfo {
    let mut nv12 = [ffi::FOURCC_NV12, 0x30313050];
    let mut p010 = [0x30313050];
    let range = |min, max| Range32U { min, max, step: 16 };
    let mem = |formats: &mut [u32], kind| EncMemDesc {
        mem_handle_type: kind,
        width: range(64, 4096),
        height: range(48, 2304),
        reserved: [0; 7],
        num_color_formats: formats.len() as u16,
        color_formats: formats.as_mut_ptr(),
    };
    let system = ffi::RESOURCE_SYSTEM_SURFACE;
    let mut sys = [mem(&mut p010, 5), mem(&mut nv12, system)];
    let mut tenbit = [mem(&mut p010, system)];
    let profile = |profile, m: *mut EncMemDesc, n| EncProfile {
        profile,
        reserved: [0; 7],
        num_mem_types: n,
        mem_desc: m,
    };
    let mut avc = [
        profile(ffi::PROFILE_AVC_HIGH, sys.as_mut_ptr(), 2),
        profile(ffi::PROFILE_AVC_CONSTRAINED_BASELINE, sys.as_mut_ptr(), 2),
        profile(122, sys.as_mut_ptr(), 2),
    ];
    let mut hevc = [
        profile(ffi::PROFILE_HEVC_MAIN, sys.as_mut_ptr(), 2),
        profile(ffi::PROFILE_HEVC_MAIN10, sys.as_mut_ptr(), 2),
    ];
    let mut av1 = [profile(1, tenbit.as_mut_ptr(), 1)];
    let codec = |codec_id, p: &mut [EncProfile]| EncCodec {
        codec_id,
        max_codec_level: 51,
        bidirectional_prediction: 1,
        reserved: [0; 7],
        num_profiles: p.len() as u16,
        profiles: p.as_mut_ptr(),
    };
    let mut codecs = [
        codec(ffi::CODEC_AVC, &mut avc),
        codec(ffi::CODEC_HEVC, &mut hevc),
        codec(u32::from_le_bytes(*b"AV1 "), &mut av1),
        codec(u32::from_le_bytes(*b"VP9 "), &mut []),
        EncCodec {
            num_profiles: 9,
            profiles: std::ptr::null_mut(),
            ..codec(ffi::CODEC_HEVC, &mut [])
        },
    ];
    let mut d: ImplDescription = unsafe { std::mem::zeroed() };
    d.impl_type = ffi::IMPL_TYPE_HARDWARE;
    d.acceleration_mode = 0x300;
    d.api_version = Version {
        major: 2,
        minor: 13,
    };
    for (i, b) in b"mfx-gen".iter().enumerate() {
        d.impl_name[i] = *b as _;
    }
    d.vendor_id = ffi::VENDOR_INTEL;
    d.vendor_impl_id = 1;
    d.enc.num_codecs = codecs.len() as u16;
    d.enc.codecs = codecs.as_mut_ptr();
    unsafe { read_impl(&d) }
}

#[test]
fn the_runtime_description_is_read_and_only_h264_and_hevc_are_taken() {
    let imp = report();
    assert_eq!(
        (imp.name.as_str(), imp.api, imp.hardware, imp.vendor_impl_id),
        ("mfx-gen", (2, 13), true, 1)
    );
    assert_eq!(imp.acceleration, 0x300);
    assert_eq!(imp.codecs.len(), 5);
    let avc = &imp.codecs[0];
    assert_eq!(avc.profiles, vec![100, 578, 122]);
    assert!(avc.nv12, "system-memory NV12 is listed");
    assert_eq!(avc.width, Some((64, 4096)));
    assert_eq!(avc.height, Some((48, 2304)));
    assert!(!imp.codecs[2].nv12, "AV1 here takes P010 only");
    assert!(imp.codecs[4].profiles.is_empty(), "a null list is no list");
    assert!(imp.encodes(Codec::H264) && imp.encodes(Codec::Hevc));
    assert!(!imp.encodes(Codec::Av1));

    let c = caps("luid:1 (mfx-gen, oneVPL API 2.13)", &imp);
    let codecs: Vec<Codec> = c.iter().map(|c| c.codec).collect();
    assert_eq!(codecs, vec![Codec::H264, Codec::Hevc, Codec::Hevc]);
    for c in &c {
        assert_eq!(c.backend, Backend::Qsv);
        assert_eq!(c.acceleration, Acceleration::Hardware);
        assert!(c.device_id.contains("oneVPL API 2.13"));
    }
    let names: Vec<(&str, u8)> = c[0]
        .profiles
        .iter()
        .map(|p| (p.name.as_str(), p.bit_depth))
        .collect();
    assert_eq!(names, vec![("high", 8), ("constrained_baseline", 8)]);
    assert_eq!(c[0].status, ProbeStatus::Discovered);
    assert_eq!(c[0].limits.pixel_formats, Some(vec![PixelFormat::Nv12]));
    assert_eq!(c[0].limits.min_width, Some(64));
    assert_eq!(c[0].limits.max_height, Some(2304));
    assert_eq!(c[1].profiles.len(), 2);
    assert_eq!(
        c[2].status,
        ProbeStatus::ProbeOnly,
        "no profiles, no NV12: listed, never used"
    );
}

fn imp(luid: Option<u64>, device: Option<u16>, id: u32) -> ImplInfo {
    ImplInfo {
        hardware: true,
        vendor: ffi::VENDOR_INTEL,
        vendor_impl_id: id,
        luid,
        device_id: device,
        ..Default::default()
    }
}

/// A capped session runs constant bitrate, which Quick Sync holds at its cap on any
/// picture; an uncapped one runs constant QP.
#[test]
fn a_capped_session_holds_its_cap() {
    for (kbps, holds) in [(2000, true), (0, false)] {
        let (fake, _) = Fake::new(Codec::H264);
        let enc = session(Box::new(fake), &cfg(Codec::H264, 640, 480, kbps)).unwrap();
        assert_eq!(enc.holds_cap(), holds, "{kbps}");
    }
}

#[test]
fn the_implementation_for_an_adapter() {
    let a = Adapter {
        luid: 0x1_0000_abcd,
        device_id: 0x46d1,
        name: "Intel(R) UHD Graphics".into(),
    };
    assert_eq!(a.label(), "luid:000000010000abcd Intel(R) UHD Graphics");
    let by_luid = [imp(Some(7), Some(0x46d1), 0), imp(Some(a.luid), None, 1)];
    assert_eq!(pick(&by_luid, &a).unwrap().vendor_impl_id, 1);
    let by_device = [imp(None, Some(0x56a0), 0), imp(None, Some(0x46d1), 1)];
    assert_eq!(pick(&by_device, &a).unwrap().vendor_impl_id, 1);
    let mut software = imp(None, None, 0);
    software.hardware = false;
    let only = [software.clone(), imp(None, None, 3)];
    assert_eq!(pick(&only, &a).unwrap().vendor_impl_id, 3);
    let two = [imp(None, None, 0), imp(None, None, 1)];
    let e = pick(&two, &a).unwrap_err();
    assert!(e.contains("none for this adapter"), "{e}");
    assert!(pick(&[software], &a).is_err());
}

#[test]
fn intel_device_ids_from_instance_ids() {
    let adl_n = r"PCI\VEN_8086&DEV_46D1&SUBSYS_00000000&REV_00\3&11583659&0&10";
    assert_eq!(intel_device(adl_n), Some(0x46d1));
    let lower = r"pci\ven_8086&dev_56a0&subsys_1";
    assert_eq!(intel_device(lower), Some(0x56a0));
    assert_eq!(intel_device(r"PCI\VEN_10DE&DEV_2684"), None);
    assert_eq!(intel_device(r"PCI\VEN_8086"), None);
    assert_eq!(intel_device(r"PCI\VEN_8086&DEV_0000"), None);
}

#[test]
fn a_pinned_qsv_that_cannot_start_says_why() {
    let why = "the VEN_8086&DEV_46D1 driver names no DriverStorePathForVPL; nor on the \
               search path (libmfx64-gen.dll: not found)";
    let reg = Registry {
        encoders: Vec::new(),
        diagnostics: vec![Diagnostic {
            backend: Backend::Qsv,
            device: Some("luid:0000000000001234 Intel(R) UHD Graphics".into()),
            reason: why.into(),
        }],
    };
    let pinned = Selection::Pinned(Backend::Qsv);
    let e = crate::negotiate::h264_backend(&reg, pinned).unwrap_err();
    let named = e.contains("no usable qsv H264 encoder");
    assert!(named && e.contains(why), "{e}");

    let mut c = EncoderCapability::new(
        Codec::H264,
        Backend::Qsv,
        Acceleration::Hardware,
        "luid:1 (mfx-gen, oneVPL API 2.13)",
    );
    let failed = "sample encode: MFXInitialize failed: MFX_ERR_UNSUPPORTED (-3)";
    c.status = ProbeStatus::Unavailable(failed.into());
    let reg = Registry {
        encoders: vec![c],
        diagnostics: Vec::new(),
    };
    let e = crate::negotiate::h264_backend(&reg, pinned).unwrap_err();
    assert!(e.contains(failed), "{e}");
}

#[test]
fn qsv_is_experimental_and_hardware_h264_and_hevc_only() {
    assert!(Backend::Qsv.experimental());
    assert_eq!(Backend::Qsv.name(), "qsv");
    assert_eq!(
        serde_json::to_value(Backend::Qsv).unwrap(),
        serde_json::json!("qsv")
    );
    for codec in [Codec::H264, Codec::Hevc] {
        assert_eq!(Backend::Qsv.implements(codec), cfg!(windows));
    }
    assert!(!Backend::Qsv.implements(Codec::Av1));
    #[cfg(not(windows))]
    assert!(Backend::parse("qsv").unwrap_err().contains("Windows-only"));
}

#[test]
fn a_probe_sample_that_stops_keeps_its_whole_session() {
    let (mut fake, log) = Fake::new(Codec::H264);
    fake.sync = VecDeque::from([ffi::WRN_IN_EXECUTION]);
    let s = super::session::Session::new(Box::new(fake), &cfg(Codec::H264, 160, 160, 2000));
    let mut kept = Vec::new();
    let e = sample_on(s.unwrap(), 160, &mut kept).unwrap_err();
    assert!(e.0.starts_with(super::driver::STOPPED), "{e}");
    assert_eq!(kept.len(), 1, "the session, its buffers and bitstream stay");
    assert_eq!(kept[0].stopped(), Some(e.0.as_str()));
    let l = log.lock().unwrap();
    assert!(!l.dropped, "the runtime is left for the process to end");
    assert_eq!(l.live, 1, "the frame in flight keeps its surface");
    drop(l);

    let (mut fake, log) = Fake::new(Codec::H264);
    fake.encode = VecDeque::from([-1]);
    let s = super::session::Session::new(Box::new(fake), &cfg(Codec::H264, 160, 160, 2000));
    let mut kept = Vec::new();
    let e = sample_on(s.unwrap(), 160, &mut kept).unwrap_err();
    assert!(!e.0.starts_with(super::driver::STOPPED), "{e}");
    assert!(kept.is_empty(), "a session that still answers is closed");
    assert!(log.lock().unwrap().dropped);
}
