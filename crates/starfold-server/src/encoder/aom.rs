//! libaom (BSD-2-Clause, only with the `libaom` feature, built from source by
//! starfold-aom-sys): AV1 in realtime mode with the screen-content tools (palette, intra
//! block copy), as WebRTC AV1 screen share uses it.
//!
//! A paint-over is sharpened by QP, as with x264: a refinement frame runs at a fixed
//! quantizer and predicts from the last picture named as GOLDEN, with LAST itself off.
//! libaom's real-time path skips a block whose source didn't change only when LAST is a
//! reference, so this is what makes it re-code a still picture.

use starfold_aom_sys as ffi;

use super::sharpness::Sharpness;
use super::{
    av1, AccessUnit, BitstreamFormat, Codec, ColorInfo, EncodeError, EncodeOptions, EncoderConfig,
    FrameView, RateConfig, Refine, StreamConfig, VideoEncoder,
};
use std::ffi::CStr;
use std::os::raw::{c_int, c_uint};
use std::ptr;

const MAX_Q: u32 = 63;
const UNCAPPED_KBPS: u32 = 200_000;
const SPEED: c_int = 9;
/// The quantizer of the ladder's last rung (H.264 QP 12 and below): on video, the one that
/// settles where x264's paint-over does.
const FINEST_Q: c_int = 4;
const FINEST_QP: u8 = 12;

const LAST: usize = 0;
const GOLDEN: usize = 3;
/// The buffer slot no frame refreshes, for the reference names a frame doesn't use.
const SPARE: c_int = 7;
/// What a frame under rate control records for the blocks it touched: unknown, above any QP a
/// pass can aim for. libaom skips the blocks whose source didn't change at whatever quantizer
/// the frame runs, so the frame's own says nothing about them.
const MOTION_QP: i32 = 52;

pub struct Aom {
    // libaom keeps pointers to the configuration it was given, so neither may move.
    ctx: Box<ffi::aom_codec_ctx_t>,
    cfg: Box<ffi::aom_codec_enc_cfg_t>,
    pts: i64,
    duration: u64,
    announced: bool,
    fresh: bool,
    sharpness: Sharpness,
}

unsafe impl Send for Aom {}

/// The ladder's H.264 QP as libaom's quantizer: `qp * 63 / 51` from QP 16 up, and four
/// quantizers a step from QP 16's 20 down to the last rung's [`FINEST_Q`] at QP 12.
pub(crate) fn quantizer(qp: u8) -> c_int {
    let qp = qp.max(FINEST_QP) as c_int;
    if qp >= 16 {
        ((qp * MAX_Q as c_int + 25) / 51).min(MAX_Q as c_int)
    } else {
        FINEST_Q + (qp - FINEST_QP as c_int) * 4
    }
}

/// Every pass aims at the last rung, whatever rung it was asked for: the pass's budget, not
/// the ladder, decides how fine it gets, so a picture whose whole repaint fits one budget is
/// sharp after one pass instead of three, and a larger one goes as far as each budget allows.
fn finest(r: Refine) -> Refine {
    Refine { qp: FINEST_QP, ..r }
}

/// libaom's quantizer `q` as the lowest H.264 QP whose [`quantizer`] is at least `q`: a pass
/// at any finer QP still sharpens a picture coded at `q`.
pub(crate) fn as_qp(q: c_int) -> i32 {
    (FINEST_QP..=51)
        .find(|&qp| quantizer(qp) >= q)
        .unwrap_or(51) as i32
}

