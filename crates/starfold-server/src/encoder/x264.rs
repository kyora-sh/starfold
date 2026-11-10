//! libx264 (GPL-2.0-or-later, the default `x264` feature) tuned for low-latency desktop streaming.

#![allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code
)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/x264.rs"));
}

use super::h264::ParamSets;
use super::sharpness::Sharpness;
use super::{
    AccessUnit, ColorInfo, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig,
    Refine, VideoEncoder,
};
use std::ffi::CString;
use std::ptr;

/// How far a pass may raise the VBV rate. x264 keeps a frame within what the buffer holds
/// less a share of one frame's refill, so a refill far above what the buffer holds now leaves
/// the pass no room at all.
const REFINE_VBV_WIDEN: i32 = 4;
/// A pass's CRF-max over its CRF of 0: VBV may raise no row past CRF's own estimate plus this,
/// so the forced QP holds whenever it is above that (x264 lets the buffer underflow instead).
const REFINE_CRF_MAX: f32 = 0.1;
/// The CRF-max outside passes. x264 can't clear one once set, so every stream carries one
/// this far above its CRF: VBV still raises QP up to 51 unless CRF's own estimate is
/// under 51 - CRF.
const CRF_MAX: f32 = 51.0;

pub struct X264 {
    h: *mut ffi::x264_t,
    param: ffi::x264_param_t,
    pic: ffi::x264_picture_t,
    params: ParamSets,
    pts: i64,
    width: u32,
    height: u32,
    sharpness: Sharpness,
    /// Nothing encoded yet: the next frame is an IDR whether asked for or not.
    fresh: bool,
}

unsafe impl Send for X264 {}

impl X264 {
    pub fn new(cfg: &EncoderConfig) -> Result<Self, EncodeError> {
        unsafe {
            let mut param: ffi::x264_param_t = std::mem::zeroed();
            let preset = CString::new(cfg.preset.as_str()).unwrap();
            let tune = c"zerolatency";
            if ffi::x264_param_default_preset(&mut param, preset.as_ptr(), tune.as_ptr()) < 0 {
                return Err(EncodeError(format!("bad x264 preset {}", cfg.preset)));
            }
            param.i_width = cfg.width as i32;
            param.i_height = cfg.height as i32;
            param.i_csp = ffi::X264_CSP_I420 as i32;
            param.i_fps_num = cfg.fps;
            param.i_fps_den = 1;
            param.i_timebase_num = 1;
            param.i_timebase_den = 1000;
            // VFR input costs a frame of encoder delay (duration estimation).
            param.b_vfr_input = 0;
            param.i_threads = cfg.threads as i32;
            param.b_sliced_threads = (cfg.threads > 1) as i32;
            param.i_slice_count = cfg.threads as i32;
            param.i_keyint_max = ffi::X264_KEYINT_MAX_INFINITE as i32;
            param.i_keyint_min = 1;
            param.i_scenecut_threshold = 0;
            param.b_intra_refresh = 0;
            param.i_bframe = 0;
            param.b_annexb = 1;
            param.b_repeat_headers = 1;
            param.b_aud = 0;
            param.i_log_level = if std::env::var("VOXL_X264_INFO").as_deref() == Ok("debug") {
                // Per-frame QP, macroblock types and size.
                ffi::X264_LOG_DEBUG
            } else if std::env::var("VOXL_X264_INFO").is_ok() {
                ffi::X264_LOG_INFO
            } else {
                ffi::X264_LOG_WARNING
            } as i32;
            param.rc.i_rc_method = ffi::X264_RC_CRF as i32;
            param.rc.f_rf_constant = cfg.rate.quality;
            param.rc.f_rf_constant_max = CRF_MAX;
            param.rc.i_lookahead = 0;
            // ultrafast's diamond search only follows motion of a few pixels, so a dragged
            // window or a fast scroll would be coded as new pixels every frame. UMH finds
            // those shifts (a third of the bits for a window drag) for about the same CPU.
            param.analyse.i_me_method = ffi::X264_ME_UMH as i32;
            param.analyse.i_me_range = 64;
            // Per-macroblock "unchanged" hints from X damage let x264 skip analysis.
            param.analyse.b_mb_info = (std::env::var("VOXL_NO_MBINFO").is_err()) as i32;
            param.i_sync_lookahead = 0;
            param.rc.b_mb_tree = 0;
            if cfg.rate.max_kbps > 0 {
                param.rc.i_vbv_max_bitrate = cfg.rate.max_kbps as i32;
                param.rc.i_vbv_buffer_size = cfg.rate.buf_kbit.max(1) as i32;
            }
            let c = ColorInfo::SRGB_BT601_LIMITED;
            param.vui.i_colorprim = c.primaries as i32;
            param.vui.i_transfer = c.transfer as i32;
            param.vui.i_colmatrix = c.matrix as i32;
            param.vui.b_fullrange = c.full_range as i32;
            let profile = c"high";
            ffi::x264_param_apply_profile(&mut param, profile.as_ptr());

            let h = ffi::x264_encoder_open(&mut param);
            if h.is_null() {
                return Err(EncodeError("x264_encoder_open failed".into()));
            }
            let mut pic: ffi::x264_picture_t = std::mem::zeroed();
            ffi::x264_picture_init(&mut pic);
            pic.img.i_csp = ffi::X264_CSP_I420 as i32;
            pic.img.i_plane = 3;
            Ok(X264 {
                h,
                param,
                pic,
                params: ParamSets::default(),
                pts: 0,
                width: cfg.width,
                height: cfg.height,
                sharpness: Sharpness::new(
                    cfg.width.div_ceil(16) as usize * cfg.height.div_ceil(16) as usize,
                    (cfg.best_quality.round() as i32).clamp(12, 51),
                ),
                fresh: true,
            })
        }
    }

