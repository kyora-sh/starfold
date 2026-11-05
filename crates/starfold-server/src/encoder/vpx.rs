//! libvpx (BSD-3-Clause, only with the `libvpx` feature): VP8 and VP9 in realtime mode,
//! tuned for screen content as WebRTC screen share does.

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code,
    unused_imports,
    clippy::all
)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/vpx.rs"));
}

use super::{
    AccessUnit, BitstreamFormat, Codec, ColorInfo, EncodeError, EncodeOptions, EncoderConfig,
    FrameView, RateConfig, StreamConfig, VideoEncoder,
};
use std::ffi::CStr;
use std::os::raw::{c_int, c_uint};
use std::ptr;

const MAX_Q: u32 = 56;
/// Uncapped still needs a target: this one leaves the lowest QP to decide.
const UNCAPPED_KBPS: u32 = 200_000;

pub struct Vpx {
    // libvpx keeps pointers to the configuration it was given, so neither may move.
    ctx: Box<ffi::vpx_codec_ctx_t>,
    cfg: Box<ffi::vpx_codec_enc_cfg_t>,
    codec: Codec,
    pts: i64,
    duration: u64,
    announced: bool,
}

unsafe impl Send for Vpx {}

fn lowest_q(quality: f32) -> u32 {
    (quality.round().max(0.0) as u32).min(MAX_Q)
}

fn apply(cfg: &mut ffi::vpx_codec_enc_cfg_t, rate: RateConfig) {
    cfg.rc_target_bitrate = if rate.max_kbps == 0 {
        UNCAPPED_KBPS
    } else {
        rate.max_kbps
    };
    cfg.rc_min_quantizer = lowest_q(rate.quality);
}

/// VP9 profile 0, 8-bit, at the lowest level whose picture size fits.
fn vp9_string(w: u32, h: u32) -> String {
    let level = match w as u64 * h as u64 {
        0..=2_228_224 => 41,
        2_228_225..=8_912_896 => 51,
        _ => 62,
    };
    format!("vp09.00.{level}.08")
}

impl Vpx {
    pub fn new(c: &EncoderConfig) -> Result<Self, EncodeError> {
        let (iface, speed) = unsafe {
            match c.codec {
                Codec::Vp8 => (ffi::vpx_codec_vp8_cx(), 8),
                Codec::Vp9 => (ffi::vpx_codec_vp9_cx(), 8),
                other => return Err(EncodeError(format!("libvpx can't encode {other:?}"))),
            }
        };
        let speed: c_int = c.preset.parse().unwrap_or(speed);
        unsafe {
            let mut cfg: Box<ffi::vpx_codec_enc_cfg_t> = Box::new(std::mem::zeroed());
            let r = ffi::vpx_codec_enc_config_default(iface, &mut *cfg, 0);
            if r != ffi::vpx_codec_err_t_VPX_CODEC_OK {
                return Err(EncodeError(format!("libvpx default config failed ({r})")));
            }
            cfg.g_w = c.width;
            cfg.g_h = c.height;
            cfg.g_threads = c.threads.max(1);
            cfg.g_timebase = ffi::vpx_rational { num: 1, den: 1000 };
            cfg.g_pass = ffi::vpx_enc_pass_VPX_RC_ONE_PASS;
            cfg.g_lag_in_frames = 0;
            cfg.rc_end_usage = ffi::vpx_rc_mode_VPX_CBR;
            cfg.rc_dropframe_thresh = 0;
            cfg.rc_resize_allowed = 0;
            cfg.rc_max_quantizer = MAX_Q;
            cfg.rc_buf_sz = 1000;
            cfg.rc_buf_initial_sz = 500;
            cfg.rc_buf_optimal_sz = 600;
            cfg.kf_mode = ffi::vpx_kf_mode_VPX_KF_DISABLED;
            apply(&mut cfg, c.rate);
            let mut ctx: Box<ffi::vpx_codec_ctx_t> = Box::new(std::mem::zeroed());
            let r = ffi::vpx_codec_enc_init_ver(&mut *ctx, iface, &*cfg, 0, ffi::STARFOLD_VPX_ABI);
            if r != ffi::vpx_codec_err_t_VPX_CODEC_OK {
                return Err(EncodeError(format!("libvpx init failed ({r})")));
            }
            let enc = Vpx {
                ctx,
                cfg,
                codec: c.codec,
                pts: -1,
                duration: (1000 / c.fps.max(1)).max(1) as u64,
                announced: false,
            };
            enc.controls(speed)?;
            Ok(enc)
        }
    }

    fn control(&self, id: u32, v: c_int) -> Result<(), EncodeError> {
        let ctx = &*self.ctx as *const _ as *mut ffi::vpx_codec_ctx_t;
        let r = unsafe { ffi::vpx_codec_control_(ctx, id as c_int, v) };
        if r == ffi::vpx_codec_err_t_VPX_CODEC_OK {
            Ok(())
        } else {
            Err(self.error(&format!("control {id}")))
        }
    }

