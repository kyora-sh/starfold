use super::driver::{Api, Driver, Format, Value, RC_CBR, RC_QUALITY};
use super::session;
use crate::encoder::hw::sample_config;
use crate::encoder::{
    AccessUnit, Backend, Codec, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig,
    VideoEncoder,
};
use std::sync::{Arc, Mutex};

const H264_SPS: &[u8] = &[0x67, 0x42, 0xc0, 0x1f, 0xda];
const H264_PPS: &[u8] = &[0x68, 0xce, 0x3c, 0x80];
const HEVC_VPS: &[u8] = &[0x40, 0x01, 0x0c];
const HEVC_SPS: &[u8] = &[
    0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03,
    0x00, 0x5d, 0xa0,
];
const HEVC_PPS: &[u8] = &[0x44, 0x01, 0xc1];
const AV1_SEQ: &[u8] = &[
    0x00, 0x00, 0x00, 0x6a, 0xee, 0x7f, 0xd8, 0xfc, 0xc1, 0xb2, 0x02, 0x1a, 0x0c, 0x00,
];

fn annexb(nals: &[&[u8]]) -> Vec<u8> {
    nals.iter()
        .flat_map(|n| [&[0, 0, 0, 1][..], *n].concat())
        .collect()
}

fn bitstream(codec: Codec, idr: bool) -> Vec<u8> {
    match (codec, idr) {
        (Codec::H264, true) => annexb(&[H264_SPS, H264_PPS, &[0x65, 0x88, 0x84]]),
        (Codec::H264, false) => annexb(&[&[0x41, 0x9a, 0x02]]),
        (Codec::Hevc, true) => annexb(&[HEVC_VPS, HEVC_SPS, HEVC_PPS, &[0x26, 0x01, 0xaf]]),
        (Codec::Hevc, false) => annexb(&[&[0x02, 0x01, 0xd0]]),
        (Codec::Av1, true) => {
            let mut d = vec![0x0a, AV1_SEQ.len() as u8];
            d.extend_from_slice(AV1_SEQ);
            d.extend_from_slice(&[0x32, 0x01, 0x10]);
            d
        }
        (_, false) => vec![0x32, 0x01, 0x30],
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Call {
    Types(Format),
    Set(Api, Value),
    Begin,
    Encode,
}

struct Fake {
    log: Arc<Mutex<Vec<Call>>>,
    codec: Codec,
    refuse: Vec<Api>,
    fail_encode: Option<usize>,
    /// Codes this frame (0-based) as a key frame unasked.
    own_key: Option<usize>,
    /// Ignores forced key frames.
    no_idr: bool,
    key_asked: bool,
    frames: usize,
}

impl Fake {
    fn new(codec: Codec) -> (Fake, Arc<Mutex<Vec<Call>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            log: log.clone(),
            codec,
            refuse: Vec::new(),
            fail_encode: None,
            own_key: None,
            no_idr: false,
            key_asked: false,
            frames: 0,
        };
        (fake, log)
    }
}

impl Driver for Fake {
    fn set_types(&mut self, f: &Format) -> Result<(), EncodeError> {
        self.log.lock().unwrap().push(Call::Types(*f));
        Ok(())
    }

    fn set(&mut self, api: Api, v: Value) -> Result<(), EncodeError> {
        if self.refuse.contains(&api) {
            return Err(EncodeError("E_NOTIMPL".into()));
        }
        self.log.lock().unwrap().push(Call::Set(api, v));
        if api == Api::ForceKeyFrame {
            self.key_asked = true;
        }
        Ok(())
    }

    fn begin(&mut self) -> Result<(), EncodeError> {
        self.log.lock().unwrap().push(Call::Begin);
        Ok(())
    }

    fn encode(&mut self, f: &FrameView<'_>) -> Result<Vec<u8>, EncodeError> {
        f.check()?;
        self.log.lock().unwrap().push(Call::Encode);
        let n = self.frames;
        self.frames += 1;
        let asked = std::mem::take(&mut self.key_asked);
        if self.fail_encode == Some(n) {
            return Err(EncodeError("injected".into()));
        }
        let key = (asked && !self.no_idr) || self.own_key == Some(n);
        Ok(bitstream(self.codec, key))
    }
}

fn cfg(codec: Codec, w: u32, h: u32, kbps: u32) -> EncoderConfig {
    let mut c = sample_config(Backend::MediaFoundation, codec, w, h, 30);
    c.rate = RateConfig::new(24.0, kbps, 30);
    c
}

fn try_encode(
    enc: &mut dyn VideoEncoder,
    w: u32,
    h: u32,
    key: bool,
) -> Result<Vec<AccessUnit>, EncodeError> {
    let (y, uv) = (
        vec![16u8; (w * h) as usize],
        vec![128u8; (w.div_ceil(2) * h.div_ceil(2)) as usize],
    );
    let f = FrameView {
        width: w,
        height: h,
        y: &y,
        u: &uv,
        v: &uv,
        stride_y: w as usize,
        stride_uv: w.div_ceil(2) as usize,
    };
    enc.encode(
        &f,
        EncodeOptions {
            force_keyframe: key,
            ..Default::default()
        },
    )
}

fn encode(enc: &mut dyn VideoEncoder, w: u32, h: u32, key: bool) -> Vec<AccessUnit> {
    try_encode(enc, w, h, key).unwrap()
}

/// The calls so far, emptying the log.
fn take(log: &Arc<Mutex<Vec<Call>>>) -> Vec<Call> {
    std::mem::take(&mut *log.lock().unwrap())
}

const KEY: Call = Call::Set(Api::ForceKeyFrame, Value::U32(1));

#[test]
fn h264_low_latency_cbr_with_an_idr_on_request() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 1920, 1080, 6000)).unwrap();
    assert_eq!(
        take(&log),
        vec![
            Call::Types(Format {
                codec: Codec::H264,
                width: 1920,
                height: 1080,
                fps: 30,
                bps: 6_000_000,
            }),
            Call::Set(Api::LowLatency, Value::U32(1)),
            Call::Set(Api::RateControl, Value::U32(RC_CBR)),
            Call::Set(Api::MeanBitRate, Value::U32(6_000_000)),
            Call::Set(Api::MaxBitRate, Value::U32(6_000_000)),
            Call::Set(Api::BufferSize, Value::U32(250_000)),
            Call::Set(Api::BFrames, Value::U32(0)),
            Call::Set(Api::RefFrames, Value::U32(1)),
            Call::Set(Api::GopSize, Value::U32(0)),
            Call::Begin,
        ]
    );
    let first = encode(enc.as_mut(), 1920, 1080, false);
    assert!(first[0].keyframe, "the first frame is an IDR");
    let c = first[0].config.as_ref().unwrap();
    assert_eq!(
        (c.codec_string.as_str(), c.coded),
        ("avc1.42c01f", (1920, 1080))
    );
    let p = encode(enc.as_mut(), 1920, 1080, false);
    assert!(!p[0].keyframe && p[0].config.is_none());
    let k = encode(enc.as_mut(), 1920, 1080, true);
    assert!(k[0].keyframe);
    assert!(k[0].config.is_none(), "the same SPS is no new config");
    assert_eq!(
        take(&log),
        vec![KEY, Call::Encode, Call::Encode, KEY, Call::Encode]
    );
    assert!(try_encode(enc.as_mut(), 1280, 720, false).is_err());
}