    /// Rate control as it stands, but holding a pass's forced QP: CRF 0 with REFINE_CRF_MAX,
    /// so VBV's row control can't raise it, and room for `max_bytes` (two in the buffer, and at
    /// least two frames' refill so x264 never lowers rows below it either), refilled at up to
    /// REFINE_VBV_WIDEN times the rate, so the motion after a pass doesn't start from an
    /// empty buffer. A whole raw picture stands in for a missing budget.
    fn widen(&mut self, max_bytes: Option<usize>, mbs: usize) -> Result<(), EncodeError> {
        let mut p = self.param;
        p.rc.f_rf_constant = 0.0;
        p.rc.f_rf_constant_max = REFINE_CRF_MAX;
        if p.rc.i_vbv_max_bitrate > 0 {
            let b = max_bytes.unwrap_or(mbs * 384);
            let frame_kbit = (b * 8).div_ceil(1000) as i32;
            let rate = p.rc.i_vbv_max_bitrate;
            p.rc.i_vbv_max_bitrate =
                (frame_kbit * p.i_fps_num as i32).clamp(rate, rate * REFINE_VBV_WIDEN);
            let refill = p.rc.i_vbv_max_bitrate / p.i_fps_num.max(1) as i32;
            p.rc.i_vbv_buffer_size = p.rc.i_vbv_buffer_size.max(frame_kbit * 2).max(refill * 2);
        }
        Self::reconfig(self.h, &mut p)
    }

    fn reconfig(h: *mut ffi::x264_t, p: &mut ffi::x264_param_t) -> Result<(), EncodeError> {
        match unsafe { ffi::x264_encoder_reconfig(h, p) } {
            0 => Ok(()),
            r => Err(EncodeError(format!("x264_encoder_reconfig failed ({r})"))),
        }
    }

    fn unit(&mut self, size: i32, nals: *mut ffi::x264_nal_t, pts: i64) -> Option<AccessUnit> {
        if size <= 0 || nals.is_null() {
            return None;
        }
        // NAL payloads are contiguous in memory.
        let data = unsafe { std::slice::from_raw_parts((*nals).p_payload, size as usize) };
        self.params.unit(
            data.to_vec(),
            pts,
            (self.width, self.height),
            ColorInfo::SRGB_BT601_LIMITED,
        )
    }
}

impl VideoEncoder for X264 {
    fn set_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError> {
        self.param.rc.f_rf_constant = rate.quality;
        if rate.max_kbps > 0 {
            self.param.rc.i_vbv_max_bitrate = rate.max_kbps as i32;
            self.param.rc.i_vbv_buffer_size = rate.buf_kbit.max(1) as i32;
        }
        Self::reconfig(self.h, &mut self.param)
    }

