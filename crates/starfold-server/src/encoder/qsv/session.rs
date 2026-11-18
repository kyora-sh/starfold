//! One Quick Sync session: IDR then P frames off the last frame only (one reference, no B
//! frames, one frame in flight), an IDR on request, parameter sets with every IDR, input
//! through the runtime's own system-memory NV12 surfaces. It opens on the full encoder
//! (LowPower off), and on the low-power one (VDEnc) only if the runtime refuses that.
//!
//! The stream is not held to HRD conformance (NalHrdConformance off, no NAL HRD parameters
//! in its VUI): oneVPL lets a constant-bitrate encoder change its bitrate through Reset only
//! then (the spec's "Dynamic Bitrate Change"), and every rate change goes through Reset. A
//! Reset is built from what the encoder reads back (GetVideoParam) with TargetKbps (and
//! MaxKbps with it) changed, held to the one ceiling the session was sized for and its
//! BRCParamMultiplier can hold; a raise is no IDR. An encoder that doesn't read back the
//! rate control method, codec, picture size and format asked for, and at constant bitrate
//! HRD conformance off, is not used: Init fails, and before or after a Reset it is out of
//! use. So is one that reads back no defined frame rate when a rate change needs it, and
//! one whose Reset the runtime refuses: it takes no more pictures and the stream opens a
//! fresh one. The session reports no bitrate; the server's figure is what it asked for.
//!
//! It runs in the encoder helper process (see [`super::remote`]). A device that hangs or is
//! lost stops the session: it calls the runtime no more, not even to close it, and its error
//! starts with [`STOPPED`]; the helper reports that and exits, and the server ends every
//! Quick Sync helper. A call stuck inside the driver is ended the same way, by the server
//! killing the helper.

use super::driver::{fatal, Fault, Runtime, STOPPED};
use super::ffi::{
    self, Bitstream, EncodeCtrl, ExtBuffer, ExtCodingOption, ExtVideoSignalInfo, VideoParam,
};
use crate::encoder::hw::{self, Rc};
use crate::encoder::{
    AccessUnit, Codec, ColorInfo, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig,
    VideoEncoder,
};
use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

/// How long one frame may take on the GPU before the device counts as hung. The hardware
/// encodes a 1080p frame in a few milliseconds; a frame not done in a second is not slow,
/// and waiting forever (MFX_INFINITE) would hold the stream with it. It is under the
/// server's 2 s frame deadline, so a well-behaved hang is reported before the server kills.
pub const SYNC_WAIT_MS: u32 = 1000;
/// How long a device that answers "busy" may keep a frame from being queued.
const BUSY_WAIT: Duration = Duration::from_millis(500);
const BUSY_POLL: Duration = Duration::from_millis(1);

fn align(v: u32, a: u32) -> u16 {
    v.div_ceil(a).saturating_mul(a).min(u16::MAX as u32) as u16
}

/// Low-delay rate control in `m`: CBR at `r`'s bitrate, else constant QP.
pub fn apply(r: &Rc, m: &mut ffi::InfoMfx) {
    m.frame_info.frame_rate_ext_n = r.fps;
    m.frame_info.frame_rate_ext_d = 1;
    if r.cbr {
        let kbps = r.bps / 1000;
        let buffer_kb = r.buffer_bits / 8000;
        let mult = kbps.max(buffer_kb).div_ceil(u16::MAX as u32).max(1);
        m.rate_control_method = ffi::RATECONTROL_CBR;
        m.brc_param_multiplier = mult as u16;
        m.target_kbps = (kbps / mult).max(1) as u16;
        m.max_kbps = m.target_kbps;
        m.buffer_size_in_kb = (buffer_kb / mult).max(1) as u16;
        m.initial_delay_in_kb = m.buffer_size_in_kb;
    } else {
        let qp = r.qp.min(51) as u16;
        m.rate_control_method = ffi::RATECONTROL_CQP;
        m.brc_param_multiplier = 0;
        m.initial_delay_in_kb = qp;
        m.target_kbps = qp;
        m.max_kbps = qp;
        m.buffer_size_in_kb = 0;
    }
}

