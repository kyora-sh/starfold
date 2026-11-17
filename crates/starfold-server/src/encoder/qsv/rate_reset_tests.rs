//! Rate changes on a constant-bitrate Quick Sync session, against the fake runtime, which
//! refuses one by Reset unless Init turned HRD conformance off (as oneVPL documents it): the
//! stream's first rate change right after open, its steady-state ones, what a Reset changes
//! and starts from, what the session then reports, and a Reset the runtime refuses.

use super::driver::STOPPED;
use super::ffi::{self, VideoParam};
use super::session::Session;
use super::tests::{Fake, INCOMPATIBLE_VIDEO_PARAM};
use crate::encoder::hw::{sample_config, Rc};
use crate::encoder::{
    AccessUnit, Backend, Codec, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig,
    RateInForce, VideoEncoder,
};
use crate::video::keyframe_rate;
use std::collections::VecDeque;

const W: u32 = 1920;
const H: u32 = 1080;

/// A stream's encoder: 1920x1080 at 30 fps, capped at `kbps`.
fn cfg(kbps: u32) -> EncoderConfig {
    let mut c = sample_config(Backend::Qsv, Codec::H264, W, H, 30);
    c.rate = RateConfig::new(24.0, kbps, 30);
    c
}

fn frame(s: &mut Session, key: bool) -> Result<Vec<AccessUnit>, EncodeError> {
    let (y, uv) = (
        vec![16u8; (W * H) as usize],
        vec![128u8; (W * H / 4) as usize],
    );
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

/// TargetKbps, BRCParamMultiplier applied.
fn kbps(p: &VideoParam) -> u32 {
    p.mfx.target_kbps as u32 * p.mfx.brc_param_multiplier.max(1) as u32
}

/// `k` kbit/s as a TargetKbps of the multiplier Init chose for a 30 Mbit/s cap at 30 fps
/// (11: room for a boosted keyframe at 10 fps) can hold it.
fn at(k: u32) -> u32 {
    k / 11 * 11
}

/// Everything of the rate control but TargetKbps and MaxKbps, and the frame rate.
fn rest(p: &VideoParam) -> (u16, u16, u16, u32, u32) {
    let m = p.mfx;
    (
        m.buffer_size_in_kb,
        m.initial_delay_in_kb,
        m.brc_param_multiplier,
        m.frame_info.frame_rate_ext_n,
        m.frame_info.frame_rate_ext_d,
    )
}

/// What video.rs's sample() does: open, then raise to a first keyframe's rate before the
/// first picture.
#[test]
fn the_first_rate_change_after_open_goes_through() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut s = Session::new(Box::new(fake), &cfg(30_000)).unwrap();
    s.set_rate(keyframe_rate(24.0, 16_000, 30)).unwrap();
    assert!(frame(&mut s, false).unwrap()[0].keyframe);
    assert_eq!(s.rate_in_force(), RateInForce::Known(at(64_000)));
    let l = log.lock().unwrap();
    assert_eq!(
        l.hrd,
        vec![Some((ffi::CODINGOPTION_OFF, ffi::CODINGOPTION_OFF))],
        "Init turns HRD conformance and NAL HRD parameters off"
    );
    assert_eq!(l.inited[0].mfx.brc_param_multiplier, 11);
    assert_eq!(l.reset.len(), 1);
    assert_eq!(kbps(&l.reset[0]), at(64_000));
}

/// What a stream does for each rate it settles on (video.rs, a stripe's rate): every
/// change goes through Reset, with pictures in between, and what runs is each one.
#[test]
fn steady_state_rate_changes_go_through() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut s = Session::new(Box::new(fake), &cfg(30_000)).unwrap();
    s.set_rate(keyframe_rate(24.0, 16_000, 30)).unwrap();
    frame(&mut s, false).unwrap();
    let rates = [
        RateConfig::new(24.0, 12_000, 30),
        RateConfig::new(24.0, 6_000, 30),
        keyframe_rate(24.0, 6_000, 30),
        RateConfig::new(24.0, 20_000, 30),
        RateConfig::new(24.0, 8_000, 30),
    ];
    let mut ran = Vec::new();
    for r in rates {
        s.set_rate(r).unwrap();
        ran.push(s.rate_in_force());
        frame(&mut s, false).unwrap();
    }
    let want = [12_000, 6_000, 24_000, 20_000, 8_000].map(at);
    assert_eq!(ran, want.map(RateInForce::Known));
    let l = log.lock().unwrap();
    let asked: Vec<u32> = l.reset.iter().map(kbps).collect();
    assert_eq!(asked[0], at(64_000));
    assert_eq!(asked[1..], want);
    assert_eq!(l.inited.len(), 1, "no encoder opened again");
}

