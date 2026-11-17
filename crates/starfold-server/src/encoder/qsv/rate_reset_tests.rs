//! Rate changes on a constant-bitrate Quick Sync session, against the fake runtime, which
//! refuses one by Reset unless Init turned HRD conformance off (as oneVPL documents it): the
//! stream's first rate change right after open, its steady-state ones, a Reset the runtime
//! still refuses, and what a Reset starts from.

use super::ffi::{self, VideoParam};
use super::session::Session;
use super::tests::{Fake, INCOMPATIBLE_VIDEO_PARAM};
use crate::encoder::hw::{sample_config, Rc};
use crate::encoder::{
    AccessUnit, Backend, Codec, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig,
    VideoEncoder,
};
use crate::video::keyframe_rate;
use std::collections::VecDeque;

const W: u32 = 1920;
const H: u32 = 1080;

/// A stream's encoder: 1920x1080 at 30 fps, capped at the default 30 Mbit/s.
fn cfg() -> EncoderConfig {
    let mut c = sample_config(Backend::Qsv, Codec::H264, W, H, 30);
    c.rate = RateConfig::new(24.0, 30_000, 30);
    c
}

fn frame(s: &mut Session, key: bool) -> Result<Vec<AccessUnit>, EncodeError> {
    let (y, uv) = (vec![16u8; (W * H) as usize], vec![128u8; (W * H / 4) as usize]);
    let f = FrameView {
        width: W,
        height: H,
        y: &y,
        u: &uv,
        v: &uv,
        stride_y: W as usize,
        stride_uv: (W / 2) as usize,
    };
    s.encode(
        &f,
        EncodeOptions {
            pts_ms: 10,
            force_keyframe: key,
            ..Default::default()
        },
    )
}

fn kbps(p: &VideoParam) -> u32 {
    p.mfx.target_kbps as u32 * p.mfx.brc_param_multiplier.max(1) as u32
}

/// What video.rs's sample() does: open, then raise to a first keyframe's rate before the
/// first picture.
#[test]
fn the_first_rate_change_after_open_goes_through() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut s = Session::new(Box::new(fake), &cfg()).unwrap();
    s.set_rate(keyframe_rate(24.0, 16_000, 30)).unwrap();
    assert!(frame(&mut s, false).unwrap()[0].keyframe);
    let l = log.lock().unwrap();
    assert_eq!(
        l.hrd,
        vec![Some((ffi::CODINGOPTION_OFF, ffi::CODINGOPTION_OFF))],
        "Init turns HRD conformance and NAL HRD parameters off"
    );
    assert_eq!(l.reset.len(), 1);
    assert_eq!(kbps(&l.reset[0]), 64_000);
    assert_eq!(l.closes, 0, "a Reset, not a reopen");
}

/// What a stream does for each rate it settles on (video.rs, a stripe's rate): every
/// change goes through Reset, with pictures in between.
#[test]
fn steady_state_rate_changes_go_through() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut s = Session::new(Box::new(fake), &cfg()).unwrap();
    s.set_rate(keyframe_rate(24.0, 16_000, 30)).unwrap();
    frame(&mut s, false).unwrap();
    let rates = [
        RateConfig::new(24.0, 12_000, 30),
        RateConfig::new(24.0, 6_000, 30),
        keyframe_rate(24.0, 6_000, 30),
        RateConfig::new(24.0, 20_000, 30),
        RateConfig::new(24.0, 8_000, 30),
    ];
    for r in rates {
        s.set_rate(r).unwrap();
        frame(&mut s, false).unwrap();
    }
    let l = log.lock().unwrap();
    let asked: Vec<u32> = l.reset.iter().map(kbps).collect();
    assert_eq!(asked, vec![64_000, 12_000, 6_000, 24_000, 20_000, 8_000]);
    assert_eq!(l.closes, 0);
}

/// A Reset the runtime refuses as incompatible closes the encoder and opens it again at the
/// new rate, as oneVPL asks; the next picture is an IDR.
#[test]
fn a_reset_the_runtime_refuses_reopens_at_the_new_rate() {
    let (mut fake, log) = Fake::new(Codec::H264);
    fake.reset = VecDeque::from([INCOMPATIBLE_VIDEO_PARAM]);
    let mut s = Session::new(Box::new(fake), &cfg()).unwrap();
    assert!(frame(&mut s, false).unwrap()[0].keyframe);
    assert!(!frame(&mut s, false).unwrap()[0].keyframe);
    s.set_rate(RateConfig::new(24.0, 12_000, 30)).unwrap();
    assert!(frame(&mut s, false).unwrap()[0].keyframe, "an IDR after it");
    let l = log.lock().unwrap();
    assert_eq!(l.closes, 1);
    assert_eq!(l.inited.len(), 2);
    assert_eq!(kbps(&l.inited[1]), 12_000, "opened again at the new rate");
    assert_eq!(
        l.hrd[1],
        Some((ffi::CODINGOPTION_OFF, ffi::CODINGOPTION_OFF))
    );
    assert_eq!(
        (l.inited[1].mfx.frame_info.crop_w, l.inited[1].mfx.frame_info.crop_h),
        (W as u16, H as u16)
    );
}

/// A Reset starts from what the runtime says the encoder runs with (GetVideoParam), which
/// may differ from what Init asked, and changes only the rate.
#[test]
fn a_reset_starts_from_what_the_encoder_runs_with() {
    let (mut fake, log) = Fake::new(Codec::H264);
    let rc = Rc::new(Codec::H264, &cfg().rate, None);
    let mut working = super::session::params(Codec::H264, (W, H), &rc, false);
    working.mfx.gop_pic_size = 300;
    working.mfx.target_usage = 4;
    fake.got = Some(working);
    let mut s = Session::new(Box::new(fake), &cfg()).unwrap();
    s.set_rate(RateConfig::new(24.0, 12_000, 30)).unwrap();
    let l = log.lock().unwrap();
    let r = l.reset[0].mfx;
    assert_eq!((r.gop_pic_size, r.target_usage), (300, 4), "the runtime's values");
    assert_eq!(kbps(&l.reset[0]), 12_000);
}
