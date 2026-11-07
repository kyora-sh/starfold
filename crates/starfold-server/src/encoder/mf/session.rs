//! One Media Foundation session: low-latency mode, no B frames, one reference, an IDR on
//! request through `CODECAPI_AVEncVideoForceKeyFrame`, CBR or constant QP.

use super::driver::{Api, Driver, Format, Value, RC_CBR, RC_QUALITY};
use crate::encoder::hw::{self, Rc};
use crate::encoder::{
    AccessUnit, Codec, ColorInfo, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig,
    VideoEncoder,
};

/// `MF_MT_AVG_BITRATE` of a constant-QP session, which still needs one to pick a level.
const UNCAPPED_BPS: u32 = 20_000_000;

pub struct Session {
    drv: Box<dyn Driver>,
    codec: Codec,
    size: (u32, u32),
    rc: Rc,
    /// The next picture must be an IDR: first frame, or the last encode failed.
    need_idr: bool,
    stream: hw::Stream,
}

/// Sets `api`, or says which one the encoder refused.
fn set(drv: &mut dyn Driver, api: Api, v: Value) -> Result<(), EncodeError> {
    drv.set(api, v)
        .map_err(|e| EncodeError(format!("Media Foundation {api:?}: {}", e.0)))
}

/// The bitrate (CBR) or QP of `rc`; the buffer size and peak are hints the encoder may
/// not take.
fn rate(drv: &mut dyn Driver, rc: &Rc) -> Result<(), EncodeError> {
    if rc.cbr {
        set(drv, Api::MeanBitRate, Value::U32(rc.bps))?;
        let _ = drv.set(Api::MaxBitRate, Value::U32(rc.bps));
        let _ = drv.set(Api::BufferSize, Value::U32(rc.buffer_bits));
        Ok(())
    } else {
        set(drv, Api::Qp, Value::U64(rc.qp.into()))
    }
}

impl Session {
    pub fn new(mut drv: Box<dyn Driver>, cfg: &EncoderConfig) -> Result<Session, EncodeError> {
        if !matches!(cfg.codec, Codec::H264 | Codec::Hevc | Codec::Av1) {
            return Err(EncodeError(format!(
                "Media Foundation has no {:?} encoder",
                cfg.codec
            )));
        }
        let rc = Rc::new(cfg.codec, &cfg.rate, None);
        drv.set_types(&Format {
            codec: cfg.codec,
            width: cfg.width,
            height: cfg.height,
            fps: rc.fps,
            bps: if rc.cbr { rc.bps } else { UNCAPPED_BPS },
        })?;
        let d = drv.as_mut();
        set(d, Api::LowLatency, Value::U32(1))?;
        set(
            d,
            Api::RateControl,
            Value::U32(if rc.cbr { RC_CBR } else { RC_QUALITY }),
        )?;
        rate(d, &rc)?;
        let _ = d.set(Api::BFrames, Value::U32(0));
        let _ = d.set(Api::RefFrames, Value::U32(1));
        let _ = d.set(Api::GopSize, Value::U32(0));
        d.begin()?;
        let size = (cfg.width, cfg.height);
        Ok(Session {
            drv,
            codec: cfg.codec,
            size,
            rc,
            need_idr: true,
            stream: hw::Stream::new(
                "Media Foundation",
                cfg.codec,
                size,
                ColorInfo::SRGB_BT601_LIMITED,
                true,
            ),
        })
    }
}

impl VideoEncoder for Session {
    /// A new bitrate or QP; the frame rate stays the one the session opened with.
    fn set_rate(&mut self, rate_cfg: RateConfig) -> Result<(), EncodeError> {
        let mut rc = Rc::new(self.codec, &rate_cfg, Some(&self.rc));
        rc.fps = self.rc.fps;
        if rc == self.rc {
            return Ok(());
        }
        rate(self.drv.as_mut(), &rc)?;
        self.rc = rc;
        Ok(())
    }

    fn encode(
        &mut self,
        f: &FrameView<'_>,
        opts: EncodeOptions<'_>,
    ) -> Result<Vec<AccessUnit>, EncodeError> {
        if (f.width, f.height) != self.size {
            return Err(EncodeError(format!(
                "encoder opened at {}x{}, got {}x{}",
                self.size.0, self.size.1, f.width, f.height
            )));
        }
        let idr = opts.force_keyframe || self.need_idr;
        self.need_idr = true;
        if idr {
            set(self.drv.as_mut(), Api::ForceKeyFrame, Value::U32(1))?;
        }
        let data = self.drv.encode(f)?;
        let au = self.stream.unit(data, opts.pts_ms, idr)?;
        self.need_idr = false;
        Ok(au.into_iter().collect())
    }

    fn drain(&mut self) -> Result<Vec<AccessUnit>, EncodeError> {
        Ok(Vec::new())
    }
}