/// A Reset changes TargetKbps and MaxKbps only, as the spec allows a constant-bitrate
/// encoder: the buffer, the initial delay, the multiplier and the frame rate stay as the
/// encoder runs them. A stream taking pictures at another rate than the encoder's gets the
/// same budget per picture through TargetKbps, and the session reports the rate in the
/// stream's own terms.
#[test]
fn a_reset_changes_the_bitrate_only() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut s = Session::new(Box::new(fake), &cfg(30_000)).unwrap();
    s.set_rate(RateConfig::new(24.0, 12_000, 10)).unwrap();
    let per_picture = at(12_000 * 30 / 10);
    assert_eq!(
        s.rate_in_force(),
        RateInForce::Known(per_picture * 10 / 30),
        "per second at 10 fps"
    );
    let l = log.lock().unwrap();
    let (init, reset) = (l.inited[0], l.reset[0]);
    assert_eq!(rest(&reset), rest(&init));
    assert_eq!(reset.mfx.max_kbps, reset.mfx.target_kbps);
    assert_eq!(
        kbps(&reset),
        per_picture,
        "12 Mbit/s at 10 fps, per picture at 30 fps"
    );
}

/// A rate change raises no IDR: with HRD conformance off a constant-bitrate encoder changes
/// its bitrate in the same coded sequence. An IDR comes only when asked.
#[test]
fn a_raised_rate_needs_no_idr() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut s = Session::new(Box::new(fake), &cfg(30_000)).unwrap();
    assert!(
        frame(&mut s, false).unwrap()[0].keyframe,
        "the first picture"
    );
    assert!(!frame(&mut s, false).unwrap()[0].keyframe);
    s.set_rate(RateConfig::new(24.0, 60_000, 30)).unwrap();
    assert!(
        !frame(&mut s, false).unwrap()[0].keyframe,
        "a raise, no IDR"
    );
    assert!(frame(&mut s, true).unwrap()[0].keyframe, "one asked for");
    let l = log.lock().unwrap();
    let idr = Some(ffi::FRAMETYPE_I | ffi::FRAMETYPE_REF | ffi::FRAMETYPE_IDR);
    assert_eq!(l.frame_types, vec![idr, None, None, idr]);
}

/// A Reset the runtime corrects is reported as what it corrected it to, from reading the
/// encoder back: not as the rate asked for, and not as a failure.
#[test]
fn a_corrected_rate_is_reported_as_the_one_in_force() {
    let (mut fake, log) = Fake::new(Codec::H264);
    fake.correct = Some(25_000);
    let mut s = Session::new(Box::new(fake), &cfg(2_000)).unwrap();
    s.set_rate(RateConfig::new(24.0, 40_000, 30)).unwrap();
    assert_eq!(s.rate_in_force(), RateInForce::Known(25_000));
    let ran = s.in_force().unwrap();
    assert_eq!((ran.target_kbps, ran.max_kbps), (25_000, 25_000));
    assert!(
        !frame(&mut s, false).unwrap().is_empty(),
        "the session goes on"
    );
    assert_eq!(
        kbps(&log.lock().unwrap().reset[0]),
        40_000,
        "what was asked"
    );
}

/// A Reset the runtime refuses (incompatible, invalid, or only partly in hardware) takes the
/// encoder out of use at once: no picture, rate change or Reset reaches it again, and it is
/// not opened again in place. Its error is not a stopped device, so the helper answers it as
/// an ordinary failure and the stream opens a fresh encoder for its next picture (moving on
/// to the next one if that keeps failing); Quick Sync is not given up.
#[test]
fn a_refused_reset_puts_the_encoder_out_of_use() {
    for refused in [INCOMPATIBLE_VIDEO_PARAM, -15, ffi::WRN_PARTIAL_ACCELERATION] {
        let (mut fake, log) = Fake::new(Codec::H264);
        fake.reset = VecDeque::from([refused]);
        let mut s = Session::new(Box::new(fake), &cfg(30_000)).unwrap();
        frame(&mut s, false).unwrap();
        let e = s.set_rate(RateConfig::new(24.0, 12_000, 30)).unwrap_err();
        let said = ffi::status_name(refused);
        assert!(
            e.0.contains("out of use") && e.0.contains(said) && !e.0.starts_with(STOPPED),
            "{e}"
        );
        assert!(s.stopped().is_none(), "the device still answers");
        assert_eq!(s.rate_in_force(), RateInForce::Unknown);
        let pictures = log.lock().unwrap().frame_types.len();
        assert_eq!(frame(&mut s, false).err(), Some(e.clone()), "no picture");
        assert_eq!(s.set_rate(RateConfig::new(24.0, 8_000, 30)).unwrap_err(), e);
        let l = log.lock().unwrap();
        assert_eq!(l.frame_types.len(), pictures, "nothing reached the runtime");
        assert_eq!((l.reset.len(), l.inited.len()), (1, 1));
    }
}