/// The encoder parameters for `codec` at `size` with `rc`, without extension buffers.
pub fn params(codec: Codec, size: (u32, u32), rc: &Rc, low_power: bool) -> VideoParam {
    let mut p = VideoParam {
        async_depth: 1,
        io_pattern: ffi::IOPATTERN_IN_SYSTEM_MEMORY,
        ..Default::default()
    };
    let m = &mut p.mfx;
    let (codec_id, block, idr_interval) = match codec {
        // HEVC counts IdrInterval differently: 1 makes every I frame an IDR.
        Codec::Hevc => (ffi::CODEC_HEVC, 32, 1),
        _ => (ffi::CODEC_AVC, 16, 0),
    };
    m.codec_id = codec_id;
    m.low_power = if low_power {
        ffi::CODINGOPTION_ON
    } else {
        ffi::CODINGOPTION_OFF
    };
    m.target_usage = ffi::TARGETUSAGE_BEST_SPEED;
    m.gop_pic_size = u16::MAX;
    m.gop_ref_dist = 1;
    m.gop_opt_flag = ffi::GOP_CLOSED;
    m.idr_interval = idr_interval;
    m.num_ref_frame = 1;
    let fi = &mut m.frame_info;
    fi.fourcc = ffi::FOURCC_NV12;
    fi.chroma_format = ffi::CHROMAFORMAT_YUV420;
    fi.pic_struct = ffi::PICSTRUCT_PROGRESSIVE;
    fi.bit_depth_luma = 8;
    fi.bit_depth_chroma = 8;
    fi.width = align(size.0, block);
    fi.height = align(size.1, block);
    fi.crop_w = size.0 as u16;
    fi.crop_h = size.1 as u16;
    apply(rc, m);
    p
}

fn signal(c: ColorInfo) -> ExtVideoSignalInfo {
    ExtVideoSignalInfo {
        header: ExtBuffer {
            buffer_id: ffi::EXTBUFF_VIDEO_SIGNAL_INFO,
            buffer_sz: std::mem::size_of::<ExtVideoSignalInfo>() as u32,
        },
        video_format: 5,
        video_full_range: c.full_range as u16,
        colour_description_present: 1,
        colour_primaries: c.primaries as u16,
        transfer_characteristics: c.transfer as u16,
        matrix_coefficients: c.matrix as u16,
    }
}

/// What GetVideoParam says the encoder runs with.
struct Working {
    par: VideoParam,
    /// NalHrdConformance (0: not said).
    hrd: u16,
}

/// The most kbit/s a Reset may put in TargetKbps for a stream capped at `cap_kbps` and
/// coded at `fps`: a first keyframe's boost of the largest share one stripe can get (an
/// encoder that doesn't hold its cap; one that does gets less), held to its budget per
/// picture at the slowest rate the stream budgets for.
fn ceiling_kbps(cap_kbps: u32, fps: u32) -> u64 {
    let share = crate::video::stripe_share(cap_kbps, 1, false);
    let boost = crate::video::keyframe_rate(0.0, share, fps).max_kbps as u64;
    boost * fps.max(1) as u64 / crate::video::FPS_FLOOR.min(fps.max(1)) as u64
}

/// The most TargetKbps (kbit/s, per picture at the encoder's own frame rate) a session may
/// be given: what Init sized it for (`sized_for`, from the configured maximum), and what
/// the BRCParamMultiplier the runtime reports can hold. Init and every Reset go by it; a
/// request above it is clamped to it.
fn brc_ceiling(sized_for: u64, multiplier: u16) -> u64 {
    sized_for.min(u16::MAX as u64 * multiplier.max(1) as u64)
}

/// `p` with a BRCParamMultiplier large enough that a Reset never needs another to reach
/// `ceiling` kbit/s: the rate fields rescaled to it, their values unchanged.
fn sized(mut p: VideoParam, ceiling: u64) -> VideoParam {
    let m = &mut p.mfx;
    if m.rate_control_method != ffi::RATECONTROL_CBR {
        return p;
    }
    let was = InForce::of(m);
    let mult = ceiling
        .div_ceil(u16::MAX as u64)
        .max(was.multiplier as u64)
        .min(u16::MAX as u64) as u16;
    let field = |v: u32| (v / mult as u32).clamp(1, u16::MAX as u32) as u16;
    m.brc_param_multiplier = mult;
    m.target_kbps = field(was.target_kbps);
    m.max_kbps = field(was.max_kbps);
    m.buffer_size_in_kb = field(was.buffer_kb);
    m.initial_delay_in_kb = field(was.initial_delay_kb);
    p
}