#[test]
fn hevc_constant_qp_when_uncapped() {
    let (fake, log) = Fake::new(Codec::Hevc);
    let mut enc = session(Box::new(fake), &cfg(Codec::Hevc, 1280, 720, 0)).unwrap();
    let calls = take(&log);
    let Call::Types(f) = calls[0] else {
        panic!("{calls:?}")
    };
    assert!(f.bps > 0, "the output type still has a bitrate");
    assert!(calls.contains(&Call::Set(Api::RateControl, Value::U32(RC_QUALITY))));
    assert!(calls.contains(&Call::Set(Api::Qp, Value::U64(24))));
    assert!(!calls
        .iter()
        .any(|c| matches!(c, Call::Set(Api::MeanBitRate, _))));
    let first = encode(enc.as_mut(), 1280, 720, false);
    assert!(first[0].keyframe);
    assert_eq!(
        first[0].config.as_ref().unwrap().codec_string,
        "hvc1.1.6.L93.90"
    );
    assert!(!encode(enc.as_mut(), 1280, 720, false)[0].keyframe);
}

#[test]
fn av1_gets_temporal_delimiters_and_its_sequence_header() {
    let (fake, log) = Fake::new(Codec::Av1);
    let mut enc = session(Box::new(fake), &cfg(Codec::Av1, 1920, 1080, 0)).unwrap();
    assert!(take(&log).contains(&Call::Set(Api::Qp, Value::U64(120))));
    let first = encode(enc.as_mut(), 1920, 1080, false);
    assert_eq!(&first[0].data[..2], &[0x12, 0x00]);
    let c = first[0].config.as_ref().unwrap();
    assert_eq!(c.codec_string, "av01.0.13M.08.0.110.01.13.06.0");
    assert_eq!(c.format, crate::encoder::BitstreamFormat::Av1LowOverhead);
    let p = encode(enc.as_mut(), 1920, 1080, false);
    assert!(!p[0].keyframe && p[0].config.is_none());
}

