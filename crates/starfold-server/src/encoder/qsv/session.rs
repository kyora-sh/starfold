//! One Quick Sync session: IDR then P frames off the last frame only (one reference, no B
//! frames, one frame in flight), an IDR on request, parameter sets with every IDR, input
//! through the runtime's own system-memory NV12 surfaces. A device that hangs or is lost
//! ends the session for good, and with it the backend (see [`super::give_up`]).

use super::driver::{Fault, Runtime};
use super::ffi::{self, Bitstream, EncodeCtrl, ExtBuffer, ExtVideoSignalInfo, VideoParam};
use crate::encoder::hw::{self, Rc};
use crate::encoder::{
    AccessUnit, Codec, ColorInfo, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig,
    VideoEncoder,
};
use std::time::{Duration, Instant};

/// How long one frame may take on the GPU before the device counts as hung. The hardware
/// encodes a 1080p frame in a few milliseconds; a frame not done in three seconds is not
/// slow, and waiting forever (MFX_INFINITE) would hold the stream with it.
pub const SYNC_WAIT_MS: u32 = 3000;
/// How long a device that answers "busy" may keep a frame from being queued.
const BUSY_WAIT: Duration = Duration::from_millis(500);
const BUSY_POLL: Duration = Duration::from_millis(1);

/// Statuses after which the device is not asked again.
fn fatal(s: ffi::Status) -> bool {
    matches!(
        s,
        ffi::ERR_GPU_HANG | ffi::ERR_DEVICE_FAILED | ffi::ERR_DEVICE_LOST
    )
}

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

pub struct Session {
    rt: Box<dyn Runtime>,
    codec: Codec,
    size: (u32, u32),
    color: ColorInfo,
    /// What the encoder was set up with; its extension buffer list points at `ext`.
    par: VideoParam,
    signal: Box<ExtVideoSignalInfo>,
    ext: Box<[*mut ExtBuffer; 1]>,
    /// The session runs on VDEnc (fixed-function, LowPower on).
    pub low_power: bool,
    rc: Rc,
    /// The highest bitrate the session has been set up for: a higher one goes out at an IDR.
    peak_bps: u32,
    /// The next picture must be an IDR: first frame, a raised rate, or the last encode
    /// failed.
    need_idr: bool,
    buf: Vec<u8>,
    stream: hw::Stream,
    /// Why the device is not asked again.
    dead: Option<String>,
}