/// An inter frame's references: LAST on slot 0, or for a refinement slot 0 named as GOLDEN
/// with LAST off. Only slot 0 is refreshed, and every name a frame doesn't use points at the
/// spare slot: a name left on slot 0 would be refreshed with it, and libaom's rate control
/// would count every frame as a golden one.
pub(crate) fn references(refine: bool) -> ffi::aom_svc_ref_frame_config_t {
    let mut c: ffi::aom_svc_ref_frame_config_t = unsafe { std::mem::zeroed() };
    c.ref_idx = [SPARE; 7];
    c.ref_idx[LAST] = 0;
    c.refresh[0] = 1;
    if refine {
        c.ref_idx[GOLDEN] = 0;
        c.reference[GOLDEN] = 1;
    } else {
        c.reference[LAST] = 1;
    }
    c
}

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
        ffi::check_layout().map_err(EncodeError)?;
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
                fresh: true,
                sharpness: Sharpness::new(
                    c.width.div_ceil(16) as usize * c.height.div_ceil(16) as usize,
                    51,
                ),
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

    fn set_references(&mut self, refine: bool) -> Result<(), EncodeError> {
        let mut c = references(refine);
        let ctx: *mut ffi::aom_codec_ctx_t = &mut *self.ctx;
        let r = unsafe {
            ffi::aom_codec_control(
                ctx,
                ffi::aome_enc_control_id_AV1E_SET_SVC_REF_FRAME_CONFIG as c_int,
                &mut c as *mut ffi::aom_svc_ref_frame_config_t,
            )
        };
        if r == ffi::aom_codec_err_t_AOM_CODEC_OK {
            Ok(())
        } else {
            Err(self.error("reference config"))
        }
    }

    pub(crate) fn last_quantizer(&mut self) -> Result<c_int, EncodeError> {
        let mut q: c_int = 0;
        let ctx: *mut ffi::aom_codec_ctx_t = &mut *self.ctx;
        let r = unsafe {
            ffi::aom_codec_control(
                ctx,
                ffi::aome_enc_control_id_AOME_GET_LAST_QUANTIZER_64 as c_int,
                &mut q as *mut c_int,
            )
        };
        if r == ffi::aom_codec_err_t_AOM_CODEC_OK {
            Ok(q)
        } else {
            Err(self.error("last quantizer"))
        }
    }

    /// The last frame's luma as a decoder sees it, `g_w` bytes a row.
    #[cfg(test)]
    pub(crate) fn reconstruction(&mut self) -> Result<Vec<u8>, EncodeError> {
        let mut img: ffi::aom_image_t = unsafe { std::mem::zeroed() };
        let ctx: *mut ffi::aom_codec_ctx_t = &mut *self.ctx;
        let r = unsafe {
            ffi::aom_codec_control(
                ctx,
                ffi::aom_com_control_id_AV1_GET_NEW_FRAME_IMAGE as c_int,
                &mut img as *mut ffi::aom_image_t,
            )
        };
        if r != ffi::aom_codec_err_t_AOM_CODEC_OK || img.planes[0].is_null() {
            return Err(self.error("reconstruction"));
        }
        let (w, h) = (self.cfg.g_w as usize, self.cfg.g_h as usize);
        let stride = img.stride[0] as usize;
        let wide = img.fmt & ffi::AOM_IMG_FMT_HIGHBITDEPTH != 0;
        Ok((0..h)
            .flat_map(|r| (0..w).map(move |c| (r, c)))
            .map(|(r, c)| unsafe {
                if wide {
                    (img.planes[0].add(r * stride + 2 * c) as *const u16).read_unaligned() as u8
                } else {
                    *img.planes[0].add(r * stride + c)
                }
            })
            .collect())
    }

    /// Back to the stream's rate control after a refinement's fixed quantizer, which also
    /// turned adaptive quantisation off.
    fn restore(&mut self) -> Result<(), EncodeError> {
        let r = unsafe { ffi::aom_codec_enc_config_set(&mut *self.ctx, &*self.cfg) };
        if r != ffi::aom_codec_err_t_AOM_CODEC_OK {
            return Err(self.error("rate restore after a refinement"));
        }
        self.control(ffi::aome_enc_control_id_AV1E_SET_AQ_MODE, 3)
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
        let mbs = p.width.div_ceil(16) as usize * p.height.div_ceil(16) as usize;
        if opts.static_mbs.as_ref().is_some_and(|m| m.len() < mbs) {
            return Err(EncodeError(format!(
                "macroblock map has fewer than {mbs} entries"
            )));
        }
        let key = opts.force_keyframe || self.fresh;
        let refine = opts.refine.filter(|_| !key).map(finest);
        let fixed = refine.and_then(|r| self.sharpness.pick(r));
        if refine.is_some() && fixed.is_none() {
            self.sharpness.skipped();
            return Ok(Vec::new());
        }
        let mut img: ffi::aom_image_t = unsafe { std::mem::zeroed() };
        let wrapped = unsafe {
            ffi::aom_img_wrap(
                &mut img,
                ffi::aom_img_fmt_AOM_IMG_FMT_I420,
                p.width as c_uint,
                p.height as c_uint,
                1,
                p.y.as_ptr() as *mut u8,
            )
        };
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
        self.pts = opts.pts_ms.max(self.pts + 1);
        let flags = if opts.force_keyframe {
            ffi::AOM_EFLAG_FORCE_KF as ffi::aom_enc_frame_flags_t
        } else {
            0
        };
        if !key {
            self.set_references(fixed.is_some())?;
        }
        if let Some(qp) = fixed {
            let q = quantizer(qp as u8);
            if let Err(e) = self.control(ffi::aome_enc_control_id_AV1E_SET_QUANTIZER_ONE_PASS, q) {
                let _ = self.restore();
                return Err(e);
            }
        }
        let r = unsafe {
            ffi::aom_codec_encode(&mut *self.ctx, &img, self.pts, self.duration as _, flags)
        };
        let out = if r == ffi::aom_codec_err_t_AOM_CODEC_OK {
            self.fresh = false;
            self.collect()
        } else {
            Err(self.error("encode"))
        };
        let q = match out {
            Ok(_) => self.last_quantizer(),
            Err(_) => Ok(0),
        };
        let restored = match fixed {
            Some(_) => self.restore(),
            None => Ok(()),
        };
        let (out, q) = (out?, q?);
        restored?;
        let bytes: usize = out.iter().map(|a| a.data.len()).sum();
        if bytes > 0 {
            match fixed {
                Some(qp) if q == quantizer(qp as u8) => self.sharpness.refined(qp, bytes),
                Some(_) => self.sharpness.refined(as_qp(q), bytes),
                None => {
                    let whole = out.iter().any(|a| a.keyframe);
                    let map = opts.static_mbs.as_deref().filter(|_| !whole);
                    self.sharpness.moved(MOTION_QP, map.map(|m| &m[..mbs]));
                }
            }
        }
        Ok(out)
    }

    fn refinable(&self, r: Refine) -> bool {
        self.sharpness.pick(finest(r)).is_some()
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