/// A rate the encoder's multiplier can't express needs a change Reset doesn't make: the
/// encoder goes out of use, and no Reset is sent.
#[test]
fn a_rate_beyond_the_multiplier_puts_the_encoder_out_of_use() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut s = Session::new(Box::new(fake), &cfg(2_000)).unwrap();
    let e = s.set_rate(RateConfig::new(24.0, 100_000, 30)).unwrap_err();
    assert!(
        e.0.contains("out of use") && e.0.contains("BRCParamMultiplier"),
        "{e}"
    );
    assert!(frame(&mut s, false).is_err());
    assert!(log.lock().unwrap().reset.is_empty());
}

/// Init leaves room in the multiplier for the most a Reset can ask: a first keyframe's boost
/// of a stripe's largest share, budgeted per picture at 10 fps.
#[test]
fn init_leaves_room_for_a_boosted_keyframe() {
    let (fake, _) = Fake::new(Codec::H264);
    let mut s = Session::new(Box::new(fake), &cfg(30_000)).unwrap();
    s.set_rate(keyframe_rate(24.0, 60_000, 10)).unwrap();
    assert_eq!(s.rate_in_force(), RateInForce::Known(239_998));
}

/// A frame rate the runtime reports as a fraction (30000/1001) is kept as one, wherever it
/// turns a rate asked per second at the stream's picture rate into TargetKbps and back.
#[test]
fn a_fractional_frame_rate_is_kept_as_a_fraction() {
    let (mut fake, log) = Fake::new(Codec::H264);
    fake.frame_rate = Some((30_000, 1_001));
    let mut s = Session::new(Box::new(fake), &cfg(2_000)).unwrap();
    s.set_rate(RateConfig::new(24.0, 12_000, 10)).unwrap();
    // 12000 kbit/s at 10 pictures a second is 1200 kbit a picture: 35964 kbit/s at
    // 30000/1001 (34800 if that were taken for 29).
    assert_eq!(kbps(&log.lock().unwrap().reset[0]), 35_964);
    assert_eq!(s.rate_in_force(), RateInForce::Known(12_000));
}

/// A rate change that fails, even after the runtime took the Reset (here the read-back
/// fails), leaves the rate unknown: never the one before it, never the one asked for.
#[test]
fn a_failed_rate_change_leaves_the_rate_unknown() {
    let (mut fake, _) = Fake::new(Codec::H264);
    fake.video_param = VecDeque::from([0, 0, 0, 0, -1]);
    let mut s = Session::new(Box::new(fake), &cfg(30_000)).unwrap();
    s.set_rate(RateConfig::new(24.0, 12_000, 30)).unwrap();
    assert_eq!(s.rate_in_force(), RateInForce::Known(at(12_000)));
    let e = s.set_rate(RateConfig::new(24.0, 6_000, 30)).unwrap_err();
    assert!(e.0.contains("GetVideoParam"), "{e}");
    assert_eq!(s.rate_in_force(), RateInForce::Unknown);
    assert!(frame(&mut s, false).is_err());
    assert!(s.set_rate(RateConfig::new(24.0, 8_000, 30)).is_err());
    assert_eq!(s.rate_in_force(), RateInForce::Unknown, "still unknown");
}

/// A Reset starts from what the runtime says the encoder runs with (GetVideoParam), which
/// may differ from what Init asked, and changes only the rate.
#[test]
fn a_reset_starts_from_what_the_encoder_runs_with() {
    let (mut fake, log) = Fake::new(Codec::H264);
    let rc = Rc::new(Codec::H264, &cfg(30_000).rate, None);
    let mut working = super::session::params(Codec::H264, (W, H), &rc, false);
    working.mfx.gop_pic_size = 300;
    working.mfx.target_usage = 4;
    fake.got = Some(working);
    let mut s = Session::new(Box::new(fake), &cfg(30_000)).unwrap();
    s.set_rate(RateConfig::new(24.0, 12_000, 30)).unwrap();
    let l = log.lock().unwrap();
    let r = l.reset[0].mfx;
    assert_eq!(
        (r.gop_pic_size, r.target_usage),
        (300, 4),
        "the runtime's values"
    );
    assert_eq!(kbps(&l.reset[0]), 12_000);
}