/// No HRD conformance and no NAL HRD parameters, so a bitrate change can go through Reset.
fn coding_option() -> ExtCodingOption {
    ExtCodingOption {
        header: ExtBuffer {
            buffer_id: ffi::EXTBUFF_CODING_OPTION,
            buffer_sz: std::mem::size_of::<ExtCodingOption>() as u32,
        },
        nal_hrd_conformance: ffi::CODINGOPTION_OFF,
        vui_nal_hrd_parameters: ffi::CODINGOPTION_OFF,
        ..Default::default()
    }
}

pub struct Session {
    /// Dropped (closed) only while the device answers; see [`Session::stopped`].
    rt: ManuallyDrop<Box<dyn Runtime>>,
    codec: Codec,
    size: (u32, u32),
    color: ColorInfo,
    /// What the encoder was set up with; its extension buffer list points at `ext`.
    par: VideoParam,
    signal: Box<ExtVideoSignalInfo>,
    coding: Box<ExtCodingOption>,
    ext: Box<[*mut ExtBuffer; 2]>,
    /// The coding option GetVideoParam fills in, so HRD conformance is read back.
    readback: Box<ExtCodingOption>,
    readback_ext: Box<[*mut ExtBuffer; 1]>,
    /// The output buffer and the descriptors the runtime is handed, at fixed addresses.
    buf: Vec<u8>,
    bs: Box<Bitstream>,
    ctrl: Box<EncodeCtrl>,
    /// The session was opened asking for VDEnc (fixed-function, LowPower on).
    pub low_power: bool,
    /// LowPower as the runtime reads it back after Init: on, off, or None if it doesn't say.
    pub low_power_read: Option<bool>,
    /// Why it does: how the runtime refused the full encoder.
    pub low_power_why: Option<String>,
    /// The rate last asked for, so the same request isn't made twice.
    rc: Rc,
    /// The most kbit/s TargetKbps may be given, from the configured maximum (see
    /// [`brc_ceiling`]).
    sized_for: u64,
    /// What the encoder runs with, as the runtime last read it back: rate control method,
    /// multiplier (for the ceiling) and frame rate (for a rate change's scaling). None once a
    /// rate change failed and nothing has been read back since.
    in_force: Option<InForce>,
    /// The next picture must be an IDR: the first frame, or the last encode failed.
    need_idr: bool,
    stream: hw::Stream,
    /// Why the device is not asked again.
    stopped: Option<String>,
    /// Why this encoder takes no more requests: the runtime refused a change, or couldn't
    /// say what it runs. The stream replaces it; the device itself answers.
    out_of_use: Option<String>,
}

/// The rate control an encoder runs with, as GetVideoParam reports it, BRCParamMultiplier
/// applied: kbit/s and KB.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct InForce {
    pub cbr: bool,
    pub target_kbps: u32,
    pub max_kbps: u32,
    pub buffer_kb: u32,
    pub initial_delay_kb: u32,
    pub multiplier: u16,
    /// The frame rate as the runtime reports it: FrameRateExtN / FrameRateExtD frames per
    /// second, kept as that fraction (30000/1001 is not 29 or 30). Either one 0 leaves it
    /// undefined, and nothing is derived from it.
    pub fps_n: u32,
    pub fps_d: u32,
}

/// `a / b`, rounded to the nearest.
fn div_round(a: u64, b: u64) -> u64 {
    (2 * a + b) / (2 * b.max(1))
}

impl InForce {
    fn of(m: &ffi::InfoMfx) -> InForce {
        let mult = m.brc_param_multiplier.max(1);
        let k = |v: u16| v as u32 * mult as u32;
        let fi = &m.frame_info;
        InForce {
            cbr: m.rate_control_method == ffi::RATECONTROL_CBR,
            target_kbps: k(m.target_kbps),
            max_kbps: k(m.max_kbps),
            buffer_kb: k(m.buffer_size_in_kb),
            initial_delay_kb: k(m.initial_delay_in_kb),
            multiplier: mult,
            fps_n: fi.frame_rate_ext_n,
            fps_d: fi.frame_rate_ext_d,
        }
    }