// The raw pointers point into the session's own boxes.
unsafe impl Send for Session {}

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
        let size = (cfg.width, cfg.height);
        let mut s = Session {
            rt,
            codec: cfg.codec,
            size,
            color,
            par: VideoParam::default(),
            signal: Box::new(signal(color)),
            ext: Box::new([std::ptr::null_mut()]),
            low_power: true,
            rc,
            peak_bps: rc.bps,
            need_idr: true,
            buf: Vec::new(),
            stream: hw::Stream::new("Quick Sync", cfg.codec, size, color, true),
            dead: None,
        };
        let mut refused = Vec::new();
        for low_power in [true, false] {
            let mode = if low_power {
                "LowPower on"
            } else {
                "LowPower off"
            };
            let mut q = s.with_ext(params(cfg.codec, size, &rc, low_power));
            let r = s.rt.query(&mut q);
            if r < ffi::ERR_NONE {
                refused.push(format!(
                    "{mode}: MFXVideoENCODE_Query: {}",
                    ffi::status_name(r)
                ));
                continue;
            }
            let mut p = s.with_ext(params(cfg.codec, size, &rc, low_power));
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

    /// `p` with the session's extension buffers.
    fn with_ext(&mut self, mut p: VideoParam) -> VideoParam {
        *self.signal = signal(self.color);
        self.ext[0] = &mut *self.signal as *mut ExtVideoSignalInfo as *mut ExtBuffer;
        p.ext_param = self.ext.as_mut_ptr();
        p.num_ext_param = 1;
        p
    }

    /// Reads back what the encoder runs with: refuses B frames, sizes the output buffer.
    fn refit(&mut self) -> Result<(), EncodeError> {
        let mut got = VideoParam::default();
        let r = self.rt.video_param(&mut got);
        if r < ffi::ERR_NONE {
            return Err(self.failed("MFXVideoENCODE_GetVideoParam", r));
        }
        if got.mfx.gop_ref_dist > 1 {
            return Err(EncodeError(format!(
                "the runtime set up B frames (GopRefDist {})",
                got.mfx.gop_ref_dist
            )));
        }
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

    /// The error for a call that returned `s`; a hung or lost device ends the session.
    fn failed(&mut self, what: &str, s: ffi::Status) -> EncodeError {
        let msg = format!("{what} failed: {} ({s})", ffi::status_name(s));
        if fatal(s) {
            self.die(msg)
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

    /// Stops using the device for good, and the backend with it.
    fn die(&mut self, why: String) -> EncodeError {
        let msg = format!("Quick Sync stopped: {why}");
        self.rt.abandon();
        super::give_up(&msg);
        self.dead = Some(msg.clone());
        EncodeError(msg)
    }

    fn alive(&self) -> Result<(), EncodeError> {
        match &self.dead {
            Some(why) => Err(EncodeError(why.clone())),
            None => Ok(()),
        }
    }
}

impl VideoEncoder for Session {
    fn set_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError> {
        self.alive()?;
        let rc = Rc::new(self.codec, &rate, Some(&self.rc));
        if rc == self.rc {
            return Ok(());
        }
        let raise = rc.bps > self.peak_bps;
        let mut p = self.par;
        apply(&rc, &mut p.mfx);
        let r = self.rt.reset(&mut p);
        if r < ffi::ERR_NONE {
            return Err(self.failed("MFXVideoENCODE_Reset", r));
        }
        self.par = p;
        self.rc = rc;
        if raise {
            self.peak_bps = rc.bps;
            self.need_idr = true;
        }
        self.refit()
    }

    fn encode(
        &mut self,
        f: &FrameView<'_>,
        opts: EncodeOptions<'_>,
    ) -> Result<Vec<AccessUnit>, EncodeError> {
        self.alive()?;
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
        let mut ctrl = EncodeCtrl {
            frame_type: ffi::FRAMETYPE_I | ffi::FRAMETYPE_REF | ffi::FRAMETYPE_IDR,
            ..Default::default()
        };
        let mut bs = Bitstream::over(&mut self.buf);
        let mut sync: ffi::SyncPoint = std::ptr::null_mut();
        let start = Instant::now();
        let r = loop {
            let c = if idr { Some(&mut ctrl) } else { None };
            let r = self.rt.encode(c, surface, &mut bs, &mut sync);
            if r != ffi::WRN_DEVICE_BUSY || start.elapsed() >= BUSY_WAIT {
                break r;
            }
            std::thread::sleep(BUSY_POLL);
        };
        self.rt.release(surface);
        match r {
            ffi::ERR_MORE_DATA => {
                return Err(EncodeError(
                    "Quick Sync held a frame back (MFX_ERR_MORE_DATA); it was set up to code each at once"
                        .into(),
                ))
            }
            ffi::WRN_DEVICE_BUSY => {
                return Err(self.die(format!(
                    "the device stayed busy for {} ms",
                    BUSY_WAIT.as_millis()
                )))
            }
            r if r < ffi::ERR_NONE => return Err(self.failed("MFXVideoENCODE_EncodeFrameAsync", r)),
            _ => {}
        }
        if sync.is_null() {
            return Err(EncodeError(
                "MFXVideoENCODE_EncodeFrameAsync gave no sync point".into(),
            ));
        }
        let r = self.rt.sync(sync, SYNC_WAIT_MS);
        if r == ffi::WRN_IN_EXECUTION {
            return Err(self.die(format!(
                "the GPU did not finish a frame in {SYNC_WAIT_MS} ms"
            )));
        }
        if r < ffi::ERR_NONE {
            return Err(self.failed("MFXVideoCORE_SyncOperation", r));
        }
        let (off, len) = (bs.data_offset as usize, bs.data_length as usize);
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
}
