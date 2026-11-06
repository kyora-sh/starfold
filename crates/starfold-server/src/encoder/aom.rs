//! libaom (BSD-2-Clause, only with the `libaom` feature): AV1 in realtime mode with the
//! screen-content tools (palette, intra block copy), as WebRTC AV1 screen share uses it.

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code,
    unused_imports,
    clippy::all
)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/aom.rs"));
}

use super::{
    av1, AccessUnit, BitstreamFormat, Codec, ColorInfo, EncodeError, EncodeOptions, EncoderConfig,
    FrameView, RateConfig, StreamConfig, VideoEncoder,
};
use std::ffi::CStr;
use std::os::raw::{c_int, c_uint};
use std::ptr;

const MAX_Q: u32 = 63;
const UNCAPPED_KBPS: u32 = 200_000;
const SPEED: c_int = 10;

pub struct Aom {
    // libaom keeps pointers to the configuration it was given, so neither may move.
    ctx: Box<ffi::aom_codec_ctx_t>,
    cfg: Box<ffi::aom_codec_enc_cfg_t>,
    pts: i64,
    duration: u64,
    announced: bool,
}

unsafe impl Send for Aom {}

fn lowest_q(quality: f32) -> u32 {
    (quality.round().max(0.0) as u32).min(MAX_Q)
}

fn apply(cfg: &mut ffi::aom_codec_enc_cfg_t, rate: RateConfig) {
    cfg.rc_target_bitrate = if rate.max_kbps == 0 {
        UNCAPPED_KBPS
    } else {
        rate.max_kbps
    };
    cfg.rc_min_quantizer = lowest_q(rate.quality);
}

/// The lowest main-tier level (seq_level_idx) whose picture size fits: 4.0, 5.0 or 6.0.
fn level(w: u32, h: u32) -> c_int {
    match w as u64 * h as u64 {
        0..=2_228_224 => 8,
        2_228_225..=8_912_896 => 12,
        _ => 16,
    }
}

impl Aom {
    pub fn new(c: &EncoderConfig) -> Result<Self, EncodeError> {
        if c.codec != Codec::Av1 {
            return Err(EncodeError(format!("libaom can't encode {:?}", c.codec)));
        }
        let speed = super::arm_speed("libaom", &c.preset, SPEED)?;
        unsafe {
            let iface = ffi::aom_codec_av1_cx();
            let mut cfg: Box<ffi::aom_codec_enc_cfg_t> = Box::new(std::mem::zeroed());
            let r = ffi::aom_codec_enc_config_default(iface, &mut *cfg, ffi::AOM_USAGE_REALTIME);
            if r != ffi::aom_codec_err_t_AOM_CODEC_OK {
                return Err(EncodeError(format!("libaom default config failed ({r})")));
            }
            cfg.g_w = c.width;
            cfg.g_h = c.height;
            cfg.g_threads = c.threads.max(1);
            cfg.g_timebase = ffi::aom_rational { num: 1, den: 1000 };
            cfg.g_pass = ffi::aom_enc_pass_AOM_RC_ONE_PASS;
            cfg.g_lag_in_frames = 0;
            cfg.rc_end_usage = ffi::aom_rc_mode_AOM_CBR;
            cfg.rc_dropframe_thresh = 0;
            cfg.rc_resize_mode = 0;
            cfg.rc_superres_mode = 0;
            cfg.rc_max_quantizer = MAX_Q;
            cfg.rc_buf_sz = 1000;
            cfg.rc_buf_initial_sz = 500;
            cfg.rc_buf_optimal_sz = 600;
            cfg.kf_mode = ffi::aom_kf_mode_AOM_KF_DISABLED;
            apply(&mut cfg, c.rate);
            let mut ctx: Box<ffi::aom_codec_ctx_t> = Box::new(std::mem::zeroed());
            let r = ffi::aom_codec_enc_init_ver(&mut *ctx, iface, &*cfg, 0, ffi::STARFOLD_AOM_ABI);
            if r != ffi::aom_codec_err_t_AOM_CODEC_OK {
                return Err(EncodeError(format!("libaom init failed ({r})")));
            }
            let mut enc = Aom {
                ctx,
                cfg,
                pts: -1,
                duration: (1000 / c.fps.max(1)).max(1) as u64,
                announced: false,
            };
            enc.controls(speed)?;
            Ok(enc)
        }
    }

    fn control(&mut self, id: u32, v: c_int) -> Result<(), EncodeError> {
        let ctx: *mut ffi::aom_codec_ctx_t = &mut *self.ctx;
        let r = unsafe { ffi::aom_codec_control(ctx, id as c_int, v) };
        if r == ffi::aom_codec_err_t_AOM_CODEC_OK {
            Ok(())
        } else {
            Err(self.error(&format!("control {id}")))
        }
    }

    fn controls(&mut self, speed: c_int) -> Result<(), EncodeError> {
        use ffi::*;
        let color = ColorInfo::SRGB_BT601_LIMITED;
        let tiles = (self.cfg.g_threads.max(1)).ilog2().min(4) as c_int;
        let (w, h) = (self.cfg.g_w, self.cfg.g_h);
        for (id, v) in [
            (aome_enc_control_id_AOME_SET_CPUUSED, speed),
            (
                aome_enc_control_id_AV1E_SET_TUNE_CONTENT,
                aom_tune_content_AOM_CONTENT_SCREEN as c_int,
            ),
            (aome_enc_control_id_AV1E_SET_ENABLE_PALETTE, 1),
            (aome_enc_control_id_AV1E_SET_ENABLE_INTRABC, 1),
            (aome_enc_control_id_AV1E_SET_ROW_MT, 1),
            (aome_enc_control_id_AV1E_SET_TILE_COLUMNS, tiles),
            (aome_enc_control_id_AV1E_SET_AQ_MODE, 3),
            (
                aome_enc_control_id_AV1E_SET_TARGET_SEQ_LEVEL_IDX,
                level(w, h),
            ),
            (
                aome_enc_control_id_AV1E_SET_COLOR_PRIMARIES,
                color.primaries as c_int,
            ),
            (
                aome_enc_control_id_AV1E_SET_TRANSFER_CHARACTERISTICS,
                color.transfer as c_int,
            ),
            (
                aome_enc_control_id_AV1E_SET_MATRIX_COEFFICIENTS,
                color.matrix as c_int,
            ),
            (
                aome_enc_control_id_AV1E_SET_COLOR_RANGE,
                aom_color_range_AOM_CR_STUDIO_RANGE as c_int,
            ),
        ] {
            self.control(id, v)?;
        }
        Ok(())
    }

