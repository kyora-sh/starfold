use super::ffi::{self, Config, InitParams};
use super::*;
use crate::encoder::hw::sample_config;
use crate::encoder::{AccessUnit, EncodeOptions, FrameView, RateConfig};
use std::sync::{Arc, Mutex};

const H264_SPS: &[u8] = &[0x67, 0x42, 0xc0, 0x1f, 0xda];
const H264_PPS: &[u8] = &[0x68, 0xce, 0x3c, 0x80];
const HEVC_VPS: &[u8] = &[0x40, 0x01, 0x0c];
const HEVC_SPS: &[u8] = &[
    0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03,
    0x00, 0x5d, 0xa0,
];
const HEVC_PPS: &[u8] = &[0x44, 0x01, 0xc1];
/// The web client's 8-bit design example.
const AV1_SEQ: &[u8] = &[
    0x00, 0x00, 0x00, 0x6a, 0xee, 0x7f, 0xd8, 0xfc, 0xc1, 0xb2, 0x02, 0x1a, 0x0c, 0x00,
];

fn annexb(nals: &[&[u8]]) -> Vec<u8> {
    nals.iter()
        .flat_map(|n| [&[0, 0, 0, 1][..], *n].concat())
        .collect()
}

/// What the driver would code: parameter sets and an IDR when forced, else a P frame.
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

#[derive(Default)]
struct Log {
    init: Vec<(InitParams, Config)>,
    reconfigured: Vec<(InitParams, Config, u32)>,
    flags: Vec<u32>,
}

struct Fake {
    log: Arc<Mutex<Log>>,
    codec: Codec,
    fail_encode: Option<usize>,
    /// Codes a P frame whatever it is asked for.
    no_idr: bool,
    /// Leaves the PPS out of the first IDR.
    first_without_pps: bool,
}

impl Fake {
    fn new(codec: Codec) -> (Fake, Arc<Mutex<Log>>) {
        let log = Arc::new(Mutex::new(Log::default()));
        let fake = Fake {
            log: log.clone(),
            codec,
            fail_encode: None,
            no_idr: false,
            first_without_pps: false,
        };
        (fake, log)
    }
}

impl Driver for Fake {
    fn preset(&mut self, codec: Guid) -> Result<Box<Config>, EncodeError> {
        let mut c: Box<Config> = Box::new(unsafe { std::mem::zeroed() });
        c.gop_length = 250;
        c.frame_interval_p = 3;
        c.rc.flags = ffi::RC_ENABLE_LOOKAHEAD;
        c.rc.lookahead_depth = 8;
        match codec {
            CODEC_H264 => c.codec.h264.flags = ffi::H264_DISABLE_SPSPPS | 1,
            CODEC_HEVC => c.codec.hevc.flags = ffi::HEVC_DISABLE_SPSPPS | 1,
            _ => c.codec.av1.flags = ffi::AV1_DISABLE_SEQ_HDR | ffi::AV1_ANNEXB | 1 << 7,
        }
        Ok(c)
    }

    fn initialize(&mut self, init: &InitParams, cfg: &Config) -> Result<(), EncodeError> {
        assert!(init.encode_config.is_null());
        self.log.lock().unwrap().init.push((*init, *cfg));
        Ok(())
    }

    fn reconfigure(
        &mut self,
        init: &InitParams,
        cfg: &Config,
        flags: u32,
    ) -> Result<(), EncodeError> {
        self.log
            .lock()
            .unwrap()
            .reconfigured
            .push((*init, *cfg, flags));
        Ok(())
    }

    fn encode(&mut self, f: &FrameView<'_>, flags: u32) -> Result<Vec<u8>, EncodeError> {
        f.check()?;
        let mut log = self.log.lock().unwrap();
        log.flags.push(flags);
        if self.fail_encode == Some(log.flags.len() - 1) {
            return Err(EncodeError("injected".into()));
        }
        if self.first_without_pps && log.flags.len() == 1 {
            return Ok(annexb(&[H264_SPS, &[0x65, 0x88, 0x84]]));
        }
        Ok(bitstream(
            self.codec,
            !self.no_idr && flags & ffi::PIC_FLAG_FORCEIDR != 0,
        ))
    }
}

