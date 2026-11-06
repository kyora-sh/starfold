//! Kvazaar (BSD-3-Clause, only with the `kvazaar` feature): HEVC, low delay with no frame-level
//! parallelism, presets ultrafast to veryfast. Kvazaar can neither force a keyframe nor change
//! its rate while open, so both reopen the encoder at the next picture: a rate change costs a
//! keyframe.

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code,
    unused_imports,
    clippy::all
)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/kvazaar.rs"));
}

use super::hevc::Units;
use super::{
    AccessUnit, Codec, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig,
    VideoEncoder,
};
use std::ffi::CString;
use std::ptr;

const UNCAPPED_KBPS: u32 = 200_000;
const PRESETS: &[&str] = &["ultrafast", "superfast", "veryfast"];
/// Kvazaar reads input pictures padded to its smallest CU, as its own CLI pads them.
const CU: u32 = 8;

pub struct Kvazaar {
    api: &'static ffi::kvz_api,
    enc: *mut ffi::kvz_encoder,
    width: u32,
    height: u32,
    fps: u32,
    threads: u32,
    preset: &'static str,
    kbps: u32,
    /// The rate changed since the encoder opened.
    stale: bool,
    pts: i64,
    units: Units,
}

unsafe impl Send for Kvazaar {}

fn kbps(rate: RateConfig) -> u32 {
    if rate.max_kbps == 0 {
        UNCAPPED_KBPS
    } else {
        rate.max_kbps
    }
}

impl Kvazaar {
    pub fn new(c: &EncoderConfig) -> Result<Self, EncodeError> {
        if c.codec != Codec::Hevc {
            return Err(EncodeError(format!("Kvazaar can't encode {:?}", c.codec)));
        }
        let preset = match c.preset.as_str() {
            "" => PRESETS[0],
            p => *PRESETS.iter().find(|&&name| name == p).ok_or_else(|| {
                EncodeError(format!("Kvazaar preset {p:?} is not one of {PRESETS:?}"))
            })?,
        };
        let api = unsafe { ffi::kvz_api_get(8).as_ref() }
            .ok_or_else(|| EncodeError("Kvazaar has no 8-bit API".into()))?;
        let mut k = Kvazaar {
            api,
            enc: ptr::null_mut(),
            width: c.width,
            height: c.height,
            fps: c.fps.max(1),
            threads: c.threads.max(1),
            preset,
            kbps: kbps(c.rate),
            stale: false,
            pts: -1,
            units: Units::new(c.width, c.height),
        };
        k.reopen()?;
        Ok(k)
    }

    fn reopen(&mut self) -> Result<(), EncodeError> {
        self.close();
        self.stale = false;
        let api = self.api;
        let cfg = unsafe { (api.config_alloc.unwrap())() };
        if cfg.is_null() {
            return Err(EncodeError("Kvazaar config_alloc failed".into()));
        }
        let options = [
            ("preset", self.preset.to_string()),
            ("width", self.width.to_string()),
            ("height", self.height.to_string()),
            ("input-fps", self.fps.to_string()),
            ("period", "0".into()),
            ("gop", "0".into()),
            ("ref", "1".into()),
            ("owf", "0".into()),
            ("wpp", "1".into()),
            ("threads", self.threads.to_string()),
            (
                "bitrate",
                (self.kbps as u64 * 1000).min(i32::MAX as u64).to_string(),
            ),
            ("range", "tv".into()),
            ("colorprim", "bt709".into()),
            ("transfer", "iec61966-2-1".into()),
            ("colormatrix", "smpte170m".into()),
            ("hash", "none".into()),
        ];
        let set = (|| {
            if unsafe { (api.config_init.unwrap())(cfg) } != 1 {
                return Err(EncodeError("Kvazaar config_init failed".into()));
            }
            for (name, value) in &options {
                let (n, v) = (
                    CString::new(*name).unwrap(),
                    CString::new(value.as_str()).unwrap(),
                );
                if unsafe { (api.config_parse.unwrap())(cfg, n.as_ptr(), v.as_ptr()) } != 1 {
                    return Err(EncodeError(format!(
                        "Kvazaar option {name}={value} refused"
                    )));
                }
            }
            let enc = unsafe { (api.encoder_open.unwrap())(cfg) };
            if enc.is_null() {
                return Err(EncodeError("Kvazaar encoder_open failed".into()));
            }
            Ok(enc)
        })();
        unsafe { (api.config_destroy.unwrap())(cfg) };
        self.enc = set?;
        Ok(())
    }

    fn close(&mut self) {
        if !self.enc.is_null() {
            unsafe { (self.api.encoder_close.unwrap())(self.enc) };
            self.enc = ptr::null_mut();
        }
    }