    fn encode(
        &mut self,
        p: &FrameView<'_>,
        opts: EncodeOptions<'_>,
    ) -> Result<Vec<AccessUnit>, EncodeError> {
        if (p.width, p.height) != (self.width, self.height) {
            return Err(EncodeError(format!(
                "x264 opened at {}x{}, got {}x{}",
                self.width, self.height, p.width, p.height
            )));
        }
        p.check()?;
        let mbs = p.width.div_ceil(16) as usize * p.height.div_ceil(16) as usize;
        if opts.static_mbs.as_ref().is_some_and(|m| m.len() < mbs) {
            return Err(EncodeError(format!(
                "macroblock map has fewer than {mbs} entries"
            )));
        }
        let idr = opts.force_keyframe || self.fresh;
        let refine = opts.refine.filter(|_| !idr);
        let fixed = refine.and_then(|r| self.sharpness.pick(r));
        if refine.is_some() && fixed.is_none() {
            self.sharpness.skipped();
            return Ok(Vec::new());
        }
        if let Some(r) = refine {
            // Nothing is lost, the picture just stays as it is.
            if let Err(e) = self.widen(r.max_bytes, mbs) {
                log::warn!("no more refinement on this stream: {e}");
                self.sharpness.off = true;
                return Ok(Vec::new());
            }
        }
        unsafe {
            self.pic.img.plane[0] = p.y.as_ptr() as *mut u8;
            self.pic.img.plane[1] = p.u.as_ptr() as *mut u8;
            self.pic.img.plane[2] = p.v.as_ptr() as *mut u8;
            self.pic.img.i_stride[0] = p.stride_y as i32;
            self.pic.img.i_stride[1] = p.stride_uv as i32;
            self.pic.img.i_stride[2] = p.stride_uv as i32;
            self.pic.i_type = if opts.force_keyframe {
                ffi::X264_TYPE_IDR as i32
            } else {
                ffi::X264_TYPE_AUTO as i32
            };
            self.pic.i_qpplus1 = fixed.map_or(0, |q| q + 1);
            // X264_MBINFO_CONSTANT (1) on unchanged macroblocks lets x264 skip their
            // analysis; x264 reads the map during this call only.
            let mut static_mbs = opts.static_mbs.map(|m| &mut m[..mbs]);
            self.pic.prop.mb_info = static_mbs
                .as_deref_mut()
                .map_or(ptr::null_mut(), |m| m.as_mut_ptr());
            self.pic.prop.mb_info_free = None;
            // Strictly increasing pts even if the wall clock stalls.
            self.pts = opts.pts_ms.max(self.pts + 1);
            self.pic.i_pts = self.pts;
            let mut nals: *mut ffi::x264_nal_t = ptr::null_mut();
            let mut n: i32 = 0;
            let mut pic_out: ffi::x264_picture_t = std::mem::zeroed();
            let size =
                ffi::x264_encoder_encode(self.h, &mut nals, &mut n, &mut self.pic, &mut pic_out);
            self.pic.prop.mb_info = ptr::null_mut();
            if fixed.is_some() {
                Self::reconfig(self.h, &mut self.param)?;
            }
            if size < 0 {
                return Err(EncodeError("x264_encoder_encode failed".into()));
            }
            self.fresh = false;
            if size > 0 {
                match fixed {
                    // Proof that no row went above the pass's QP: x264's average QP over CRF's
                    // estimate. Without it the pass counts for nothing, and the picture stays
                    // as coarse as it was for the next one.
                    Some(qp)
                        if self.param.rc.i_vbv_max_bitrate == 0
                            || pic_out.prop.f_crf_avg > REFINE_CRF_MAX as f64 =>
                    {
                        self.sharpness.refined(qp, size as usize)
                    }
                    Some(_) => {}
                    None => {
                        let map = static_mbs.as_deref().filter(|_| pic_out.b_keyframe == 0);
                        self.sharpness.moved(pic_out.i_qpplus1 - 1, map);
                    }
                }
            }
            if std::env::var("VOXL_DEBUG_MARKER").is_ok() {
                log::info!(
                    "x264 out {} bytes, delayed {}, pts in {} out {}",
                    size,
                    ffi::x264_encoder_delayed_frames(self.h),
                    self.pts,
                    pic_out.i_pts
                );
            }
            Ok(self.unit(size, nals, pic_out.i_pts).into_iter().collect())
        }
    }

    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        let mut out = Vec::new();
        while unsafe { ffi::x264_encoder_delayed_frames(self.h) } > 0 {
            let mut nals: *mut ffi::x264_nal_t = ptr::null_mut();
            let mut n: i32 = 0;
            let mut pic_out: ffi::x264_picture_t = unsafe { std::mem::zeroed() };
            let size = unsafe {
                ffi::x264_encoder_encode(self.h, &mut nals, &mut n, ptr::null_mut(), &mut pic_out)
            };
            if size < 0 {
                return Err(EncodeError("x264_encoder_encode failed".into()));
            }
            out.extend(self.unit(size, nals, pic_out.i_pts));
        }
        Ok(out)
    }

    fn refinable(&self, r: Refine) -> bool {
        self.sharpness.pick(r).is_some()
    }
}

impl Drop for X264 {
    fn drop(&mut self) {
        unsafe { ffi::x264_encoder_close(self.h) }
    }
}