    /// The frame rate is a defined fraction.
    fn has_frame_rate(&self) -> bool {
        self.fps_n > 0 && self.fps_d > 0
    }

    /// TargetKbps for `kbps` asked at `fps` pictures a second: the same budget per picture
    /// at this encoder's own frame rate. None if the runtime reports no defined frame rate.
    fn target_for(&self, kbps: u64, fps: u32) -> Option<u64> {
        self.has_frame_rate().then(|| {
            div_round(
                kbps * self.fps_n as u64,
                self.fps_d as u64 * fps.max(1) as u64,
            )
        })
    }
}

// The raw pointers point into the session's own boxes, or at the runtime's surface.
unsafe impl Send for Session {}

impl Drop for Session {
    /// A stopped session (given up, killed, deadline missed) never calls the runtime; an
    /// out-of-use but healthy session closes normally.
    fn drop(&mut self) {
        if self.stopped.is_none() {
            unsafe { ManuallyDrop::drop(&mut self.rt) };
        }
    }
}

impl Session {
    pub fn new(rt: Box<dyn Runtime>, cfg: &EncoderConfig) -> Result<Session, EncodeError> {
        if !matches!(cfg.codec, Codec::H264 | Codec::Hevc) {
            return Err(EncodeError(format!(
                "Quick Sync here encodes H.264 and HEVC, not {:?}",
                cfg.codec
            )));
        }
        let color = ColorInfo::SRGB_BT601_LIMITED;
        let rc = Rc::new(cfg.codec, &cfg.rate, None);
        let r = &cfg.rate;
        let boosted = crate::video::keyframe_rate(r.quality, r.max_kbps, r.fps);
        let mut init = rc;
        init.buffer_bits = rc
            .buffer_bits
            .max(Rc::new(cfg.codec, &boosted, None).buffer_bits);
        // No multiplier is in force yet: Init asks for room for the whole ceiling.
        let ceiling = brc_ceiling(ceiling_kbps(r.max_kbps, r.fps), u16::MAX);
        let size = (cfg.width, cfg.height);
        let mut s = Session {
            rt: ManuallyDrop::new(rt),
            codec: cfg.codec,
            size,
            color,
            par: VideoParam::default(),
            signal: Box::new(signal(color)),
            coding: Box::new(coding_option()),
            ext: Box::new([std::ptr::null_mut(); 2]),
            readback: Box::default(),
            readback_ext: Box::new([std::ptr::null_mut()]),
            buf: Vec::new(),
            bs: Box::new(Bitstream::over(&mut [])),
            ctrl: Box::default(),
            low_power: false,
            low_power_read: None,
            low_power_why: None,
            rc,
            sized_for: ceiling,
            in_force: None,
            need_idr: true,
            stream: hw::Stream::new("Quick Sync", cfg.codec, size, color, true),
            stopped: None,
            out_of_use: None,
        };
        let mut refused = Vec::new();
        for low_power in [false, true] {
            let mode = if low_power {
                "LowPower on"
            } else {
                "LowPower off"
            };
            let mut q = s.with_ext(sized(params(cfg.codec, size, &init, low_power), ceiling));
            let r = s.rt.query(&mut q);
            if fatal(r) {
                return Err(s.failed("MFXVideoENCODE_Query", r));
            }
            if r < ffi::ERR_NONE {
                refused.push(format!(
                    "{mode}: MFXVideoENCODE_Query: {}",
                    ffi::status_name(r)
                ));
                continue;
            }
            let mut p = s.with_ext(sized(params(cfg.codec, size, &init, low_power), ceiling));
            let r = s.rt.init(&mut p);
            if r == ffi::WRN_PARTIAL_ACCELERATION {
                return Err(EncodeError(
                    "the runtime would encode partly in software (MFX_WRN_PARTIAL_ACCELERATION)"
                        .into(),
                ));
            }
            if r < ffi::ERR_NONE {
                if fatal(r) {
                    return Err(s.failed("MFXVideoENCODE_Init", r));
                }
                refused.push(format!(
                    "{mode}: MFXVideoENCODE_Init: {}",
                    ffi::status_name(r)
                ));
                continue;
            }
            s.par = p;
            s.low_power = low_power;
            s.low_power_why = low_power.then(|| refused.join("; "));
            s.refit()?;
            return Ok(s);
        }
        Err(EncodeError(format!(
            "no Quick Sync {:?} session at {}x{} ({})",
            cfg.codec,
            size.0,
            size.1,
            refused.join("; ")
        )))
    }

