//! OpenH264 (BSD-2-Clause, compiled from source by openh264-sys2) through its C vtable:
//! constrained Baseline, screen-content mode, bitrate rate control, IDR on request only.

use super::h264::ParamSets;
use super::{
    AccessUnit, ColorInfo, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig,
    VideoEncoder,
};
use openh264_sys2 as sys;
use std::os::raw::{c_int, c_void};
use std::ptr;

/// Level 5.2's frame size, OpenH264's limit for one picture.
pub const MAX_MBS: usize = 36864;
/// OpenH264 clamps the frame rate it plans rate control for to this.
const MAX_FPS: u32 = 60;
/// What an uncapped rate (`max_kbps` 0) becomes: level 5.2's bitrate.
const UNCAPPED_BPS: c_int = 240_000_000;

pub struct OpenH264 {
    enc: *mut sys::ISVCEncoder,
    info: Box<sys::SFrameBSInfo>,
    params: ParamSets,
    bps: c_int,
    pts: i64,
    width: u32,
    height: u32,
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

fn bps(max_kbps: u32) -> c_int {
    if max_kbps == 0 {
        UNCAPPED_BPS
    } else {
        (max_kbps as u64 * 1000).min(UNCAPPED_BPS as u64) as c_int
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
            bps: bps(cfg.rate.max_kbps),
            pts: -1,
            width: cfg.width,
            height: cfg.height,
        };
        let mut level: c_int = sys::WELS_LOG_ERROR;
        me.option(sys::ENCODER_OPTION_TRACE_LEVEL, &mut level)?;

        let mut p = sys::SEncParamExt::default();
        check("GetDefaultParams", unsafe {
            me.vt().GetDefaultParams.unwrap()(me.enc, &mut p)
        })?;
        let fps = cfg.fps.clamp(1, MAX_FPS) as f32;
        let threads = cfg.threads.clamp(1, 4) as u16;
        p.iUsageType = sys::SCREEN_CONTENT_REAL_TIME;
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
        // Screen-content mode always detects scene changes, and without long-term
        // references each one is an IDR. The WebSocket never loses a frame.
        p.bEnableLongTermReference = true;
        p.iMultipleThreadIdc = threads;
        p.bUseLoadBalancing = false;
        p.iLoopFilterDisableIdc = 0;
        p.bEnableDenoise = false;
        p.bEnableBackgroundDetection = false;
        p.bEnableAdaptiveQuant = false;
        p.bEnableFrameCroppingFlag = true;
        p.bEnableSceneChangeDetect = false;
        p.bIsLosslessLink = true;
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
        let b = bps(rate.max_kbps);
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
        let (w, h) = (f.width as usize, f.height as usize);
        let ch = h.div_ceil(2);
        if f.y.len() < (h - 1) * f.stride_y + w
            || f.u.len() < (ch - 1) * f.stride_uv + w.div_ceil(2)
            || f.v.len() < (ch - 1) * f.stride_uv + w.div_ceil(2)
        {
            return Err(EncodeError("I420 planes shorter than the picture".into()));
        }
        self.pts = opts.pts_ms.max(self.pts + 1);
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
        check("EncodeFrame", unsafe {
            self.vt().EncodeFrame.unwrap()(self.enc, &src, &mut *self.info)
        })?;
        let info = &*self.info;
        if info.eFrameType == sys::videoFrameTypeSkip || info.iFrameSizeInBytes <= 0 {
            return Ok(Vec::new());
        }
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
