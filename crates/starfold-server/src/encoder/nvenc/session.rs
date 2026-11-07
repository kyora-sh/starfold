//! One NVENC session: IDR then P frames off the last frame only (one reference, no B
//! frames, an infinite GOP), an IDR on request, parameter sets with every IDR.

use super::driver::Driver;
use super::ffi::{self, Config, InitParams};
use crate::encoder::hw::{self, Rc};
use crate::encoder::{
    AccessUnit, Codec, ColorInfo, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig,
    VideoEncoder,
};

/// Low-delay rate control in `rc`: CBR at `r`'s bitrate, else constant QP.
fn apply(r: &Rc, rc: &mut ffi::RcParams) {
    rc.version = ffi::RC_PARAMS_VER;
    rc.flags = (rc.flags | ffi::RC_ZERO_REORDER_DELAY) & !ffi::RC_ENABLE_LOOKAHEAD;
    rc.lookahead_depth = 0;
    if r.cbr {
        rc.mode = ffi::RC_CBR;
        rc.average_bit_rate = r.bps;
        rc.max_bit_rate = r.bps;
        rc.vbv_buffer_size = r.buffer_bits;
        rc.vbv_initial_delay = r.buffer_bits;
    } else {
        rc.mode = ffi::RC_CONSTQP;
        rc.const_qp = ffi::Qp {
            inter_p: r.qp,
            inter_b: r.qp,
            intra: r.qp,
        };
    }
}

fn vui(v: &mut ffi::Vui, c: ColorInfo) {
    v.video_signal_type_present_flag = 1;
    v.video_format = 5;
    v.video_full_range_flag = c.full_range as u32;
    v.colour_description_present_flag = 1;
    v.colour_primaries = c.primaries as u32;
    v.transfer_characteristics = c.transfer as u32;
    v.colour_matrix = c.matrix as u32;
}