fn cfg(codec: Codec, w: u32, h: u32, kbps: u32) -> EncoderConfig {
    let mut c = sample_config(Backend::Nvenc, codec, w, h, 30);
    c.rate = RateConfig::new(24.0, kbps, 30);
    c
}

fn encode(enc: &mut dyn VideoEncoder, w: u32, h: u32, key: bool) -> Vec<AccessUnit> {
    try_encode(enc, w, h, key).unwrap()
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

const IDR: u32 = ffi::PIC_FLAG_FORCEIDR | ffi::PIC_FLAG_OUTPUT_SPSPPS;

#[test]
fn h264_low_latency_cbr_with_an_idr_on_request() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 1920, 1080, 6000)).unwrap();
    {
        let l = log.lock().unwrap();
        let (i, c) = &l.init[0];
        assert_eq!(i.version, ffi::INITIALIZE_PARAMS_VER);
        assert_eq!((i.encode_guid, i.preset_guid), (CODEC_H264, ffi::PRESET_P4));
        assert_eq!(i.tuning_info, ffi::TUNING_ULTRA_LOW_LATENCY);
        assert_eq!((i.encode_width, i.encode_height), (1920, 1080));
        assert_eq!((i.max_encode_width, i.max_encode_height), (1920, 1080));
        assert_eq!((i.frame_rate_num, i.frame_rate_den), (30, 1));
        assert_eq!((i.enable_ptd, i.enable_encode_async), (1, 0));
        assert_eq!(c.version, ffi::CONFIG_VER);
        assert_eq!((c.gop_length, c.frame_interval_p), (ffi::INFINITE_GOP, 1));
        assert_eq!(c.rc.mode, ffi::RC_CBR);
        assert_eq!(
            (c.rc.average_bit_rate, c.rc.max_bit_rate),
            (6_000_000, 6_000_000)
        );
        assert_eq!(c.rc.vbv_buffer_size, 250_000);
        assert_eq!(
            c.rc.flags & (ffi::RC_ZERO_REORDER_DELAY | ffi::RC_ENABLE_LOOKAHEAD),
            ffi::RC_ZERO_REORDER_DELAY
        );
        assert_eq!(c.rc.lookahead_depth, 0);
        let h = unsafe { c.codec.h264 };
        assert_eq!(
            h.flags,
            ffi::H264_REPEAT_SPSPPS | 1,
            "the preset's other bits stay"
        );
        assert_eq!((h.idr_period, h.max_num_ref_frames), (ffi::INFINITE_GOP, 1));
        assert_eq!(h.num_ref_l0, ffi::NUM_REF_FRAMES_1);
        assert_eq!(
            (
                h.vui.colour_description_present_flag,
                h.vui.colour_primaries,
                h.vui.transfer_characteristics,
                h.vui.colour_matrix,
                h.vui.video_full_range_flag
            ),
            (1, 1, 13, 6, 0)
        );
    }
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
    assert_eq!(log.lock().unwrap().flags, vec![IDR, 0, IDR]);
    assert!(try_encode(enc.as_mut(), 1280, 720, false).is_err());
}

#[test]
fn hevc_constant_qp_when_uncapped() {
    let (fake, log) = Fake::new(Codec::Hevc);
    let mut enc = session(Box::new(fake), &cfg(Codec::Hevc, 1280, 720, 0)).unwrap();
    {
        let l = log.lock().unwrap();
        let c = &l.init[0].1;
        assert_eq!(c.rc.mode, ffi::RC_CONSTQP);
        assert_eq!(
            (
                c.rc.const_qp.intra,
                c.rc.const_qp.inter_p,
                c.rc.average_bit_rate
            ),
            (24, 24, 0)
        );
        let h = unsafe { c.codec.hevc };
        assert_eq!(h.flags, ffi::HEVC_REPEAT_SPSPPS | 1);
        assert_eq!(
            (h.idr_period, h.max_num_ref_frames_in_dpb),
            (ffi::INFINITE_GOP, 1)
        );
        assert_eq!(h.vui.colour_matrix, 6);
    }
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
    {
        let l = log.lock().unwrap();
        let c = &l.init[0].1;
        assert_eq!(c.rc.const_qp.intra, 120, "AV1 QP is a qindex");
        let a = unsafe { c.codec.av1 };
        assert_eq!(a.flags, ffi::AV1_REPEAT_SEQ_HDR | 1 << 7);
        assert_eq!(
            (a.idr_period, a.max_num_ref_frames_in_dpb),
            (ffi::INFINITE_GOP, 1)
        );
        assert_eq!(
            (
                a.color_primaries,
                a.transfer_characteristics,
                a.matrix_coefficients,
                a.color_range
            ),
            (1, 13, 6, 0)
        );
    }
    let first = encode(enc.as_mut(), 1920, 1080, false);
    assert_eq!(&first[0].data[..2], &[0x12, 0x00]);
    let c = first[0].config.as_ref().unwrap();
    assert_eq!(c.codec_string, "av01.0.13M.08.0.110.01.13.06.0");
    assert_eq!(c.format, crate::encoder::BitstreamFormat::Av1LowOverhead);
    let p = encode(enc.as_mut(), 1920, 1080, false);
    assert!(!p[0].keyframe && p[0].config.is_none());
    assert_eq!(&p[0].data[..2], &[0x12, 0x00]);
}

