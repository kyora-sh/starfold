//! x265 (GPL-2.0, measurement only, only with the `x265` feature): HEVC with a preset and
//! tune zerolatency, CBR through the VBV, repeated headers on every keyframe.

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code,
    unused_imports,
    clippy::all
)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/x265.rs"));
}

use super::hevc::Units;
use super::{
    AccessUnit, Codec, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig,
    VideoEncoder,
};
use std::ffi::CString;
use std::ptr;

/// Level 5.1 main tier's ceiling: level and tier stay put, so a rate change never needs new
/// ones (x265 refuses a rate reconfigure that would change either).
const UNCAPPED_KBPS: u32 = 40_000;
const PRESETS: &[&str] = &["ultrafast", "superfast", "veryfast"];

pub struct X265 {
    api: &'static ffi::x265_api,
    param: *mut ffi::x265_param,
    enc: *mut ffi::x265_encoder,
    pic: *mut ffi::x265_picture,
    width: u32,
    height: u32,
    pts: i64,
    units: Units,
}

unsafe impl Send for X265 {}

fn kbps(rate: RateConfig) -> u32 {
    if rate.max_kbps == 0 {
        UNCAPPED_KBPS
    } else {
        rate.max_kbps.min(UNCAPPED_KBPS)
    }
}

impl X265 {
    pub fn new(c: &EncoderConfig) -> Result<Self, EncodeError> {
        if c.codec != Codec::Hevc {
            return Err(EncodeError(format!("x265 can't encode {:?}", c.codec)));
        }
        let preset = match c.preset.as_str() {
            "" => PRESETS[0],
            p => *PRESETS.iter().find(|&&name| name == p).ok_or_else(|| {
                EncodeError(format!("x265 preset {p:?} is not one of {PRESETS:?}"))
            })?,
        };
        let api = unsafe { ffi::x265_api_get(8).as_ref() }
            .ok_or_else(|| EncodeError("x265 has no 8-bit API".into()))?;
        let param = unsafe { (api.param_alloc.unwrap())() };
        if param.is_null() {
            return Err(EncodeError("x265 param_alloc failed".into()));
        }
        let mut x = X265 {
            api,
            param,
            enc: ptr::null_mut(),
            pic: ptr::null_mut(),
            width: c.width,
            height: c.height,
            pts: -1,
            units: Units::new(c.width, c.height),
        };
        let (p, t) = (
            CString::new(preset).unwrap(),
            CString::new("zerolatency").unwrap(),
        );
        if unsafe { (api.param_default_preset.unwrap())(param, p.as_ptr(), t.as_ptr()) } != 0 {
            return Err(EncodeError(format!("x265 preset {preset} refused")));
        }
        for (name, value) in [
            ("input-res", format!("{}x{}", c.width, c.height)),
            ("fps", c.fps.max(1).to_string()),
            ("input-csp", "i420".into()),
            ("keyint", "-1".into()),
            ("scenecut", "0".into()),
            ("repeat-headers", "1".into()),
            ("level-idc", "5.1".into()),
            ("high-tier", "0".into()),
            ("frame-threads", "1".into()),
            ("pools", c.threads.max(1).to_string()),
            (
                "qpmin",
                (c.rate.quality.round().max(0.0) as u32).min(51).to_string(),
            ),
            ("range", "limited".into()),
            ("colorprim", "bt709".into()),
            ("transfer", "iec61966-2-1".into()),
            ("colormatrix", "smpte170m".into()),
            ("info", "0".into()),
            ("log-level", "warning".into()),
        ] {
            x.parse(name, &value)?;
        }
        x.rate(kbps(c.rate))?;
        x.enc = unsafe { (api.encoder_open.unwrap())(param) };
        if x.enc.is_null() {
            return Err(EncodeError("x265 encoder_open failed".into()));
        }
        x.pic = unsafe { (api.picture_alloc.unwrap())() };
        if x.pic.is_null() {
            return Err(EncodeError("x265 picture_alloc failed".into()));
        }
        unsafe { (api.picture_init.unwrap())(param, x.pic) };
        Ok(x)
    }