    /// One call into the encoder: `pic` null flushes. The bytes it gave back, and the pts of
    /// the source picture they came from.
    fn call(&mut self, pic: *mut ffi::kvz_picture) -> Result<Option<(Vec<u8>, i64)>, EncodeError> {
        let api = self.api;
        let (mut chunks, mut len) = (ptr::null_mut(), 0u32);
        let (mut recon, mut src) = (ptr::null_mut(), ptr::null_mut());
        let mut info: ffi::kvz_frame_info = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            (api.encoder_encode.unwrap())(
                self.enc,
                pic,
                &mut chunks,
                &mut len,
                &mut recon,
                &mut src,
                &mut info,
            )
        };
        let pts = unsafe { src.as_ref() }.map(|s| s.pts);
        unsafe {
            (api.picture_free.unwrap())(recon);
            (api.picture_free.unwrap())(src);
        }
        let mut data = Vec::with_capacity(len as usize);
        let mut c = chunks;
        while let Some(chunk) = unsafe { c.as_ref() } {
            let n = (chunk.len as usize).min(chunk.data.len());
            data.extend_from_slice(&chunk.data[..n]);
            c = chunk.next;
        }
        unsafe { (api.chunk_free.unwrap())(chunks) };
        if ok != 1 {
            return Err(EncodeError("Kvazaar encoder_encode failed".into()));
        }
        if data.len() != len as usize {
            return Err(EncodeError(format!(
                "Kvazaar said {len} bytes and gave {}",
                data.len()
            )));
        }
        Ok((!data.is_empty()).then(|| (data, pts.unwrap_or(self.pts))))
    }
}

impl VideoEncoder for Kvazaar {
    /// Applied by reopening at the next picture: Kvazaar only takes a rate when it opens.
    fn set_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError> {
        let kbps = kbps(rate);
        self.stale |= kbps != self.kbps;
        self.kbps = kbps;
        Ok(())
    }

    fn encode(
        &mut self,
        p: &FrameView<'_>,
        opts: EncodeOptions<'_>,
    ) -> Result<Vec<AccessUnit>, EncodeError> {
        if (p.width, p.height) != (self.width, self.height) {
            return Err(EncodeError(format!(
                "Kvazaar opened at {}x{}, got {}x{}",
                self.width, self.height, p.width, p.height
            )));
        }
        p.check()?;
        let mut out = Vec::new();
        if (opts.force_keyframe || self.stale) && self.pts >= 0 {
            out.extend(self.drain()?);
            self.reopen()?;
        }
        self.pts = opts.pts_ms.max(self.pts + 1);
        let api = self.api;
        let (pw, ph) = (
            self.width.next_multiple_of(CU) as usize,
            self.height.next_multiple_of(CU) as usize,
        );
        let pic = unsafe { (api.picture_alloc.unwrap())(pw as i32, ph as i32) };
        let Some(dst) = (unsafe { pic.as_mut() }) else {
            return Err(EncodeError("Kvazaar picture_alloc failed".into()));
        };
        let (w, h) = (self.width as usize, self.height as usize);
        let stride = dst.stride as usize;
        if (dst.width, dst.height) != (pw as i32, ph as i32) || stride < pw {
            unsafe { (api.picture_free.unwrap())(pic) };
            return Err(EncodeError("Kvazaar picture smaller than asked".into()));
        }
        // Past the frame, rows and columns repeat its last ones, as Kvazaar's CLI pads.
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        for (plane, src, rows, cols, s_src, s_dst, out_rows, out_cols) in [
            (dst.y, p.y, h, w, p.stride_y, stride, ph, pw),
            (dst.u, p.u, ch, cw, p.stride_uv, stride / 2, ph / 2, pw / 2),
            (dst.v, p.v, ch, cw, p.stride_uv, stride / 2, ph / 2, pw / 2),
        ] {
            let out = unsafe { std::slice::from_raw_parts_mut(plane, s_dst * out_rows) };
            for r in 0..out_rows {
                let from = r.min(rows - 1) * s_src;
                let row = &mut out[r * s_dst..r * s_dst + out_cols];
                row[..cols].copy_from_slice(&src[from..from + cols]);
                let edge = row[cols - 1];
                row[cols..].fill(edge);
            }
        }
        dst.pts = self.pts;
        let res = self.call(pic);
        unsafe { (api.picture_free.unwrap())(pic) };
        if let Some((data, pts)) = res? {
            out.extend(self.units.unit(data, pts));
        }
        Ok(out)
    }

    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        let mut out = Vec::new();
        while let Some((data, pts)) = self.call(ptr::null_mut())? {
            out.extend(self.units.unit(data, pts));
        }
        Ok(out)
    }
}

impl Drop for Kvazaar {
    fn drop(&mut self) {
        self.close();
    }
}