    /// Why the device stopped answering, once it did.
    pub fn stopped(&self) -> Option<&str> {
        self.stopped.as_deref()
    }

    /// `p` with the session's extension buffers: its colour signalling, and no HRD.
    fn with_ext(&mut self, mut p: VideoParam) -> VideoParam {
        *self.signal = signal(self.color);
        *self.coding = coding_option();
        self.ext[0] = &mut *self.signal as *mut ExtVideoSignalInfo as *mut ExtBuffer;
        self.ext[1] = &mut *self.coding as *mut ExtCodingOption as *mut ExtBuffer;
        p.ext_param = self.ext.as_mut_ptr();
        p.num_ext_param = 2;
        p
    }

    /// Takes this encoder out of use after `e`, unless the device stopped (that ends
    /// Quick Sync instead): no request goes through it again, and the stream replaces it.
    fn out_of_use(&mut self, e: EncodeError) -> EncodeError {
        if self.stopped.is_some() {
            return e;
        }
        let why = format!("this Quick Sync encoder is out of use: {}", e.0);
        self.out_of_use = Some(why.clone());
        EncodeError(why)
    }

    /// What the encoder runs with now (GetVideoParam): its parameters, and NalHrdConformance
    /// as the runtime fills it into the coding option attached to the call (0 if it doesn't
    /// say).
    fn working(&mut self) -> Result<Working, EncodeError> {
        *self.readback = ExtCodingOption {
            header: ExtBuffer {
                buffer_id: ffi::EXTBUFF_CODING_OPTION,
                buffer_sz: std::mem::size_of::<ExtCodingOption>() as u32,
            },
            ..Default::default()
        };
        self.readback_ext[0] = &mut *self.readback as *mut ExtCodingOption as *mut ExtBuffer;
        let mut par = VideoParam {
            ext_param: self.readback_ext.as_mut_ptr(),
            num_ext_param: 1,
            ..Default::default()
        };
        let r = self.rt.video_param(&mut par);
        if r < ffi::ERR_NONE {
            return Err(self.failed("MFXVideoENCODE_GetVideoParam", r));
        }
        par.ext_param = std::ptr::null_mut();
        par.num_ext_param = 0;
        Ok(Working {
            par,
            hrd: self.readback.nal_hrd_conformance,
        })
    }

    /// Why what the runtime read back is not the encoder asked for: another rate control
    /// method, codec, picture format or size, HRD conformance not read back off at constant
    /// bitrate (only then can the bitrate change through Reset), or B frames.
    fn mismatch(&self, w: &Working) -> Option<String> {
        let m = &w.par.mfx;
        let fi = &m.frame_info;
        let method = if self.rc.cbr {
            ffi::RATECONTROL_CBR
        } else {
            ffi::RATECONTROL_CQP
        };
        let codec = match self.codec {
            Codec::Hevc => ffi::CODEC_HEVC,
            _ => ffi::CODEC_AVC,
        };
        let (w0, h0) = self.size;
        if m.rate_control_method != method {
            Some(format!(
                "the runtime runs rate control method {}, not the {method} asked",
                m.rate_control_method
            ))
        } else if m.codec_id != codec {
            Some(format!(
                "the runtime runs codec {:#x}, not {codec:#x}",
                m.codec_id
            ))
        } else if fi.fourcc != ffi::FOURCC_NV12 || (fi.crop_w as u32, fi.crop_h as u32) != (w0, h0)
        {
            Some(format!(
                "the runtime codes {:#x} pictures at {}x{}, not NV12 at {w0}x{h0}",
                fi.fourcc, fi.crop_w, fi.crop_h
            ))
        } else if self.rc.cbr && w.hrd != ffi::CODINGOPTION_OFF {
            let state = if w.hrd == 0 { "unconfirmed" } else { "on" };
            Some(format!(
                "the runtime runs constant bitrate with HRD conformance {state} \
                 (NalHrdConformance {}): its bitrate can't change through Reset",
                w.hrd
            ))
        } else if m.gop_ref_dist > 1 {
            Some(format!(
                "the runtime set up B frames (GopRefDist {})",
                m.gop_ref_dist
            ))
        } else {
            None
        }
    }