#[test]
fn rate_changes_reconfigure_and_a_higher_rate_takes_an_idr() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 640, 480, 4000)).unwrap();
    encode(enc.as_mut(), 640, 480, false);
    enc.set_rate(RateConfig::new(24.0, 4000, 30)).unwrap();
    assert!(
        log.lock().unwrap().reconfigured.is_empty(),
        "nothing changed"
    );
    enc.set_rate(RateConfig::new(24.0, 2000, 30)).unwrap();
    {
        let l = log.lock().unwrap();
        let (i, c, flags) = &l.reconfigured[0];
        assert_eq!(*flags, 0);
        assert_eq!(c.rc.average_bit_rate, 2_000_000);
        assert_eq!(i.frame_rate_num, 30);
        assert_eq!(
            unsafe { c.codec.h264.flags } & ffi::H264_REPEAT_SPSPPS,
            ffi::H264_REPEAT_SPSPPS
        );
    }
    assert!(!encode(enc.as_mut(), 640, 480, false)[0].keyframe);
    enc.set_rate(RateConfig::new(24.0, 3000, 30)).unwrap();
    assert_eq!(
        log.lock().unwrap().reconfigured[1].2,
        0,
        "still under the peak"
    );
    enc.set_rate(RateConfig::new(24.0, 9000, 30)).unwrap();
    assert_eq!(
        log.lock().unwrap().reconfigured[2].2,
        ffi::RECONFIGURE_RESET | ffi::RECONFIGURE_FORCE_IDR
    );
    assert!(encode(enc.as_mut(), 640, 480, false)[0].keyframe);
    enc.set_rate(RateConfig::new(24.0, 9000, 24)).unwrap();
    assert_eq!(log.lock().unwrap().reconfigured[3].0.frame_rate_num, 24);
    enc.set_rate(RateConfig::new(24.0, 0, 24)).unwrap();
    assert_eq!(
        log.lock().unwrap().reconfigured.len(),
        4,
        "uncapped keeps a CBR session's rate"
    );
    assert_eq!(*log.lock().unwrap().flags.last().unwrap(), IDR);
}

#[test]
fn a_constant_qp_session_ignores_bitrates() {
    let (fake, log) = Fake::new(Codec::Hevc);
    let mut enc = session(Box::new(fake), &cfg(Codec::Hevc, 640, 480, 0)).unwrap();
    enc.set_rate(RateConfig::new(24.0, 9000, 30)).unwrap();
    assert!(log.lock().unwrap().reconfigured.is_empty());
    enc.set_rate(RateConfig::new(30.0, 9000, 30)).unwrap();
    let l = log.lock().unwrap();
    let (_, c, flags) = &l.reconfigured[0];
    assert_eq!(
        (c.rc.mode, c.rc.const_qp.inter_p, *flags),
        (ffi::RC_CONSTQP, 30, 0)
    );
}

#[test]
fn a_failed_encode_restarts_with_an_idr() {
    let (mut fake, log) = Fake::new(Codec::H264);
    fake.fail_encode = Some(1);
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 320, 240, 2000)).unwrap();
    encode(enc.as_mut(), 320, 240, false);
    assert!(try_encode(enc.as_mut(), 320, 240, false).is_err());
    assert!(encode(enc.as_mut(), 320, 240, false)[0].keyframe);
    assert_eq!(log.lock().unwrap().flags, vec![IDR, 0, IDR]);
}

