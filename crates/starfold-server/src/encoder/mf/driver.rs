//! The Media Foundation calls an encode session makes, behind a trait so the session
//! logic runs against a fake on hosts without Windows.

use crate::encoder::{Codec, EncodeError, FrameView};

/// The `ICodecAPI` properties a session sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    LowLatency,
    RateControl,
    MeanBitRate,
    MaxBitRate,
    BufferSize,
    Qp,
    GopSize,
    BFrames,
    RefFrames,
    ForceKeyFrame,
}

/// `eAVEncCommonRateControlMode`.
pub const RC_CBR: u32 = 0;
pub const RC_QUALITY: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    U32(u32),
    U64(u64),
}

/// The output stream and its NV12 input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `MF_MT_AVG_BITRATE`; the encoder picks its level for it.
    pub bps: u32,
}

pub trait Driver: Send {
    /// Sets the output type, then the NV12 input type.
    fn set_types(&mut self, f: &Format) -> Result<(), EncodeError>;
    fn set(&mut self, api: Api, v: Value) -> Result<(), EncodeError>;
    /// Starts streaming: no type or mode changes after it.
    fn begin(&mut self) -> Result<(), EncodeError>;
    /// Encodes `f` as the next frame and returns its bitstream.
    fn encode(&mut self, f: &FrameView<'_>) -> Result<Vec<u8>, EncodeError>;
}
