//! One NVENC session: IDR then P frames off the last frame only (one reference, no B
//! frames, an infinite GOP), an IDR on request, parameter sets with every IDR.

use super::driver::Driver;
use super::ffi::{self, Config, InitParams};
use crate::encoder::h264::ParamSets;
use crate::encoder::{
    av1, hevc, AccessUnit, BitstreamFormat, Codec, ColorInfo, EncodeError, EncodeOptions,
    EncoderConfig, FrameView, RateConfig, StreamConfig, VideoEncoder,
};

/// A temporal delimiter OBU.
const TD: [u8; 2] = [0x12, 0x00];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rc {
    pub cbr: bool,
    pub bps: u32,
    pub buffer_bits: u32,
    pub fps: u32,
    pub qp: u32,
}

impl Rc {
    /// CBR when `rate` caps the bitrate, else constant QP. A session keeps its mode, and
    /// an uncapped rate on a CBR one keeps its bitrate.
    pub fn new(codec: Codec, rate: &RateConfig, prev: Option<&Rc>) -> Rc {
        let cbr = prev.map_or(rate.max_kbps > 0, |p| p.cbr);
        let (bps, buffer_bits) = match (cbr, rate.max_kbps, prev) {
            (false, ..) => (0, 0),
            (true, 0, Some(p)) => (p.bps, p.buffer_bits),
            (true, k, _) => (
                k.saturating_mul(1000),
                rate.buf_kbit.max(1).saturating_mul(1000),
            ),
        };
        let q = rate.quality.round() as i32;
        let qp = match codec {
            Codec::Av1 => (q * 5).clamp(1, 255),
            _ => q.clamp(1, 51),
        } as u32;
        Rc {
            cbr,
            bps,
            buffer_bits,
            fps: rate.fps.max(1),
            qp,
        }
    }

    fn apply(&self, rc: &mut ffi::RcParams) {
        rc.version = ffi::RC_PARAMS_VER;
        rc.flags = (rc.flags | ffi::RC_ZERO_REORDER_DELAY) & !ffi::RC_ENABLE_LOOKAHEAD;
        rc.lookahead_depth = 0;
        if self.cbr {
            rc.mode = ffi::RC_CBR;
            rc.average_bit_rate = self.bps;
            rc.max_bit_rate = self.bps;
            rc.vbv_buffer_size = self.buffer_bits;
            rc.vbv_initial_delay = self.buffer_bits;
        } else {
            rc.mode = ffi::RC_CONSTQP;
            rc.const_qp = ffi::Qp {
                inter_p: self.qp,
                inter_b: self.qp,
                intra: self.qp,
            };
        }
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
    rc.apply(&mut c.rc);
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

enum Units {
    H264(ParamSets),
    Hevc(hevc::Units),
    Av1(Option<Vec<u8>>),
}

pub struct Session {
    drv: Box<dyn Driver>,
    codec: Codec,
    init: Box<InitParams>,
    cfg: Box<Config>,
    size: (u32, u32),
    color: ColorInfo,
    rc: Rc,
    /// The highest bitrate the session has been set up for: the driver picked its level
    /// for it, so a higher one reconfigures at an IDR.
    peak_bps: u32,
    /// The next picture must be an IDR: first frame, a raised rate, or the last encode
    /// failed.
    need_idr: bool,
    /// A stream config has gone out.
    configured: bool,
    units: Units,
}

impl Session {
    pub fn new(mut drv: Box<dyn Driver>, cfg: &EncoderConfig) -> Result<Session, EncodeError> {
        let (guid, units) = match cfg.codec {
            Codec::H264 => (ffi::CODEC_H264, Units::H264(ParamSets::default())),
            Codec::Hevc => (
                ffi::CODEC_HEVC,
                Units::Hevc(hevc::Units::new(cfg.width, cfg.height)),
            ),
            Codec::Av1 => (ffi::CODEC_AV1, Units::Av1(None)),
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
            color,
            rc,
            peak_bps: rc.bps,
            need_idr: true,
            configured: false,
            units,
        })
    }

    fn unit(
        &mut self,
        data: Vec<u8>,
        pts_ms: i64,
        idr: bool,
    ) -> Result<Option<AccessUnit>, EncodeError> {
        let au = match &mut self.units {
            Units::H264(p) => p.unit(data, pts_ms, self.size, self.color),
            Units::Hevc(u) => u.unit(data, pts_ms),
            Units::Av1(seq) => av1_unit(seq, data, pts_ms, idr, self.size, self.color)?,
        };
        match &au {
            None if idr => Err(EncodeError(format!(
                "NVENC {:?} IDR came out without a picture",
                self.codec
            ))),
            Some(a) if a.keyframe != idr => Err(EncodeError(format!(
                "NVENC {:?} asked for {}, got {}",
                self.codec,
                if idr { "an IDR" } else { "a P frame" },
                if a.keyframe { "an IDR" } else { "a P frame" }
            ))),
            Some(a) if a.config.is_none() && !self.configured => Err(EncodeError(format!(
                "NVENC {:?} stream began without readable parameter sets",
                self.codec
            ))),
            _ => {
                self.configured |= au.as_ref().is_some_and(|a| a.config.is_some());
                Ok(au)
            }
        }
    }
}

fn av1_unit(
    seq: &mut Option<Vec<u8>>,
    data: Vec<u8>,
    pts_ms: i64,
    key: bool,
    size: (u32, u32),
    color: ColorInfo,
) -> Result<Option<AccessUnit>, EncodeError> {
    if data.is_empty() {
        return Ok(None);
    }
    let mut tu = Vec::with_capacity(data.len() + TD.len());
    if (data[0] >> 3) & 0xf != 2 {
        tu.extend_from_slice(&TD);
    }
    tu.extend(data);
    let sh = av1::sequence_header(&tu).map(<[u8]>::to_vec);
    let config = match (&sh, key) {
        (None, true) => {
            return Err(EncodeError(
                "NVENC AV1 key frame without a sequence header".into(),
            ))
        }
        (Some(s), true) if seq.as_ref() != Some(s) => {
            let codec_string = av1::codec_string(&tu)
                .ok_or_else(|| EncodeError("unreadable AV1 sequence header".into()))?;
            *seq = sh;
            Some(StreamConfig {
                codec: Codec::Av1,
                codec_string,
                coded: size,
                display: size,
                format: BitstreamFormat::Av1LowOverhead,
                color,
            })
        }
        _ => None,
    };
    Ok(Some(AccessUnit {
        pts_ms,
        keyframe: key,
        data: tu,
        config,
    }))
}

impl VideoEncoder for Session {
    fn set_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError> {
        let rc = Rc::new(self.codec, &rate, Some(&self.rc));
        if rc == self.rc {
            return Ok(());
        }
        let raise = rc.bps > self.peak_bps;
        let mut cfg = self.cfg.clone();
        rc.apply(&mut cfg.rc);
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
        let au = self.unit(data, opts.pts_ms, idr)?;
        self.need_idr = false;
        Ok(au.into_iter().collect())
    }

    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        Ok(Vec::new())
    }
}