#[test]
fn a_p_frame_for_an_idr_is_an_error() {
    for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
        let (mut fake, _) = Fake::new(codec);
        fake.no_idr = true;
        let mut enc = session(Box::new(fake), &cfg(codec, 320, 240, 2000)).unwrap();
        let e = try_encode(enc.as_mut(), 320, 240, true).err().unwrap();
        assert!(
            e.0.contains("IDR") || e.0.contains("key frame"),
            "{codec:?}: {e}"
        );
    }
}

#[test]
fn a_rejected_first_idr_does_not_hide_the_next_config() {
    let (mut fake, _) = Fake::new(Codec::H264);
    fake.first_without_pps = true;
    let mut enc = session(Box::new(fake), &cfg(Codec::H264, 320, 240, 2000)).unwrap();
    assert!(try_encode(enc.as_mut(), 320, 240, false).is_err());
    let k = encode(enc.as_mut(), 320, 240, false);
    assert!(k[0].keyframe);
    assert_eq!(k[0].config.as_ref().unwrap().codec_string, "avc1.42c01f");
}

#[test]
fn av1_output_needs_a_frame_of_the_asked_type() {
    let size = (320, 240);
    let color = crate::encoder::ColorInfo::SRGB_BT601_LIMITED;
    let mut seq_only = vec![0x0a, AV1_SEQ.len() as u8];
    seq_only.extend_from_slice(AV1_SEQ);
    let e = session::av1_unit(&mut None, seq_only, 0, true, size, color)
        .err()
        .unwrap();
    assert!(e.0.contains("no frame"), "{e}");
    let e = session::av1_unit(&mut None, vec![0x12, 0x00], 0, false, size, color)
        .err()
        .unwrap();
    assert!(e.0.contains("no frame"), "{e}");
    let key = bitstream(Codec::Av1, true);
    let e = session::av1_unit(&mut None, key, 0, false, size, color)
        .err()
        .unwrap();
    assert!(e.0.contains("frame type 0"), "{e}");
}

#[test]
fn child_sessions_need_a_device_ordinal() {
    for bad in [
        "h264:160x160@30,0,0,30,24,cuda",
        "h264:160x160@30,0,0,30,24,-1",
        "h264:160x160@30,0,0,30,24",
    ] {
        assert!(
            child_session(bad).unwrap_err().contains("bad session"),
            "{bad}"
        );
    }
}