    fn parse(&mut self, name: &str, value: &str) -> Result<(), EncodeError> {
        let (n, v) = (CString::new(name).unwrap(), CString::new(value).unwrap());
        match unsafe { (self.api.param_parse.unwrap())(self.param, n.as_ptr(), v.as_ptr()) } {
            0 => Ok(()),
            r => Err(EncodeError(format!(
                "x265 option {name}={value} refused ({r})"
            ))),
        }
    }

    /// CBR: the target, a VBV capped at it and a third of a second of buffer.
    fn rate(&mut self, kbps: u32) -> Result<(), EncodeError> {
        self.parse("bitrate", &kbps.to_string())?;
        self.parse("vbv-maxrate", &kbps.to_string())?;
        self.parse("vbv-bufsize", &(kbps / 3).max(1).to_string())
    }

    fn nals(&mut self, pic: *mut ffi::x265_picture) -> Result<Option<Vec<u8>>, EncodeError> {
        let (mut nals, mut count) = (ptr::null_mut::<ffi::x265_nal>(), 0u32);
        let r = unsafe {
            (self.api.encoder_encode.unwrap())(
                self.enc,
                &mut nals,
                &mut count,
                pic,
                ptr::null_mut(),
            )
        };
        if r < 0 {
            return Err(EncodeError("x265 encoder_encode failed".into()));
        }
        if r == 0 || count == 0 || nals.is_null() {
            return Ok(None);
        }
        let nals = unsafe { std::slice::from_raw_parts(nals, count as usize) };
        let mut data = Vec::with_capacity(nals.iter().map(|n| n.sizeBytes as usize).sum());
        for n in nals {
            if n.payload.is_null() {
                return Err(EncodeError("x265 NAL without a payload".into()));
            }
            data.extend_from_slice(unsafe {
                std::slice::from_raw_parts(n.payload, n.sizeBytes as usize)
            });
        }
        Ok(Some(data))
    }
}

impl VideoEncoder for X265 {
    fn set_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError> {
        self.rate(kbps(rate))?;
        match unsafe { (self.api.encoder_reconfig.unwrap())(self.enc, self.param) } {
            0 => Ok(()),
            r => Err(EncodeError(format!("x265 encoder_reconfig failed ({r})"))),
        }
    }

    fn encode(
        &mut self,
        p: &FrameView<'_>,
        opts: EncodeOptions<'_>,
    ) -> Result<Vec<AccessUnit>, EncodeError> {
        if (p.width, p.height) != (self.width, self.height) {
            return Err(EncodeError(format!(
                "x265 opened at {}x{}, got {}x{}",
                self.width, self.height, p.width, p.height
            )));
        }
        p.check()?;
        self.pts = opts.pts_ms.max(self.pts + 1);
        let pic = unsafe { &mut *self.pic };
        pic.planes = [
            p.y.as_ptr() as *mut _,
            p.u.as_ptr() as *mut _,
            p.v.as_ptr() as *mut _,
        ];
        pic.stride = [p.stride_y as i32, p.stride_uv as i32, p.stride_uv as i32];
        pic.pts = self.pts;
        pic.sliceType = if opts.force_keyframe {
            ffi::X265_TYPE_IDR as i32
        } else {
            ffi::X265_TYPE_AUTO as i32
        };
        let pts = self.pts;
        Ok(self
            .nals(self.pic)?
            .and_then(|d| self.units.unit(d, pts))
            .into_iter()
            .collect())
    }

    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        let mut out = Vec::new();
        while let Some(d) = self.nals(ptr::null_mut())? {
            out.extend(self.units.unit(d, self.pts));
        }
        Ok(out)
    }
}

impl Drop for X265 {
    fn drop(&mut self) {
        unsafe {
            if !self.pic.is_null() {
                (self.api.picture_free.unwrap())(self.pic);
            }
            if !self.enc.is_null() {
                (self.api.encoder_close.unwrap())(self.enc);
            }
            (self.api.param_free.unwrap())(self.param);
        }
    }
}