    fn controls(&self, speed: c_int) -> Result<(), EncodeError> {
        self.control(ffi::vp8e_enc_control_id_VP8E_SET_CPUUSED, speed)?;
        match self.codec {
            Codec::Vp8 => {
                self.control(ffi::vp8e_enc_control_id_VP8E_SET_SCREEN_CONTENT_MODE, 1)?;
                self.control(ffi::vp8e_enc_control_id_VP8E_SET_STATIC_THRESHOLD, 1)
            }
            _ => {
                let tiles = (self.cfg.g_threads.max(1)).ilog2().min(4) as c_int;
                self.control(
                    ffi::vp8e_enc_control_id_VP9E_SET_TUNE_CONTENT,
                    ffi::vp9e_tune_content_VP9E_CONTENT_SCREEN as c_int,
                )?;
                self.control(ffi::vp8e_enc_control_id_VP9E_SET_ROW_MT, 1)?;
                self.control(ffi::vp8e_enc_control_id_VP9E_SET_TILE_COLUMNS, tiles)?;
                self.control(ffi::vp8e_enc_control_id_VP9E_SET_AQ_MODE, 3)?;
                self.control(
                    ffi::vp8e_enc_control_id_VP9E_SET_COLOR_SPACE,
                    ffi::vpx_color_space_VPX_CS_BT_601 as c_int,
                )?;
                self.control(
                    ffi::vp8e_enc_control_id_VP9E_SET_COLOR_RANGE,
                    ffi::vpx_color_range_VPX_CR_STUDIO_RANGE as c_int,
                )
            }
        }
    }

    fn error(&self, what: &str) -> EncodeError {
        let ctx = &*self.ctx as *const _ as *mut ffi::vpx_codec_ctx_t;
        let msg = unsafe {
            let p = ffi::vpx_codec_error(ctx);
            if p.is_null() {
                "unknown error".to_string()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        EncodeError(format!("libvpx {what}: {msg}"))
    }

    fn collect(&mut self) -> Vec<AccessUnit> {
        let mut out = Vec::new();
        let mut iter: ffi::vpx_codec_iter_t = ptr::null();
        loop {
            let pkt = unsafe { ffi::vpx_codec_get_cx_data(&mut *self.ctx, &mut iter) };
            if pkt.is_null() {
                return out;
            }
            let pkt = unsafe { &*pkt };
            if pkt.kind != ffi::vpx_codec_cx_pkt_kind_VPX_CODEC_CX_FRAME_PKT {
                continue;
            }
            let frame = unsafe { pkt.data.frame };
            if frame.buf.is_null() || frame.sz == 0 {
                continue;
            }
            let data =
                unsafe { std::slice::from_raw_parts(frame.buf as *const u8, frame.sz) }.to_vec();
            let config = (!self.announced).then(|| self.stream_config());
            self.announced = true;
            out.push(AccessUnit {
                pts_ms: frame.pts,
                keyframe: frame.flags & ffi::VPX_FRAME_IS_KEY != 0,
                data,
                config,
            });
        }
    }

    fn stream_config(&self) -> StreamConfig {
        let (w, h) = (self.cfg.g_w, self.cfg.g_h);
        StreamConfig {
            codec: self.codec,
            codec_string: match self.codec {
                Codec::Vp8 => "vp8".into(),
                _ => vp9_string(w, h),
            },
            coded: (w, h),
            display: (w, h),
            format: BitstreamFormat::Vpx,
            color: ColorInfo::SRGB_BT601_LIMITED,
        }
    }
}

impl VideoEncoder for Vpx {
    fn set_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError> {
        apply(&mut self.cfg, rate);
        let r = unsafe { ffi::vpx_codec_enc_config_set(&mut *self.ctx, &*self.cfg) };
        if r == ffi::vpx_codec_err_t_VPX_CODEC_OK {
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
                "libvpx opened at {}x{}, got {}x{}",
                self.cfg.g_w, self.cfg.g_h, p.width, p.height
            )));
        }
        p.check()?;
        self.pts = opts.pts_ms.max(self.pts + 1);
        let flags = if opts.force_keyframe {
            ffi::VPX_EFLAG_FORCE_KF as ffi::vpx_enc_frame_flags_t
        } else {
            0
        };
        unsafe {
            let mut img: ffi::vpx_image_t = std::mem::zeroed();
            let wrapped = ffi::vpx_img_wrap(
                &mut img,
                ffi::vpx_img_fmt_VPX_IMG_FMT_I420,
                p.width as c_uint,
                p.height as c_uint,
                1,
                p.y.as_ptr() as *mut u8,
            );
            if wrapped.is_null() {
                return Err(EncodeError("vpx_img_wrap failed".into()));
            }
            img.planes[0] = p.y.as_ptr() as *mut u8;
            img.planes[1] = p.u.as_ptr() as *mut u8;
            img.planes[2] = p.v.as_ptr() as *mut u8;
            img.stride[0] = p.stride_y as c_int;
            img.stride[1] = p.stride_uv as c_int;
            img.stride[2] = p.stride_uv as c_int;
            let r = ffi::vpx_codec_encode(
                &mut *self.ctx,
                &img,
                self.pts,
                self.duration as _,
                flags,
                ffi::VPX_DL_REALTIME as _,
            );
            if r != ffi::vpx_codec_err_t_VPX_CODEC_OK {
                return Err(self.error("encode"));
            }
        }
        Ok(self.collect())
    }

    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        let r = unsafe {
            ffi::vpx_codec_encode(
                &mut *self.ctx,
                ptr::null(),
                0,
                0,
                0,
                ffi::VPX_DL_REALTIME as _,
            )
        };
        if r != ffi::vpx_codec_err_t_VPX_CODEC_OK {
            return Err(self.error("flush"));
        }
        Ok(self.collect())
    }
}

impl Drop for Vpx {
    fn drop(&mut self) {
        unsafe {
            ffi::vpx_codec_destroy(&mut *self.ctx);
        }
    }
}