    fn error(&self, what: &str) -> EncodeError {
        let ctx: *const ffi::aom_codec_ctx_t = &*self.ctx;
        let msg = unsafe {
            let p = ffi::aom_codec_error(ctx);
            if p.is_null() {
                "unknown error".to_string()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        EncodeError(format!("libaom {what}: {msg}"))
    }

    fn collect(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        let mut out = Vec::new();
        let mut iter: ffi::aom_codec_iter_t = ptr::null();
        loop {
            let pkt = unsafe { ffi::aom_codec_get_cx_data(&mut *self.ctx, &mut iter) };
            if pkt.is_null() {
                return Ok(out);
            }
            let pkt = unsafe { &*pkt };
            if pkt.kind != ffi::aom_codec_cx_pkt_kind_AOM_CODEC_CX_FRAME_PKT {
                continue;
            }
            let frame = unsafe { pkt.data.frame };
            if frame.buf.is_null() || frame.sz == 0 {
                continue;
            }
            let data =
                unsafe { std::slice::from_raw_parts(frame.buf as *const u8, frame.sz) }.to_vec();
            let keyframe = frame.flags & ffi::AOM_FRAME_IS_KEY != 0;
            let config = if keyframe && !self.announced {
                let codec_string = av1::codec_string(&data).ok_or_else(|| {
                    EncodeError("libaom keyframe without a sequence header".into())
                })?;
                self.announced = true;
                Some(StreamConfig {
                    codec: Codec::Av1,
                    codec_string,
                    coded: (self.cfg.g_w, self.cfg.g_h),
                    display: (self.cfg.g_w, self.cfg.g_h),
                    format: BitstreamFormat::Av1LowOverhead,
                    color: ColorInfo::SRGB_BT601_LIMITED,
                })
            } else {
                None
            };
            out.push(AccessUnit {
                pts_ms: frame.pts,
                keyframe,
                data,
                config,
            });
        }
    }
}

impl VideoEncoder for Aom {
    fn set_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError> {
        apply(&mut self.cfg, rate);
        let r = unsafe { ffi::aom_codec_enc_config_set(&mut *self.ctx, &*self.cfg) };
        if r == ffi::aom_codec_err_t_AOM_CODEC_OK {
            Ok(())
        } else {
            Err(self.error("rate change"))
        }
    }

    fn encode(
        &mut self,
        p: &FrameView<'_>,
        opts: EncodeOptions<'_>,
    ) -> Result<Vec<AccessUnit>, EncodeError> {
        if (p.width, p.height) != (self.cfg.g_w, self.cfg.g_h) {
            return Err(EncodeError(format!(
                "libaom opened at {}x{}, got {}x{}",
                self.cfg.g_w, self.cfg.g_h, p.width, p.height
            )));
        }
        p.check()?;
        self.pts = opts.pts_ms.max(self.pts + 1);
        let flags = if opts.force_keyframe {
            ffi::AOM_EFLAG_FORCE_KF as ffi::aom_enc_frame_flags_t
        } else {
            0
        };
        unsafe {
            let mut img: ffi::aom_image_t = std::mem::zeroed();
            let wrapped = ffi::aom_img_wrap(
                &mut img,
                ffi::aom_img_fmt_AOM_IMG_FMT_I420,
                p.width as c_uint,
                p.height as c_uint,
                1,
                p.y.as_ptr() as *mut u8,
            );
            if wrapped.is_null() {
                return Err(EncodeError("aom_img_wrap failed".into()));
            }
            img.planes[0] = p.y.as_ptr() as *mut u8;
            img.planes[1] = p.u.as_ptr() as *mut u8;
            img.planes[2] = p.v.as_ptr() as *mut u8;
            img.stride[0] = p.stride_y as c_int;
            img.stride[1] = p.stride_uv as c_int;
            img.stride[2] = p.stride_uv as c_int;
            img.range = ffi::aom_color_range_AOM_CR_STUDIO_RANGE;
            let r =
                ffi::aom_codec_encode(&mut *self.ctx, &img, self.pts, self.duration as _, flags);
            if r != ffi::aom_codec_err_t_AOM_CODEC_OK {
                return Err(self.error("encode"));
            }
        }
        self.collect()
    }

    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        let mut out = Vec::new();
        loop {
            let r = unsafe { ffi::aom_codec_encode(&mut *self.ctx, ptr::null(), 0, 0, 0) };
            if r != ffi::aom_codec_err_t_AOM_CODEC_OK {
                return Err(self.error("flush"));
            }
            let more = self.collect()?;
            if more.is_empty() {
                return Ok(out);
            }
            out.extend(more);
        }
    }
}

impl Drop for Aom {
    fn drop(&mut self) {
        unsafe {
            ffi::aom_codec_destroy(&mut *self.ctx);
        }
    }
}
