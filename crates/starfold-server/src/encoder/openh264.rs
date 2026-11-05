//! OpenH264 (BSD-2-Clause, compiled from source by openh264-sys2) through its C vtable:
//! constrained Baseline, bitrate rate control, IDR on request only.

use super::h264::ParamSets;
use super::{
    AccessUnit, ColorInfo, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig,
    Refine, VideoEncoder,
};
use openh264_sys2 as sys;
use std::os::raw::{c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

/// Level 5.2's frame size, OpenH264's limit for one picture.
pub const MAX_MBS: usize = 36864;
/// OpenH264 clamps the frame rate it plans rate control for to this, so above it the
/// bitrate is scaled down to keep the per-second budget.
const MAX_FPS: u32 = 60;
/// What an uncapped rate (`max_kbps` 0) becomes: level 5.2's bitrate.
const UNCAPPED_BPS: c_int = 240_000_000;
/// What one QP step costs per macroblock on dense text, until a frame says otherwise.
const REFINE_COST: f64 = 5.0;
const REFINE_COST_RANGE: (f64, f64) = (1.0, 15.0);
/// Frames sent at a fixed QP so far, and their bytes.
pub static REFINED_FRAMES: AtomicU64 = AtomicU64::new(0);
pub static REFINED_BYTES: AtomicU64 = AtomicU64::new(0);

/// How coarse the picture still is, and what sharpening it costs: over the QPs a refinement
/// crosses, bytes grow about linearly with the steps.
pub(super) struct Sharpness {
    qp: i32,
    cost: f64,
}

impl Sharpness {
    pub(super) fn new(qp: i32) -> Self {
        Sharpness {
            qp,
            cost: REFINE_COST,
        }
    }

    pub(super) fn moved(&mut self, qp: i32) {
        self.qp = self.qp.max(qp);
    }

    fn expected(&self, mbs: usize, qp: i32) -> f64 {
        mbs as f64 * (self.qp - qp) as f64 * self.cost
    }

    /// The lowest QP down to the pass's whose frame is expected to fit.
    pub(super) fn pick(&self, r: Refine) -> Option<i32> {
        let want = r.qp as i32;
        match r.max_bytes {
            None => (want < self.qp).then_some(want),
            Some(max) => (want..self.qp).find(|&q| self.expected(r.mbs, q) <= max as f64),
        }
    }

    /// A frame that came out larger than expected counts at once, a smaller one by half:
    /// the next pass may be on a thin link.
    pub(super) fn refined(&mut self, qp: i32, mbs: usize, bytes: usize) {
        let steps = self.qp - qp;
        if mbs > 0 && steps > 0 {
            let seen = bytes as f64 / (mbs as f64 * steps as f64);
            let cost = if seen > self.cost {
                seen
            } else {
                (self.cost + seen) / 2.0
            };
            self.cost = cost.clamp(REFINE_COST_RANGE.0, REFINE_COST_RANGE.1);
        }
        self.qp = qp;
    }

    /// A pass that didn't fit: an estimate left high by an odd frame must not stop every later one.
    pub(super) fn skipped(&mut self) {
        self.cost = (self.cost * 0.75).max(REFINE_COST.min(self.cost));
    }
}

pub struct OpenH264 {
    enc: *mut sys::ISVCEncoder,
    info: Box<sys::SFrameBSInfo>,
    params: ParamSets,
    bps: c_int,
    /// Frames OpenH264 budgets per second over frames that arrive per second.
    rate_scale: f64,
    pts: i64,
    width: u32,
    height: u32,
    sharpness: Sharpness,
}

// The encoder is only ever used from one thread at a time.
unsafe impl Send for OpenH264 {}

fn check(what: &str, r: c_int) -> Result<(), EncodeError> {
    if r == 0 {
        Ok(())
    } else {
        Err(EncodeError(format!("OpenH264 {what} failed ({r})")))
    }
}

fn bps(max_kbps: u32, scale: f64) -> c_int {
    if max_kbps == 0 {
        UNCAPPED_BPS
    } else {
        (max_kbps as f64 * 1000.0 * scale).clamp(1000.0, UNCAPPED_BPS as f64) as c_int
    }
}

impl OpenH264 {
    pub fn new(cfg: &EncoderConfig) -> Result<Self, EncodeError> {
        let mut enc = ptr::null_mut();
        let r = unsafe { sys::source::APILoader::WelsCreateSVCEncoder(&mut enc) };
        if r != 0 || enc.is_null() {
            return Err(EncodeError(format!("WelsCreateSVCEncoder failed ({r})")));
        }
        let rate_scale = (MAX_FPS as f64 / cfg.fps.max(1) as f64).min(1.0);
        let mut me = OpenH264 {
            enc,
            info: Box::default(),
            params: ParamSets::default(),
            bps: bps(cfg.rate.max_kbps, rate_scale),
            rate_scale,
            pts: -1,
            width: cfg.width,
            height: cfg.height,
            sharpness: Sharpness::new((cfg.best_quality.round() as c_int).clamp(12, 51)),
        };
        let mut level: c_int = sys::WELS_LOG_ERROR;
        me.option(sys::ENCODER_OPTION_TRACE_LEVEL, &mut level)?;

        let mut p = sys::SEncParamExt::default();
        check("GetDefaultParams", unsafe {
            me.vt().GetDefaultParams.unwrap()(me.enc, &mut p)
        })?;
        let fps = cfg.fps.clamp(1, MAX_FPS) as f32;
        let threads = cfg.threads.clamp(1, 4) as u16;
        // Not SCREEN_CONTENT_REAL_TIME: that mode forces scene-change detection on (an
        // unrequested IDR per detected change, unless long-term references take them,
        // which cost ~15x the CPU on scrolling).
        p.iUsageType = sys::CAMERA_VIDEO_REAL_TIME;
        p.iPicWidth = cfg.width as c_int;
        p.iPicHeight = cfg.height as c_int;
        p.iRCMode = sys::RC_BITRATE_MODE;
        p.iTargetBitrate = me.bps;
        p.iMaxBitrate = me.bps;
        p.fMaxFrameRate = fps;
        p.iTemporalLayerNum = 1;
        p.iSpatialLayerNum = 1;
        p.iComplexityMode = sys::LOW_COMPLEXITY;
        p.uiIntraPeriod = 0;
        p.eSpsPpsIdStrategy = sys::CONSTANT_ID;
        p.bPrefixNalAddingCtrl = false;
        p.bEnableSSEI = false;
        p.bSimulcastAVC = false;
        p.iPaddingFlag = 0;
        p.iEntropyCodingModeFlag = 0;
        p.bEnableFrameSkip = false;
        p.iMinQp = (cfg.best_quality.round() as c_int).clamp(12, 51);
        p.iMaxQp = 51;
        p.bEnableLongTermReference = false;
        p.iMultipleThreadIdc = threads;
        p.bUseLoadBalancing = false;
        p.iLoopFilterDisableIdc = 0;
        p.bEnableDenoise = false;
        p.bEnableBackgroundDetection = false;
        p.bEnableAdaptiveQuant = false;
        p.bEnableFrameCroppingFlag = true;
        p.bEnableSceneChangeDetect = false;
        p.bIsLosslessLink = false;
        let l = &mut p.sSpatialLayers[0];
        l.iVideoWidth = cfg.width as c_int;
        l.iVideoHeight = cfg.height as c_int;
        l.fFrameRate = fps;
        l.iSpatialBitrate = me.bps;
        l.iMaxSpatialBitrate = me.bps;
        l.uiProfileIdc = sys::PRO_BASELINE;
        l.uiLevelIdc = sys::LEVEL_UNKNOWN;
        l.sSliceArgument.uiSliceMode = if threads > 1 {
            sys::SM_FIXEDSLCNUM_SLICE
        } else {
            sys::SM_SINGLE_SLICE
        };
        l.sSliceArgument.uiSliceNum = threads as u32;
        let c = ColorInfo::SRGB_BT601_LIMITED;
        l.bVideoSignalTypePresent = true;
        l.uiVideoFormat = 5;
        l.bFullRange = c.full_range;
        l.bColorDescriptionPresent = true;
        l.uiColorPrimaries = c.primaries;
        l.uiTransferCharacteristics = c.transfer;
        l.uiColorMatrix = c.matrix;
        let r = unsafe { me.vt().InitializeExt.unwrap()(me.enc, &p) };
        if r != 0 {
            return Err(EncodeError(format!(
                "OpenH264 rejected {}x{} @ {fps} fps, {threads} thread(s) ({r})",
                cfg.width, cfg.height
            )));
        }
        let mut fmt: c_int = sys::videoFormatI420;
        me.option(sys::ENCODER_OPTION_DATAFORMAT, &mut fmt)?;
        Ok(me)
    }

    fn vt(&self) -> &sys::ISVCEncoderVtbl {
        unsafe { &**self.enc }
    }

    fn option<T>(&mut self, id: sys::ENCODER_OPTION, v: &mut T) -> Result<(), EncodeError> {
        let r = unsafe { self.vt().SetOption.unwrap()(self.enc, id, v as *mut T as *mut c_void) };
        check("SetOption", r)
    }

    fn get_option<T: Default>(&self, id: sys::ENCODER_OPTION) -> Result<T, EncodeError> {
        let mut v = T::default();
        let r =
            unsafe { self.vt().GetOption.unwrap()(self.enc, id, &mut v as *mut T as *mut c_void) };
        check("GetOption", r)?;
        Ok(v)
    }

    /// Rate control off for the next frame, at `qp`. The QP range can't change on a live
    /// encoder (only a reset, an IDR, applies it), this can, and rate control doesn't see
    /// the frame: no drift afterwards. Upstream discourages switching mid-stream.
    fn fix_qp(&mut self, qp: c_int) -> Result<(), EncodeError> {
        let mut p: sys::SEncParamExt = self.get_option(sys::ENCODER_OPTION_SVC_ENCODE_PARAM_EXT)?;
        p.sSpatialLayers[0].iDLayerQp = qp;
        self.option(sys::ENCODER_OPTION_SVC_ENCODE_PARAM_EXT, &mut p)?;
        self.rate_control(sys::RC_OFF_MODE)
    }

    fn rate_control(&mut self, mode: sys::RC_MODES) -> Result<(), EncodeError> {
        let mut m = mode as c_int;
        self.option(sys::ENCODER_OPTION_RC_MODE, &mut m)
    }

    fn set_bps(&mut self, bps: c_int) -> Result<(), EncodeError> {
        let mut info = sys::SBitrateInfo {
            iLayer: sys::SPATIAL_LAYER_0,
            iBitrate: bps,
        };
        // The target may not pass the cap, so raise the cap first and lower it last.
        let order = if bps > self.bps {
            [sys::ENCODER_OPTION_MAX_BITRATE, sys::ENCODER_OPTION_BITRATE]
        } else {
            [sys::ENCODER_OPTION_BITRATE, sys::ENCODER_OPTION_MAX_BITRATE]
        };
        for id in order {
            self.option(id, &mut info)?;
        }
        self.bps = bps;
        Ok(())
    }
}

impl VideoEncoder for OpenH264 {
    fn set_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError> {
        let b = bps(rate.max_kbps, self.rate_scale);
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
                "OpenH264 opened at {}x{}, got {}x{}",
                self.width, self.height, f.width, f.height
            )));
        }
        f.check()?;
        self.pts = opts.pts_ms.max(self.pts + 1);
        let refine = opts.refine.filter(|_| !opts.force_keyframe);
        let fixed = refine.and_then(|r| self.sharpness.pick(r).map(|q| (q, r.mbs)));
        if refine.is_some() && fixed.is_none() {
            self.sharpness.skipped();
            return Ok(Vec::new());
        }
        let src = sys::SSourcePicture {
            iColorFormat: sys::videoFormatI420,
            iStride: [
                f.stride_y as c_int,
                f.stride_uv as c_int,
                f.stride_uv as c_int,
                0,
            ],
            pData: [
                f.y.as_ptr() as *mut u8,
                f.u.as_ptr() as *mut u8,
                f.v.as_ptr() as *mut u8,
                ptr::null_mut(),
            ],
            iPicWidth: f.width as c_int,
            iPicHeight: f.height as c_int,
            uiTimeStamp: self.pts,
            ..Default::default()
        };
        if opts.force_keyframe {
            check("ForceIntraFrame", unsafe {
                self.vt().ForceIntraFrame.unwrap()(self.enc, true)
            })?;
        }
        if let Some((qp, _)) = fixed {
            self.fix_qp(qp)?;
        }
        let encoded = unsafe { self.vt().EncodeFrame.unwrap()(self.enc, &src, &mut *self.info) };
        if fixed.is_some() {
            self.rate_control(sys::RC_BITRATE_MODE)?;
        }
        check("EncodeFrame", encoded)?;
        let info = &*self.info;
        if info.eFrameType == sys::videoFrameTypeSkip || info.iFrameSizeInBytes <= 0 {
            return Ok(Vec::new());
        }
        match fixed {
            Some((qp, mbs)) => {
                let bytes = info.iFrameSizeInBytes as usize;
                self.sharpness.refined(qp, mbs, bytes);
                REFINED_FRAMES.fetch_add(1, Ordering::Relaxed);
                REFINED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
            }
            None => {
                let s: sys::SEncoderStatistics =
                    self.get_option(sys::ENCODER_OPTION_GET_STATISTICS)?;
                self.sharpness.moved(s.uiAverageFrameQP as c_int);
            }
        }
        let info = &*self.info;
        let mut data = Vec::with_capacity(info.iFrameSizeInBytes as usize);
        for l in &info.sLayerInfo[..info.iLayerNum.clamp(0, 128) as usize] {
            let len: usize = (0..l.iNalCount.max(0) as usize)
                .map(|i| unsafe { *l.pNalLengthInByte.add(i) }.max(0) as usize)
                .sum();
            if len > 0 {
                data.extend_from_slice(unsafe { std::slice::from_raw_parts(l.pBsBuf, len) });
            }
        }
        Ok(self
            .params
            .unit(
                data,
                self.pts,
                (self.width, self.height),
                ColorInfo::SRGB_BT601_LIMITED,
            )
            .into_iter()
            .collect())
    }

    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        Ok(Vec::new())
    }
}

impl Drop for OpenH264 {
    fn drop(&mut self) {
        unsafe {
            self.vt().Uninitialize.unwrap()(self.enc);
            sys::source::APILoader::WelsDestroySVCEncoder(self.enc);
        }
    }
}