/// The structs and constants against nvEncodeAPI.h (nv-codec-headers n12.1.14.0) in
/// `$NVENC_HEADERS`, where the gate fetched it.
#[test]
fn layouts_match_the_nvenc_header() {
    use std::mem::{offset_of, size_of};
    let Some(dir) = std::env::var_os("NVENC_HEADERS") else {
        eprintln!("skipped: NVENC_HEADERS is not set");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    assert!(
        dir.join("ffnvcodec/nvEncodeAPI.h").exists(),
        "no ffnvcodec/nvEncodeAPI.h in {}",
        dir.display()
    );
    let mut checks: Vec<(String, u64)> = Vec::new();
    let mut c = |expr: &str, rust: usize| checks.push((expr.to_string(), rust as u64));
    for (t, n) in [
        ("GUID", size_of::<Guid>()),
        ("NV_ENC_CAPS_PARAM", size_of::<ffi::CapsParam>()),
        (
            "NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS",
            size_of::<ffi::OpenParams>(),
        ),
        ("NV_ENC_RC_PARAMS", size_of::<ffi::RcParams>()),
        ("NV_ENC_CONFIG_H264_VUI_PARAMETERS", size_of::<ffi::Vui>()),
        ("NV_ENC_CONFIG_H264", size_of::<ffi::H264Config>()),
        ("NV_ENC_CONFIG_HEVC", size_of::<ffi::HevcConfig>()),
        ("NV_ENC_CONFIG_AV1", size_of::<ffi::Av1Config>()),
        ("NV_ENC_CODEC_CONFIG", size_of::<ffi::CodecConfig>()),
        ("NV_ENC_CONFIG", size_of::<Config>()),
        ("NV_ENC_INITIALIZE_PARAMS", size_of::<InitParams>()),
        ("NV_ENC_RECONFIGURE_PARAMS", size_of::<ffi::Reconfigure>()),
        ("NV_ENC_PRESET_CONFIG", size_of::<ffi::PresetConfig>()),
        (
            "NV_ENC_CREATE_INPUT_BUFFER",
            size_of::<ffi::CreateInputBuffer>(),
        ),
        (
            "NV_ENC_CREATE_BITSTREAM_BUFFER",
            size_of::<ffi::CreateBitstreamBuffer>(),
        ),
        ("NV_ENC_PIC_PARAMS", size_of::<ffi::PicParams>()),
        ("NV_ENC_CODEC_PIC_PARAMS", size_of::<[u64; 194]>()),
        ("NV_ENC_LOCK_BITSTREAM", size_of::<ffi::LockBitstream>()),
        (
            "NV_ENC_LOCK_INPUT_BUFFER",
            size_of::<ffi::LockInputBuffer>(),
        ),
        (
            "NV_ENCODE_API_FUNCTION_LIST",
            size_of::<ffi::FunctionList>(),
        ),
    ] {
        c(&format!("sizeof({t})"), n);
    }
    macro_rules! off {
        ($($ct:literal $rt:ty { $($cf:literal $rf:ident),* })*) => {$($(
            c(concat!("offsetof(", $ct, ", ", $cf, ")"), offset_of!($rt, $rf));
        )*)*};
    }
    off! {
        "NV_ENC_RC_PARAMS" ffi::RcParams {
            "rateControlMode" mode, "constQP" const_qp, "averageBitRate" average_bit_rate,
            "maxBitRate" max_bit_rate, "vbvBufferSize" vbv_buffer_size,
            "vbvInitialDelay" vbv_initial_delay, "minQP" min_qp, "lookaheadDepth" lookahead_depth,
            "qpMapMode" qp_map_mode, "cbQPIndexOffset" cb_qp_index_offset, "reserved" reserved
        }
        "NV_ENC_CONFIG_H264_VUI_PARAMETERS" ffi::Vui {
            "videoFullRangeFlag" video_full_range_flag,
            "colourDescriptionPresentFlag" colour_description_present_flag,
            "colourPrimaries" colour_primaries, "colourMatrix" colour_matrix, "timeScale" time_scale
        }
        "NV_ENC_CONFIG_H264" ffi::H264Config {
            "idrPeriod" idr_period, "maxNumRefFrames" max_num_ref_frames,
            "h264VUIParameters" vui, "numRefL0" num_ref_l0, "reserved1" reserved1,
            "reserved2" reserved2
        }
        "NV_ENC_CONFIG_HEVC" ffi::HevcConfig {
            "idrPeriod" idr_period, "maxNumRefFramesInDPB" max_num_ref_frames_in_dpb,
            "hevcVUIParameters" vui, "numRefL0" num_ref_l0, "reserved1" reserved1,
            "reserved2" reserved2
        }
        "NV_ENC_CONFIG_AV1" ffi::Av1Config {
            "idrPeriod" idr_period, "maxNumRefFramesInDPB" max_num_ref_frames_in_dpb,
            "tileWidths" tile_widths, "colorPrimaries" color_primaries,
            "transferCharacteristics" transfer_characteristics,
            "matrixCoefficients" matrix_coefficients, "colorRange" color_range,
            "filmGrainParams" film_grain_params, "numFwdRefs" num_fwd_refs,
            "reserved1" reserved1, "reserved2" reserved2
        }
        "NV_ENC_CONFIG" Config {
            "profileGUID" profile_guid, "gopLength" gop_length, "frameIntervalP" frame_interval_p,
            "rcParams" rc, "encodeCodecConfig" codec, "reserved" reserved, "reserved2" reserved2
        }
        "NV_ENC_INITIALIZE_PARAMS" InitParams {
            "encodeGUID" encode_guid, "presetGUID" preset_guid, "encodeWidth" encode_width,
            "frameRateNum" frame_rate_num, "enableEncodeAsync" enable_encode_async,
            "enablePTD" enable_ptd, "privData" priv_data, "encodeConfig" encode_config,
            "maxEncodeWidth" max_encode_width, "tuningInfo" tuning_info,
            "bufferFormat" buffer_format, "reserved" reserved, "reserved2" reserved2
        }
        "NV_ENC_RECONFIGURE_PARAMS" ffi::Reconfigure { "reInitEncodeParams" reinit }
        "NV_ENC_PRESET_CONFIG" ffi::PresetConfig { "presetCfg" preset_cfg, "reserved2" reserved2 }
        "NV_ENC_CREATE_INPUT_BUFFER" ffi::CreateInputBuffer {
            "width" width, "bufferFmt" buffer_fmt, "inputBuffer" input_buffer,
            "reserved2" reserved2
        }
        "NV_ENC_CREATE_BITSTREAM_BUFFER" ffi::CreateBitstreamBuffer {
            "bitstreamBuffer" bitstream_buffer, "reserved2" reserved2
        }
        "NV_ENC_PIC_PARAMS" ffi::PicParams {
            "inputPitch" input_pitch, "encodePicFlags" encode_pic_flags,
            "inputTimeStamp" input_time_stamp, "inputBuffer" input_buffer,
            "outputBitstream" output_bitstream, "bufferFmt" buffer_fmt,
            "pictureStruct" picture_struct, "codecPicParams" codec_pic_params,
            "meHintCountsPerBlock" me_hint_counts_per_block, "qpDeltaMap" qp_delta_map,
            "alphaBuffer" alpha_buffer, "outputReconBuffer" output_recon_buffer,
            "reserved4" reserved4
        }
        "NV_ENC_LOCK_BITSTREAM" ffi::LockBitstream {
            "outputBitstream" output_bitstream, "bitstreamSizeInBytes" bitstream_size_in_bytes,
            "bitstreamBufferPtr" bitstream_buffer_ptr, "pictureType" picture_type,
            "outputStatsPtr" output_stats_ptr, "reserved2" reserved2,
            "reservedInternal" reserved_internal
        }
        "NV_ENC_LOCK_INPUT_BUFFER" ffi::LockInputBuffer {
            "inputBuffer" input_buffer, "bufferDataPtr" buffer_data_ptr, "pitch" pitch
        }
        "NV_ENCODE_API_FUNCTION_LIST" ffi::FunctionList {
            "nvEncGetEncodeGUIDCount" get_encode_guid_count,
            "nvEncGetEncodeProfileGUIDCount" get_encode_profile_guid_count,
            "nvEncGetEncodeProfileGUIDs" get_encode_profile_guids,
            "nvEncGetEncodeGUIDs" get_encode_guids,
            "nvEncGetInputFormatCount" get_input_format_count,
            "nvEncGetInputFormats" get_input_formats, "nvEncGetEncodeCaps" get_encode_caps,
            "nvEncInitializeEncoder" initialize_encoder,
            "nvEncCreateInputBuffer" create_input_buffer,
            "nvEncDestroyInputBuffer" destroy_input_buffer,
            "nvEncCreateBitstreamBuffer" create_bitstream_buffer,
            "nvEncDestroyBitstreamBuffer" destroy_bitstream_buffer,
            "nvEncEncodePicture" encode_picture, "nvEncLockBitstream" lock_bitstream,
            "nvEncUnlockBitstream" unlock_bitstream, "nvEncLockInputBuffer" lock_input_buffer,
            "nvEncUnlockInputBuffer" unlock_input_buffer,
            "nvEncDestroyEncoder" destroy_encoder,
            "nvEncOpenEncodeSessionEx" open_encode_session_ex,
            "nvEncReconfigureEncoder" reconfigure_encoder,
            "nvEncGetLastErrorString" get_last_error_string,
            "nvEncGetEncodePresetConfigEx" get_encode_preset_config_ex
        }
    }
    let bit = |off: usize, mask: u32| off * 8 + mask.trailing_zeros() as usize;
    let (rc, hevc, av1, re) = (
        offset_of!(ffi::RcParams, flags),
        offset_of!(ffi::HevcConfig, flags),
        offset_of!(ffi::Av1Config, flags),
        offset_of!(ffi::Reconfigure, flags),
    );
    let bits = [
        (
            "NV_ENC_RC_PARAMS",
            "enableLookahead",
            bit(rc, ffi::RC_ENABLE_LOOKAHEAD),
        ),
        (
            "NV_ENC_RC_PARAMS",
            "zeroReorderDelay",
            bit(rc, ffi::RC_ZERO_REORDER_DELAY),
        ),
        (
            "NV_ENC_CONFIG_H264",
            "disableSPSPPS",
            bit(0, ffi::H264_DISABLE_SPSPPS),
        ),
        (
            "NV_ENC_CONFIG_H264",
            "enableIntraRefresh",
            bit(0, ffi::H264_INTRA_REFRESH),
        ),
        (
            "NV_ENC_CONFIG_H264",
            "repeatSPSPPS",
            bit(0, ffi::H264_REPEAT_SPSPPS),
        ),
        (
            "NV_ENC_CONFIG_HEVC",
            "disableSPSPPS",
            bit(hevc, ffi::HEVC_DISABLE_SPSPPS),
        ),
        (
            "NV_ENC_CONFIG_HEVC",
            "repeatSPSPPS",
            bit(hevc, ffi::HEVC_REPEAT_SPSPPS),
        ),
        (
            "NV_ENC_CONFIG_HEVC",
            "enableIntraRefresh",
            bit(hevc, ffi::HEVC_INTRA_REFRESH),
        ),
        (
            "NV_ENC_CONFIG_AV1",
            "outputAnnexBFormat",
            bit(av1, ffi::AV1_ANNEXB),
        ),
        (
            "NV_ENC_CONFIG_AV1",
            "disableSeqHdr",
            bit(av1, ffi::AV1_DISABLE_SEQ_HDR),
        ),
        (
            "NV_ENC_CONFIG_AV1",
            "repeatSeqHdr",
            bit(av1, ffi::AV1_REPEAT_SEQ_HDR),
        ),
        (
            "NV_ENC_CONFIG_AV1",
            "enableIntraRefresh",
            bit(av1, ffi::AV1_INTRA_REFRESH),
        ),
        (
            "NV_ENC_RECONFIGURE_PARAMS",
            "resetEncoder",
            bit(re, ffi::RECONFIGURE_RESET),
        ),
        (
            "NV_ENC_RECONFIGURE_PARAMS",
            "forceIDR",
            bit(re, ffi::RECONFIGURE_FORCE_IDR),
        ),
    ];
    for (i, (_, _, b)) in bits.iter().enumerate() {
        c(&format!("bit{i}()"), *b);
    }
    for (expr, v) in [
        ("NVENCAPI_VERSION", ffi::API_VERSION),
        ("NV_ENC_CONFIG_VER", ffi::CONFIG_VER),
        ("NV_ENC_INITIALIZE_PARAMS_VER", ffi::INITIALIZE_PARAMS_VER),
        ("NV_ENC_RECONFIGURE_PARAMS_VER", ffi::RECONFIGURE_PARAMS_VER),
        ("NV_ENC_PRESET_CONFIG_VER", ffi::PRESET_CONFIG_VER),
        ("NV_ENC_PIC_PARAMS_VER", ffi::PIC_PARAMS_VER),
        ("NV_ENC_LOCK_BITSTREAM_VER", ffi::LOCK_BITSTREAM_VER),
        ("NV_ENC_LOCK_INPUT_BUFFER_VER", ffi::LOCK_INPUT_BUFFER_VER),
        (
            "NV_ENC_CREATE_INPUT_BUFFER_VER",
            ffi::CREATE_INPUT_BUFFER_VER,
        ),
        (
            "NV_ENC_CREATE_BITSTREAM_BUFFER_VER",
            ffi::CREATE_BITSTREAM_BUFFER_VER,
        ),
        ("NV_ENC_RC_PARAMS_VER", ffi::RC_PARAMS_VER),
        ("NV_ENC_CAPS_PARAM_VER", ffi::CAPS_PARAM_VER),
        (
            "NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER",
            ffi::struct_version(1),
        ),
        ("NV_ENCODE_API_FUNCTION_LIST_VER", ffi::struct_version(2)),
        ("NVENC_INFINITE_GOPLENGTH", ffi::INFINITE_GOP),
        (
            "NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY",
            ffi::TUNING_ULTRA_LOW_LATENCY,
        ),
        ("NV_ENC_PARAMS_RC_CONSTQP", ffi::RC_CONSTQP),
        ("NV_ENC_PARAMS_RC_CBR", ffi::RC_CBR),
        ("NV_ENC_PIC_STRUCT_FRAME", ffi::PIC_STRUCT_FRAME),
        ("NV_ENC_PIC_FLAG_FORCEIDR", ffi::PIC_FLAG_FORCEIDR),
        ("NV_ENC_PIC_FLAG_OUTPUT_SPSPPS", ffi::PIC_FLAG_OUTPUT_SPSPPS),
        ("NV_ENC_NUM_REF_FRAMES_1", ffi::NUM_REF_FRAMES_1),
        ("NV_ENC_BUFFER_FORMAT_NV12", ffi::FMT_NV12),
        ("NV_ENC_BUFFER_FORMAT_IYUV", ffi::FMT_IYUV),
        ("NV_ENC_BUFFER_FORMAT_YUV444", ffi::FMT_YUV444),
        ("NV_ENC_BUFFER_FORMAT_YUV420_10BIT", ffi::FMT_YUV420_10BIT),
        ("NV_ENC_DEVICE_TYPE_CUDA", 1),
    ] {
        c(&format!("(unsigned long long)({expr})"), v as usize);
    }
    for (name, g) in [
        ("NV_ENC_CODEC_H264_GUID", CODEC_H264),
        ("NV_ENC_CODEC_HEVC_GUID", CODEC_HEVC),
        ("NV_ENC_CODEC_AV1_GUID", CODEC_AV1),
        ("NV_ENC_PRESET_P4_GUID", ffi::PRESET_P4),
    ] {
        let bytes: [u8; 16] = unsafe { std::mem::transmute(g) };
        let hex: String = bytes.iter().map(|b| format!("\\x{b:02x}")).collect();
        c(&format!("!memcmp(&{name}, \"{hex}\", 16)"), 1);
    }
    let mut src = String::from(
        "#include <stdio.h>\n#include <stddef.h>\n#include <string.h>\n#include <ffnvcodec/nvEncodeAPI.h>\n",
    );
    for (i, (t, f, _)) in bits.iter().enumerate() {
        src.push_str(&format!(
            "static size_t bit{i}(void){{ static {t} s; memset(&s, 0, sizeof s); s.{f} = 1; const unsigned char *p = (const unsigned char *)&s; size_t i = 0; while (i < sizeof s && !p[i]) i++; size_t b = 0; while (i < sizeof s && !((p[i] >> b) & 1)) b++; return i * 8 + b; }}\n"
        ));
    }
    src.push_str("int main(void){\n");
    for (expr, _) in &checks {
        src.push_str(&format!(
            "printf(\"%llu\\n\", (unsigned long long)({expr}));\n"
        ));
    }
    src.push_str("return 0;}\n");
    let tmp = std::env::temp_dir().join(format!("nvenc-layout-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("l.c"), src).unwrap();
    let cc = std::process::Command::new("cc")
        .arg("-I")
        .arg(&dir)
        .arg(tmp.join("l.c"))
        .arg("-o")
        .arg(tmp.join("l"))
        .output()
        .unwrap();
    assert!(
        cc.status.success(),
        "the layout program didn't compile:\n{}",
        String::from_utf8_lossy(&cc.stderr)
    );
    let out = std::process::Command::new(tmp.join("l")).output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let got: Vec<u64> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.parse().unwrap())
        .collect();
    assert_eq!(got.len(), checks.len());
    let bad: Vec<String> = checks
        .iter()
        .zip(&got)
        .filter(|((_, r), c)| r != *c)
        .map(|((e, r), c)| format!("{e}: rust {r}, C {c}"))
        .collect();
    assert!(bad.is_empty(), "{bad:#?}");
    eprintln!(
        "checked {} layouts and constants against nvEncodeAPI.h",
        got.len()
    );
}

/// Real hardware: opens every codec on CUDA device 0 and encodes a few frames.
#[test]
#[ignore = "needs an NVIDIA GPU with NVENC"]
fn hardware_smoke() {
    for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
        let c = cfg(codec, 1280, 720, 4000);
        match open_on(DEVICE, &c) {
            Ok(mut enc) => {
                hw::sample(enc.as_mut(), 1280, 720).unwrap();
                eprintln!("{codec:?}: ok");
            }
            Err(e) => eprintln!("{codec:?}: {e}"),
        }
    }
}
