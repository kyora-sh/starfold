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

/// OpenH264 clamps the frame rate it plans rate control for to this, so above it the
/// bitrate is scaled down to keep the per-second budget.
const MAX_FPS: u32 = 60;
/// What an uncapped rate (`max_kbps` 0) becomes: level 5.2's bitrate.
const UNCAPPED_BPS: c_int = 240_000_000;
/// What one QP step costs per macroblock, until a frame says otherwise: desktop text is
/// 1.3 to 2, a screen packed with it 5 to 15. Starting high costs a thin link one soft pass,
/// starting low a frame that blocks input.
const REFINE_COST: f64 = 5.0;
const REFINE_COST_RANGE: (f64, f64) = (1.0, 15.0);

/// How coarse each macroblock still is, and what sharpening them costs. A QP step costs the
/// same below 28 and less and less above it, down to almost nothing at 51 (1080p text,
/// measured). A fixed QP reaches every macroblock whatever changed, so all of them count.
pub(super) struct Sharpness {
    pub(super) qp: Vec<u8>,
    cost: f64,
    pub(super) off: bool,
}

impl Sharpness {
    pub(super) fn new(mbs: usize, qp: i32) -> Self {
        Sharpness {
            qp: vec![qp as u8; mbs],
            cost: REFINE_COST,
            off: false,
        }
    }

    /// A frame went out under rate control at `qp`, touching what `static_mbs` doesn't mark.
    pub(super) fn moved(&mut self, qp: i32, static_mbs: Option<&[u8]>) {
        match static_mbs.filter(|m| m.len() == self.qp.len()) {
            Some(m) => {
                for (q, _) in self.qp.iter_mut().zip(m).filter(|(_, &s)| s == 0) {
                    *q = qp as u8;
                }
            }
            None => self.qp.fill(qp as u8),
        }
    }

    /// Macroblock-steps from where the picture stands to `qp`, in units of `cost`.
    fn steps(&self, qp: i32) -> f64 {
        let mut count = [0usize; 52];
        for &q in &self.qp {
            count[q.min(51) as usize] += 1;
        }
        let (mut total, mut below) = (0.0, 0);
        for q in (qp.max(0)..51).rev() {
            below += count[q as usize + 1];
            total += below as f64 * ((52 - q) as f64 / 24.0).min(1.0);
        }
        total
    }

    /// The lowest QP down to the pass's whose frame is expected to fit.
    pub(super) fn pick(&self, r: Refine) -> Option<i32> {
        let want = r.qp as i32;
        let top = self.qp.iter().copied().max().unwrap_or(0) as i32;
        if self.off {
            return None;
        }
        match r.max_bytes {
            None => (want < top).then_some(want),
            Some(max) => (want..top).find(|&q| self.steps(q) * self.cost <= max as f64),
        }
    }

    /// A frame larger than expected replaces the estimate at once, a smaller one by half:
    /// passes over the same picture vary several times over, and only too large a frame
    /// hurts. One with next to nothing to sharpen says nothing about the next picture.
    pub(super) fn refined(&mut self, qp: i32, bytes: usize) {
        let steps = self.steps(qp);
        if steps > 0.0 && bytes >= 1024 {
            let seen = bytes as f64 / steps;
            let cost = if seen > self.cost {
                seen
            } else {
                (self.cost + seen) / 2.0
            };
            self.cost = cost.clamp(REFINE_COST_RANGE.0, REFINE_COST_RANGE.1);
        }
        for q in &mut self.qp {
            *q = (*q).min(qp as u8);
        }
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
    /// The frame rate rate control budgets each frame for.
    fps: u32,
    pts: i64,
    width: u32,
    height: u32,
    sharpness: Sharpness,
    /// Makes the library itself refuse the switch to a fixed QP.
    #[cfg(test)]
    pub(crate) break_switch: bool,
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

/// The bitrate to give OpenH264 so that frames arriving at `fps` share `max_kbps`: it
/// budgets per second over at most MAX_FPS frames.
fn bps(max_kbps: u32, fps: u32) -> c_int {
    if max_kbps == 0 {
        UNCAPPED_BPS
    } else {
        let scale = (MAX_FPS as f64 / fps.max(1) as f64).min(1.0);
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
        let mut me = OpenH264 {
            enc,
            info: Box::default(),
            params: ParamSets::default(),
            bps: bps(cfg.rate.max_kbps, cfg.fps),
            fps: cfg.fps.clamp(1, MAX_FPS),
            pts: -1,
            width: cfg.width,
            height: cfg.height,
            sharpness: Sharpness::new(
                cfg.width.div_ceil(16) as usize * cfg.height.div_ceil(16) as usize,
                (cfg.best_quality.round() as c_int).clamp(12, 51),
            ),
            #[cfg(test)]
            break_switch: false,
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
        // Only the layer's rate follows set_rate; this checks it against the overall one.
        p.iTargetBitrate = self.bps;
        p.iMaxBitrate = self.bps;
        #[cfg(test)]
        if self.break_switch {
            p.iSpatialLayerNum = 0;
        }
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
        let fps = rate.fps.clamp(1, MAX_FPS);
        if fps != self.fps {
            // Frames budget the bitrate over this rate, not over the rate the stream opened at.
            let mut f = fps as f32;
            self.option(sys::ENCODER_OPTION_FRAME_RATE, &mut f)?;
            self.fps = fps;
        }
        let b = bps(rate.max_kbps, rate.fps);
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
        // A new encoder's first frame is an IDR whether asked for or not.
        let idr = opts.force_keyframe || self.pts < 0;
        self.pts = opts.pts_ms.max(self.pts + 1);
        let refine = opts.refine.filter(|_| !idr);
        let fixed = refine.and_then(|r| self.sharpness.pick(r));
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
        if let Some(qp) = fixed {
            // Nothing is lost, the picture just stays as it is: not worth the keyframe a
            // failed encode costs, again after every burst.
            if let Err(e) = self.fix_qp(qp) {
                log::warn!("no more refinement on this stream: {e}");
                self.sharpness.off = true;
                return Ok(Vec::new());
            }
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
            Some(qp) => self.sharpness.refined(qp, info.iFrameSizeInBytes as usize),
            None => {
                let whole = info.eFrameType == sys::videoFrameTypeIDR;
                let s: sys::SEncoderStatistics =
                    self.get_option(sys::ENCODER_OPTION_GET_STATISTICS)?;
                let map = opts.static_mbs.as_deref().filter(|_| !whole);
                self.sharpness.moved(s.uiAverageFrameQP as c_int, map);
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

    fn refinable(&self, r: Refine) -> bool {
        self.sharpness.pick(r).is_some()
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