    /// Reads back what the encoder runs with and keeps it, if it is the encoder asked for
    /// ([`Session::mismatch`]); sizes the output buffer.
    fn refit(&mut self) -> Result<(), EncodeError> {
        self.in_force = None;
        let w = self.working()?;
        let got = w.par;
        self.low_power_read = match got.mfx.low_power {
            ffi::CODINGOPTION_ON => Some(true),
            ffi::CODINGOPTION_OFF => Some(false),
            _ => None,
        };
        if let Some(why) = self.mismatch(&w) {
            return Err(EncodeError(why));
        }
        self.in_force = Some(InForce::of(&got.mfx));
        let fi = &self.par.mfx.frame_info;
        let picture = fi.width as usize * fi.height as usize * 3 / 2;
        let mult = got.mfx.brc_param_multiplier.max(1) as usize;
        let buffer = got.mfx.buffer_size_in_kb as usize * mult * 1000;
        let need = picture.max(buffer).max(1 << 16);
        if self.buf.len() < need {
            self.buf.resize(need, 0);
        }
        Ok(())
    }

    /// The error for a call that returned `s`; a hung or lost device stops the session.
    fn failed(&mut self, what: &str, s: ffi::Status) -> EncodeError {
        let msg = format!("{what} failed: {} ({s})", ffi::status_name(s));
        if fatal(s) {
            self.stop(msg)
        } else {
            EncodeError(msg)
        }
    }

    fn fault(&mut self, f: Fault) -> EncodeError {
        match f {
            Fault::Status(what, s) => self.failed(what, s),
            Fault::Other(m) => EncodeError(m),
        }
    }

    /// Stops calling the runtime: whatever a frame still on the GPU can reach stays as it is.
    fn stop(&mut self, why: String) -> EncodeError {
        let msg = format!("{STOPPED}{why}");
        self.stopped = Some(msg.clone());
        EncodeError(msg)
    }

    fn running(&self) -> Result<(), EncodeError> {
        match self.stopped.as_ref().or(self.out_of_use.as_ref()) {
            Some(why) => Err(EncodeError(why.clone())),
            None => Ok(()),
        }
    }
}

impl Session {
    fn change_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError> {
        self.running()?;
        let rc = Rc::new(self.codec, &rate, Some(&self.rc));
        if rc == self.rc {
            return Ok(());
        }
        let w = match self.working() {
            Ok(w) => w,
            Err(e) => return Err(self.out_of_use(e)),
        };
        if let Some(why) = self.mismatch(&w) {
            let e = EncodeError(format!("before a rate change, {why}"));
            return Err(self.out_of_use(e));
        }
        let mut working = w.par;
        let m = &mut working.mfx;
        if rc.cbr {
            // The same budget per picture at the encoder's own frame rate, which stays.
            let ran = InForce::of(m);
            let Some(asked) = ran.target_for(rc.bps as u64 / 1000, rc.fps) else {
                let e = EncodeError(format!(
                    "the encoder reports no defined frame rate ({}/{})",
                    ran.fps_n, ran.fps_d
                ));
                return Err(self.out_of_use(e));
            };
            // Above what the session can be given, it gets that.
            let kbps = asked.min(brc_ceiling(self.sized_for, ran.multiplier));
            let field = (kbps / ran.multiplier as u64).clamp(1, u16::MAX as u64) as u16;
            m.target_kbps = field;
            m.max_kbps = field;
        } else {
            let qp = rc.qp.min(51) as u16;
            m.initial_delay_in_kb = qp;
            m.target_kbps = qp;
            m.max_kbps = qp;
        }
        let mut p = self.with_ext(working);
        let r = self.rt.reset(&mut p);
        if r == ffi::WRN_PARTIAL_ACCELERATION || r < ffi::ERR_NONE {
            let e = self.failed("MFXVideoENCODE_Reset", r);
            return Err(self.out_of_use(e));
        }
        self.par = p;
        self.rc = rc;
        if let Err(e) = self.refit() {
            return Err(self.out_of_use(e));
        }
        Ok(())
    }
}