#[test]
fn hints_may_be_refused_but_the_modes_may_not() {
    let (mut fake, _) = Fake::new(Codec::H264);
    fake.refuse = vec![
        Api::MaxBitRate,
        Api::BufferSize,
        Api::BFrames,
        Api::RefFrames,
        Api::GopSize,
    ];
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 320, 240, 2000)).unwrap();
    assert!(encode(enc.as_mut(), 320, 240, false)[0].keyframe);
    for api in [
        Api::LowLatency,
        Api::RateControl,
        Api::MeanBitRate,
        Api::ForceKeyFrame,
    ] {
        let (mut fake, _) = Fake::new(Codec::H264);
        fake.refuse = vec![api];
        let r = session(Box::new(fake), &cfg(Codec::H264, 320, 240, 2000))
            .and_then(|mut enc| try_encode(enc.as_mut(), 320, 240, false));
        let e = r.err().unwrap();
        assert!(e.0.contains(&format!("{api:?}")), "{e}");
    }
    let (mut fake, _) = Fake::new(Codec::Hevc);
    fake.refuse = vec![Api::Qp];
    assert!(session(Box::new(fake), &cfg(Codec::Hevc, 320, 240, 0)).is_err());
}

#[test]
fn rate_changes_set_the_bitrate_or_qp() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 640, 480, 4000)).unwrap();
    take(&log);
    enc.set_rate(RateConfig::new(24.0, 4000, 30)).unwrap();
    assert!(take(&log).is_empty(), "nothing changed");
    enc.set_rate(RateConfig::new(24.0, 9000, 30)).unwrap();
    assert_eq!(
        take(&log)[0],
        Call::Set(Api::MeanBitRate, Value::U32(9_000_000))
    );
    enc.set_rate(RateConfig::new(24.0, 0, 30)).unwrap();
    assert!(take(&log).is_empty(), "uncapped keeps a CBR session's rate");

    let (fake, log) = Fake::new(Codec::Hevc);
    let mut enc = session(Box::new(fake), &cfg(Codec::Hevc, 640, 480, 0)).unwrap();
    take(&log);
    enc.set_rate(RateConfig::new(24.0, 9000, 30)).unwrap();
    assert!(
        take(&log).is_empty(),
        "a constant-QP session ignores bitrates"
    );
    enc.set_rate(RateConfig::new(30.0, 9000, 30)).unwrap();
    assert_eq!(take(&log), vec![Call::Set(Api::Qp, Value::U64(30))]);
}

#[test]
fn a_failed_encode_restarts_with_an_idr() {
    let (mut fake, log) = Fake::new(Codec::H264);
    fake.fail_encode = Some(1);
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 320, 240, 2000)).unwrap();
    encode(enc.as_mut(), 320, 240, false);
    take(&log);
    assert!(try_encode(enc.as_mut(), 320, 240, false).is_err());
    assert!(encode(enc.as_mut(), 320, 240, false)[0].keyframe);
    assert_eq!(take(&log), vec![Call::Encode, KEY, Call::Encode]);
}

#[test]
fn the_encoders_own_key_frames_pass_but_a_p_frame_for_an_idr_does_not() {
    for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
        let (mut fake, _) = Fake::new(codec);
        fake.own_key = Some(2);
        let mut enc = session(Box::new(fake), &cfg(codec, 320, 240, 2000)).unwrap();
        encode(enc.as_mut(), 320, 240, false);
        assert!(!encode(enc.as_mut(), 320, 240, false)[0].keyframe);
        assert!(
            encode(enc.as_mut(), 320, 240, false)[0].keyframe,
            "{codec:?}"
        );

        let (mut fake, _) = Fake::new(codec);
        fake.no_idr = true;
        let mut enc = session(Box::new(fake), &cfg(codec, 320, 240, 2000)).unwrap();
        let e = try_encode(enc.as_mut(), 320, 240, true).err().unwrap();
        assert!(e.0.contains("asked for an IDR"), "{codec:?}: {e}");
    }
}