/// Low-delay IDR + P with one reference and headers on every IDR, over a preset's `c`.
pub fn configure(c: &mut Config, codec: Codec, rc: &Rc, color: ColorInfo) {
    c.version = ffi::CONFIG_VER;
    c.gop_length = ffi::INFINITE_GOP;
    c.frame_interval_p = 1;
    apply(rc, &mut c.rc);
    unsafe {
        match codec {
            Codec::H264 => {
                let h = &mut c.codec.h264;
                h.flags = (h.flags | ffi::H264_REPEAT_SPSPPS)
                    & !(ffi::H264_DISABLE_SPSPPS | ffi::H264_INTRA_REFRESH);
                h.idr_period = ffi::INFINITE_GOP;
                h.max_num_ref_frames = 1;
                h.num_ref_l0 = ffi::NUM_REF_FRAMES_1;
                vui(&mut h.vui, color);
            }
            Codec::Hevc => {
                let h = &mut c.codec.hevc;
                h.flags = (h.flags | ffi::HEVC_REPEAT_SPSPPS)
                    & !(ffi::HEVC_DISABLE_SPSPPS | ffi::HEVC_INTRA_REFRESH);
                h.idr_period = ffi::INFINITE_GOP;
                h.max_num_ref_frames_in_dpb = 1;
                h.num_ref_l0 = ffi::NUM_REF_FRAMES_1;
                vui(&mut h.vui, color);
            }
            Codec::Av1 => {
                let a = &mut c.codec.av1;
                a.flags = (a.flags | ffi::AV1_REPEAT_SEQ_HDR)
                    & !(ffi::AV1_ANNEXB | ffi::AV1_DISABLE_SEQ_HDR | ffi::AV1_INTRA_REFRESH);
                a.idr_period = ffi::INFINITE_GOP;
                a.max_num_ref_frames_in_dpb = 1;
                a.num_fwd_refs = ffi::NUM_REF_FRAMES_1;
                a.color_primaries = color.primaries as u32;
                a.transfer_characteristics = color.transfer as u32;
                a.matrix_coefficients = color.matrix as u32;
                a.color_range = color.full_range as u32;
            }
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }
}

pub struct Session {
    drv: Box<dyn Driver>,
    codec: Codec,
    init: Box<InitParams>,
    cfg: Box<Config>,
    size: (u32, u32),
    rc: Rc,
    /// The highest bitrate the session has been set up for: the driver picked its level
    /// for it, so a higher one reconfigures at an IDR.
    peak_bps: u32,
    /// The next picture must be an IDR: first frame, a raised rate, or the last encode
    /// failed.
    need_idr: bool,
    stream: hw::Stream,
}

impl Session {
    pub fn new(mut drv: Box<dyn Driver>, cfg: &EncoderConfig) -> Result<Session, EncodeError> {
        let guid = match cfg.codec {
            Codec::H264 => ffi::CODEC_H264,
            Codec::Hevc => ffi::CODEC_HEVC,
            Codec::Av1 => ffi::CODEC_AV1,
            #[allow(unreachable_patterns)]
            c => return Err(EncodeError(format!("NVENC has no {c:?} encoder"))),
        };
        let color = ColorInfo::SRGB_BT601_LIMITED;
        let rc = Rc::new(cfg.codec, &cfg.rate, None);
        let mut c = drv.preset(guid)?;
        configure(&mut c, cfg.codec, &rc, color);
        let mut init: Box<InitParams> = Box::new(unsafe { std::mem::zeroed() });
        init.version = ffi::INITIALIZE_PARAMS_VER;
        init.encode_guid = guid;
        init.preset_guid = ffi::PRESET_P4;
        init.encode_width = cfg.width;
        init.encode_height = cfg.height;
        init.dar_width = cfg.width;
        init.dar_height = cfg.height;
        init.max_encode_width = cfg.width;
        init.max_encode_height = cfg.height;
        init.frame_rate_num = rc.fps;
        init.frame_rate_den = 1;
        init.enable_ptd = 1;
        init.tuning_info = ffi::TUNING_ULTRA_LOW_LATENCY;
        drv.initialize(&init, &c)?;
        Ok(Session {
            drv,
            codec: cfg.codec,
            init,
            cfg: c,
            size: (cfg.width, cfg.height),
            rc,
            peak_bps: rc.bps,
            need_idr: true,
            stream: hw::Stream::new("NVENC", cfg.codec, (cfg.width, cfg.height), color, false),
        })
    }
}

impl VideoEncoder for Session {
    fn set_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError> {
        let rc = Rc::new(self.codec, &rate, Some(&self.rc));
        if rc == self.rc {
            return Ok(());
        }
        let raise = rc.bps > self.peak_bps;
        let mut cfg = self.cfg.clone();
        apply(&rc, &mut cfg.rc);
        let mut init = self.init.clone();
        init.frame_rate_num = rc.fps;
        let flags = if raise {
            ffi::RECONFIGURE_RESET | ffi::RECONFIGURE_FORCE_IDR
        } else {
            0
        };
        self.drv.reconfigure(&init, &cfg, flags)?;
        self.cfg = cfg;
        self.init = init;
        self.rc = rc;
        if raise {
            self.peak_bps = rc.bps;
            self.need_idr = true;
        }
        Ok(())
    }

    fn encode(
        &mut self,
        f: &FrameView<'_>,
        opts: EncodeOptions<'_>,
    ) -> Result<Vec<AccessUnit>, EncodeError> {
        if (f.width, f.height) != self.size {
            return Err(EncodeError(format!(
                "encoder opened at {}x{}, got {}x{}",
                self.size.0, self.size.1, f.width, f.height
            )));
        }
        let idr = opts.force_keyframe || self.need_idr;
        self.need_idr = true;
        let flags = if idr {
            ffi::PIC_FLAG_FORCEIDR | ffi::PIC_FLAG_OUTPUT_SPSPPS
        } else {
            0
        };
        let data = self.drv.encode(f, flags)?;
        let au = self.stream.unit(data, opts.pts_ms, idr)?;
        self.need_idr = false;
        Ok(au.into_iter().collect())
    }

    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        Ok(Vec::new())
    }
}