impl VideoEncoder for Session {
    /// A rate change that fails, however, leaves what the encoder runs with unknown until
    /// the next read-back.
    fn set_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError> {
        let r = self.change_rate(rate);
        if r.is_err() {
            self.in_force = None;
        }
        r
    }

    fn encode(
        &mut self,
        f: &FrameView<'_>,
        opts: EncodeOptions<'_>,
    ) -> Result<Vec<AccessUnit>, EncodeError> {
        self.running()?;
        if (f.width, f.height) != self.size {
            return Err(EncodeError(format!(
                "encoder opened at {}x{}, got {}x{}",
                self.size.0, self.size.1, f.width, f.height
            )));
        }
        let idr = opts.force_keyframe || self.need_idr;
        self.need_idr = true;
        let ts = opts.pts_ms.max(0) as u64 * 90;
        let surface = match self.rt.surface(f, ts) {
            Ok(s) => s,
            Err(e) => return Err(self.fault(e)),
        };
        *self.ctrl = EncodeCtrl {
            frame_type: ffi::FRAMETYPE_I | ffi::FRAMETYPE_REF | ffi::FRAMETYPE_IDR,
            ..Default::default()
        };
        *self.bs = Bitstream::over(&mut self.buf);
        let mut sync: ffi::SyncPoint = std::ptr::null_mut();
        let start = Instant::now();
        let r = loop {
            let c = if idr { Some(&mut *self.ctrl) } else { None };
            let r = self.rt.encode(c, surface, &mut self.bs, &mut sync);
            if r != ffi::WRN_DEVICE_BUSY || start.elapsed() >= BUSY_WAIT {
                break r;
            }
            std::thread::sleep(BUSY_POLL);
        };
        if r < ffi::ERR_NONE && !fatal(r) {
            self.rt.release(surface);
        }
        match r {
            ffi::ERR_MORE_DATA => {
                return Err(EncodeError(
                    "Quick Sync held a frame back (MFX_ERR_MORE_DATA); it was set up to code each at once"
                        .into(),
                ))
            }
            ffi::WRN_DEVICE_BUSY => {
                return Err(self.stop(format!(
                    "the device stayed busy for {} ms",
                    BUSY_WAIT.as_millis()
                )))
            }
            r if r < ffi::ERR_NONE => return Err(self.failed("MFXVideoENCODE_EncodeFrameAsync", r)),
            _ => {}
        }
        if sync.is_null() {
            self.rt.release(surface);
            return Err(EncodeError(
                "MFXVideoENCODE_EncodeFrameAsync gave no sync point".into(),
            ));
        }
        let r = self.rt.sync(sync, SYNC_WAIT_MS);
        if r == ffi::WRN_IN_EXECUTION {
            return Err(self.stop(format!(
                "the GPU did not finish a frame in {SYNC_WAIT_MS} ms"
            )));
        }
        if r < ffi::ERR_NONE {
            let e = self.failed("MFXVideoCORE_SyncOperation", r);
            if self.stopped.is_none() {
                self.rt.release(surface);
            }
            return Err(e);
        }
        self.rt.release(surface);
        let (off, len) = (self.bs.data_offset as usize, self.bs.data_length as usize);
        if off.checked_add(len).is_none_or(|end| end > self.buf.len()) {
            return Err(EncodeError(format!(
                "Quick Sync wrote {len} bytes at {off} into a {}-byte buffer",
                self.buf.len()
            )));
        }
        let data = self.buf[off..off + len].to_vec();
        let au = self.stream.unit(data, opts.pts_ms, idr)?;
        self.need_idr = false;
        Ok(au.into_iter().collect())
    }

    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        Ok(Vec::new())
    }

    /// Constant bitrate as the runtime last read it back; false while that is unknown.
    fn holds_cap(&self) -> bool {
        self.in_force.is_some_and(|f| f.cbr)
    }
}
