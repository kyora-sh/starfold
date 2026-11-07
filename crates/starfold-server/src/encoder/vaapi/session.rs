//! One encode session: an input surface, two reconstructed surfaces that take turns as
//! the reference, a coded buffer. IDR then P frames off the last frame only, an IDR on
//! request, no reordering.

use super::driver::Driver;
use super::ffi::{self, VaId};
use crate::encoder::{
    AccessUnit, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig, VideoEncoder,
};
use std::ffi::c_int;

pub struct Buffer {
    pub kind: c_int,
    pub data: Vec<u8>,
}

impl Buffer {
    pub fn of<T: Copy>(kind: c_int, t: &T) -> Buffer {
        Buffer {
            kind,
            data: ffi::bytes(t),
        }
    }

    /// A packed header's parameter and data buffers.
    pub fn packed(kind: u32, data: Vec<u8>) -> [Buffer; 2] {
        let mut p: ffi::VAEncPackedHeaderParameterBuffer = ffi::zeroed();
        p.kind = kind;
        p.bit_length = (data.len() * 8) as u32;
        p.has_emulation_bytes = 1;
        [
            Buffer::of(ffi::BUF_PACKED_PARAM, &p),
            Buffer {
                kind: ffi::BUF_PACKED_DATA,
                data,
            },
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RcMode {
    Cbr,
    Vbr,
    Cqp,
}

impl RcMode {
    fn attrib(self) -> u32 {
        match self {
            RcMode::Cbr => ffi::RC_CBR,
            RcMode::Vbr => ffi::RC_VBR,
            RcMode::Cqp => ffi::RC_CQP,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rc {
    pub mode: RcMode,
    pub bps: u32,
    pub buffer_bits: u32,
    pub fps: u32,
    pub qp: u8,
}

impl Rc {
    /// The mode for `rate` among the driver's `modes`.
    pub fn mode(rate: &RateConfig, modes: u32) -> Result<RcMode, EncodeError> {
        let order: &[RcMode] = if rate.max_kbps > 0 {
            &[RcMode::Cbr, RcMode::Vbr, RcMode::Cqp]
        } else {
            &[RcMode::Cqp, RcMode::Vbr, RcMode::Cbr]
        };
        order
            .iter()
            .copied()
            .find(|m| modes & m.attrib() != 0)
            .ok_or_else(|| {
                EncodeError(format!("no usable rate control (driver offers {modes:#x})"))
            })
    }

    pub fn new(mode: RcMode, rate: &RateConfig, prev: Option<&Rc>) -> Rc {
        let kbps = match (rate.max_kbps, prev) {
            (0, Some(p)) if p.bps > 0 => p.bps / 1000,
            (0, _) => 30_000,
            (k, _) => k,
        };
        let buf_kbit = if rate.max_kbps == 0 {
            kbps / rate.fps.max(1) * 5 / 4
        } else {
            rate.buf_kbit
        };
        Rc {
            mode,
            bps: kbps.saturating_mul(1000),
            buffer_bits: buf_kbit.max(1).saturating_mul(1000),
            fps: rate.fps.max(1),
            qp: (rate.quality.round() as i32).clamp(1, 51) as u8,
        }
    }

    /// The frame rate, rate control and HRD buffers, `reset` after a change mid-stream.
    pub fn misc(&self, reset: bool) -> Vec<Buffer> {
        let mut fr: ffi::Misc<ffi::VAEncMiscParameterFrameRate> = ffi::zeroed();
        fr.kind = ffi::MISC_FRAME_RATE;
        fr.data.framerate = self.fps;
        let mut out = vec![Buffer::of(ffi::BUF_MISC, &fr)];
        if self.mode == RcMode::Cqp {
            return out;
        }
        let mut rc: ffi::Misc<ffi::VAEncMiscParameterRateControl> = ffi::zeroed();
        rc.kind = ffi::MISC_RATE_CONTROL;
        rc.data.bits_per_second = self.bps;
        rc.data.target_percentage = if self.mode == RcMode::Cbr { 100 } else { 95 };
        rc.data.window_size =
            ((self.buffer_bits as u64 * 1000 / self.bps.max(1) as u64) as u32).max(1);
        rc.data.rc_flags = reset as u32 | 1 << 1;
        let mut hrd: ffi::Misc<ffi::VAEncMiscParameterHRD> = ffi::zeroed();
        hrd.kind = ffi::MISC_HRD;
        hrd.data.buffer_size = self.buffer_bits;
        hrd.data.initial_buffer_fullness = self.buffer_bits / 2;
        out.push(Buffer::of(ffi::BUF_MISC, &rc));
        out.push(Buffer::of(ffi::BUF_MISC, &hrd));
        out
    }
}

/// What the driver offers for one profile and entrypoint.
#[derive(Clone, Debug)]
pub struct Choice {
    pub profile: c_int,
    pub entrypoint: c_int,
    pub rc_modes: u32,
    /// Packed headers the driver takes, `None` if it doesn't say.
    pub packed: Option<u32>,
    /// The `extra` attributes asked for, in order.
    pub extra: Vec<Option<u32>>,
}

impl Choice {
    pub fn low_power(&self) -> bool {
        self.entrypoint == ffi::ENTRYPOINT_ENC_SLICE_LP
    }
}

/// The first of `profiles` (best first) the driver encodes 8-bit 4:2:0 at `size` with
/// one reference, low-power entrypoint first.
pub fn choose(
    drv: &mut dyn Driver,
    profiles: &[c_int],
    size: (u32, u32),
    extra: &[c_int],
) -> Result<Choice, EncodeError> {
    let mut why = Vec::new();
    for &profile in profiles {
        let eps = match drv.entrypoints(profile) {
            Ok(e) => e,
            Err(e) => {
                why.push(format!("profile {profile}: {e}"));
                continue;
            }
        };
        for entrypoint in [ffi::ENTRYPOINT_ENC_SLICE_LP, ffi::ENTRYPOINT_ENC_SLICE] {
            if !eps.contains(&entrypoint) {
                continue;
            }
            let mut kinds = vec![
                ffi::ATTRIB_RT_FORMAT,
                ffi::ATTRIB_RATE_CONTROL,
                ffi::ATTRIB_PACKED_HEADERS,
                ffi::ATTRIB_MAX_REF_FRAMES,
                ffi::ATTRIB_MAX_WIDTH,
                ffi::ATTRIB_MAX_HEIGHT,
            ];
            kinds.extend_from_slice(extra);
            let a = match drv.attribs(profile, entrypoint, &kinds) {
                Ok(a) => a,
                Err(e) => {
                    why.push(format!("profile {profile}/{entrypoint}: {e}"));
                    continue;
                }
            };
            let fail = if a[0].is_none_or(|f| f & ffi::RT_FORMAT_YUV420 == 0) {
                Some("no 8-bit 4:2:0")
            } else if a[3].is_some_and(|r| r & 0xffff == 0) {
                Some("no reference frames")
            } else if a[4].is_some_and(|w| w < size.0) || a[5].is_some_and(|h| h < size.1) {
                Some("picture too large")
            } else {
                None
            };
            if let Some(f) = fail {
                why.push(format!("profile {profile}/{entrypoint}: {f}"));
                continue;
            }
            return Ok(Choice {
                profile,
                entrypoint,
                rc_modes: a[1].unwrap_or(0),
                packed: a[2],
                extra: a[6..].to_vec(),
            });
        }
    }
    Err(EncodeError(if why.is_empty() {
        "no encode entrypoint for this codec".into()
    } else {
        why.join("; ")
    }))
}

pub struct Picture {
    pub idr: bool,
    /// Frames since the last IDR (0 for the IDR).
    pub n: u64,
    /// IDRs before this picture.
    pub idrs: u32,
    pub recon: VaId,
    pub reference: Option<VaId>,
    pub coded: VaId,
    pub rc: Rc,
}

/// A codec's parameter buffers and bitstream handling.
pub trait Params: Send {
    /// Sizes the surfaces are allocated at (the coded size, aligned).
    fn surface_size(&self) -> (u32, u32);
    /// Every buffer for one picture, in render order; `misc` goes right after the
    /// sequence parameters, which IDRs carry.
    fn buffers(&mut self, p: &Picture, misc: Vec<Buffer>) -> Vec<Buffer>;
    fn unit(
        &mut self,
        data: Vec<u8>,
        pts_ms: i64,
        idr: bool,
    ) -> Result<Option<AccessUnit>, EncodeError>;
}

pub struct Session<P: Params> {
    drv: Box<dyn Driver>,
    params: P,
    config: VaId,
    ctx: VaId,
    surfaces: Vec<VaId>,
    coded: VaId,
    frame: (u32, u32),
    cur: usize,
    rc: Rc,
    rc_changed: bool,
    n: u64,
    idrs: u32,
    /// The next picture must be an IDR: first frame, or the last encode failed.
    need_idr: bool,
}

impl<P: Params> Session<P> {
    pub fn new(
        mut drv: Box<dyn Driver>,
        choice: &Choice,
        cfg: &EncoderConfig,
        make: impl FnOnce(&Choice, &Rc) -> Result<P, EncodeError>,
    ) -> Result<Session<P>, EncodeError> {
        let mode = Rc::mode(&cfg.rate, choice.rc_modes)?;
        let rc = Rc::new(mode, &cfg.rate, None);
        let params = make(choice, &rc)?;
        let mut attribs = vec![
            (ffi::ATTRIB_RT_FORMAT, ffi::RT_FORMAT_YUV420),
            (ffi::ATTRIB_RATE_CONTROL, mode.attrib()),
        ];
        if let Some(p) = choice.packed {
            attribs.push((ffi::ATTRIB_PACKED_HEADERS, p));
        }
        let config = drv.create_config(choice.profile, choice.entrypoint, &attribs)?;
        let mut s = Session {
            drv,
            params,
            config,
            ctx: ffi::INVALID_ID,
            surfaces: Vec::new(),
            coded: ffi::INVALID_ID,
            frame: (cfg.width, cfg.height),
            cur: 0,
            rc,
            rc_changed: false,
            n: 0,
            idrs: 0,
            need_idr: true,
        };
        let (w, h) = s.params.surface_size();
        s.surfaces = s.drv.create_surfaces(w, h, 3)?;
        s.ctx = s.drv.create_context(config, w, h, &s.surfaces)?;
        let size = (w as usize * h as usize * 3 + (1 << 16)).max(1 << 20);
        s.coded = s.drv.create_coded(s.ctx, size)?;
        Ok(s)
    }

    fn submit(&mut self, f: &FrameView<'_>, idr: bool) -> Result<Vec<u8>, EncodeError> {
        let input = self.surfaces[0];
        self.drv.upload(input, self.params.surface_size(), f)?;
        let pic = Picture {
            idr,
            n: if idr { 0 } else { self.n },
            idrs: self.idrs,
            recon: self.surfaces[1 + self.cur],
            reference: (!idr).then_some(self.surfaces[2 - self.cur]),
            coded: self.coded,
            rc: self.rc,
        };
        let misc = if idr || self.rc_changed {
            self.rc.misc(self.rc_changed && !idr)
        } else {
            Vec::new()
        };
        let bufs = self.params.buffers(&pic, misc);
        let mut ids = Vec::with_capacity(bufs.len());
        let mut res = Ok(());
        for b in &bufs {
            match self.drv.create_buffer(self.ctx, b.kind, &b.data) {
                Ok(id) => ids.push(id),
                Err(e) => {
                    res = Err(e);
                    break;
                }
            }
        }
        if res.is_ok() {
            res = self.drv.encode(self.ctx, input, &ids);
        }
        for id in ids {
            self.drv.destroy_buffer(id);
        }
        res?;
        self.drv.coded(self.coded)
    }
}

impl<P: Params> Drop for Session<P> {
    fn drop(&mut self) {
        if self.coded != ffi::INVALID_ID {
            self.drv.destroy_buffer(self.coded);
        }
        self.drv.destroy(self.config, self.ctx, &self.surfaces);
    }
}

impl<P: Params> VideoEncoder for Session<P> {
    fn set_rate(&mut self, rate: RateConfig) -> Result<(), EncodeError> {
        let rc = Rc::new(self.rc.mode, &rate, Some(&self.rc));
        if rc != self.rc {
            self.rc_changed |= rc.mode != RcMode::Cqp || rc.fps != self.rc.fps;
            self.rc = rc;
        }
        Ok(())
    }

    fn encode(
        &mut self,
        f: &FrameView<'_>,
        opts: EncodeOptions<'_>,
    ) -> Result<Vec<AccessUnit>, EncodeError> {
        if (f.width, f.height) != self.frame {
            return Err(EncodeError(format!(
                "encoder opened at {}x{}, got {}x{}",
                self.frame.0, self.frame.1, f.width, f.height
            )));
        }
        let idr = opts.force_keyframe || self.need_idr;
        self.need_idr = true;
        let data = self.submit(f, idr)?;
        let au = self.params.unit(data, opts.pts_ms, idr)?;
        self.need_idr = false;
        self.rc_changed = false;
        if idr {
            self.idrs = self.idrs.wrapping_add(1);
            self.n = 0;
        }
        self.n += 1;
        self.cur = 1 - self.cur;
        Ok(au.into_iter().collect())
    }

    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        Ok(Vec::new())
    }
}
