//! Rate changes on a constant-bitrate Quick Sync session, against the fake runtime, which
//! refuses one by Reset unless Init turned HRD conformance off (as oneVPL documents it): the
//! stream's first rate change right after open, its steady-state ones, what a Reset changes
//! and starts from, the ceiling it is held to, and what takes the encoder out of use.

use super::driver::STOPPED;
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
/// change goes through Reset, with pictures in between.
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
    for r in rates {
        s.set_rate(r).unwrap();
        frame(&mut s, false).unwrap();
    }
    let want = [12_000, 6_000, 24_000, 20_000, 8_000].map(at);
    let l = log.lock().unwrap();
    let asked: Vec<u32> = l.reset.iter().map(kbps).collect();
    assert_eq!(asked[0], at(64_000));
    assert_eq!(asked[1..], want);
    assert_eq!(l.inited.len(), 1, "no encoder opened again");
}

/// A Reset changes TargetKbps and MaxKbps only: the buffer, the initial delay, the
/// multiplier and the frame rate stay as the encoder runs them. A stream taking pictures at
/// another rate than the encoder's gets the same budget per picture through TargetKbps.
#[test]
fn a_reset_changes_the_bitrate_only() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut s = Session::new(Box::new(fake), &cfg(30_000)).unwrap();
    s.set_rate(RateConfig::new(24.0, 12_000, 10)).unwrap();
    let l = log.lock().unwrap();
    let (init, reset) = (l.inited[0], l.reset[0]);
    assert_eq!(rest(&reset), rest(&init));
    assert_eq!(reset.mfx.max_kbps, reset.mfx.target_kbps);
    assert_eq!(
        kbps(&reset),
        at(12_000 * 30 / 10),
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

/// A Reset the runtime takes but corrects (answering MFX_WRN_INCOMPATIBLE_VIDEO_PARAM) is no
/// failure when what it reads back afterwards is still the encoder asked for.
#[test]
fn a_corrected_rate_is_no_failure() {
    let (mut fake, log) = Fake::new(Codec::H264);
    fake.correct = Some(25_000);
    let mut s = Session::new(Box::new(fake), &cfg(2_000)).unwrap();
    s.set_rate(RateConfig::new(24.0, 40_000, 30)).unwrap();
    assert!(
        !frame(&mut s, false).unwrap().is_empty(),
        "the session goes on"
    );
    assert!(s.holds_cap(), "constant bitrate, read back");
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
        assert!(!s.holds_cap(), "what it runs is unknown");
        let pictures = log.lock().unwrap().frame_types.len();
        assert_eq!(frame(&mut s, false).err(), Some(e.clone()), "no picture");
        assert_eq!(s.set_rate(RateConfig::new(24.0, 8_000, 30)).unwrap_err(), e);
        let l = log.lock().unwrap();
        assert_eq!(l.frame_types.len(), pictures, "nothing reached the runtime");
        assert_eq!((l.reset.len(), l.inited.len()), (1, 1));
    }
}

/// A rate above what the session was sized for is clamped to it, and the encoder stays in
/// use.
#[test]
fn a_rate_above_the_ceiling_is_clamped_to_it() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut s = Session::new(Box::new(fake), &cfg(2_000)).unwrap();
    s.set_rate(RateConfig::new(24.0, 100_000, 30)).unwrap();
    assert_eq!(
        kbps(&log.lock().unwrap().reset[0]),
        48_000,
        "a 2 Mbit/s cap's boosted keyframe at 10 fps, per picture at 30"
    );
    assert!(!frame(&mut s, false).unwrap().is_empty(), "still in use");
}

/// The rates a stream asks while it ramps up and boosts a keyframe, at 22 pictures a second
/// on an encoder opened at 30: what the box asked of one opened at a 30 Mbit/s cap.
fn the_box_ramp() -> [RateConfig; 5] {
    [
        RateConfig::new(24.0, 16_000, 22),
        RateConfig::new(24.0, 24_000, 22),
        RateConfig::new(24.0, 45_616, 22),
        RateConfig::new(24.0, 66_207, 22),
        keyframe_rate(24.0, 60_000, 22),
    ]
}

/// On a runtime that reports the smallest multiplier that holds the rates (Intel's H.264
/// runtime turned the 11 Init asked into 1), each rate goes in at that multiplier: one above
/// what it holds (65535 kbit/s per picture) is clamped there, and the session stays in use.
#[test]
fn a_runtime_that_normalizes_the_multiplier_clamps_at_what_it_holds() {
    let (mut fake, log) = Fake::new(Codec::H264);
    fake.normalize = true;
    let mut s = Session::new(Box::new(fake), &cfg(30_000)).unwrap();
    for r in the_box_ramp() {
        s.set_rate(r).unwrap();
    }
    let l = log.lock().unwrap();
    assert!(l.reset.iter().all(|p| p.mfx.brc_param_multiplier == 1));
    let asked: Vec<u32> = l.reset.iter().map(kbps).collect();
    assert_eq!(
        asked,
        [21_818, 32_727, 62_204, 65_535, 65_535],
        "per picture at 30 fps; 90282 and 327273 held to 65535"
    );
}

/// On a runtime that keeps the multiplier Init asked (HEVC's did), the same ramp and boost
/// go in as asked, within the multiplier's steps.
#[test]
fn a_runtime_that_keeps_the_multiplier_runs_the_ramp_as_asked() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut s = Session::new(Box::new(fake), &cfg(30_000)).unwrap();
    for r in the_box_ramp() {
        s.set_rate(r).unwrap();
    }
    let l = log.lock().unwrap();
    assert!(l.reset.iter().all(|p| p.mfx.brc_param_multiplier == 11));
    for (r, p) in the_box_ramp().iter().zip(&l.reset) {
        let per_picture = (r.max_kbps as u64 * 60 + 22) / 44;
        let k = kbps(p) as u64;
        assert!(
            per_picture.abs_diff(k) < 11,
            "asked {per_picture} a picture, Reset {k}"
        );
    }
}

/// Init leaves room in the multiplier for the most a Reset can ask: a first keyframe's boost
/// of a stripe's largest share, budgeted per picture at 10 fps.
#[test]
fn init_leaves_room_for_a_boosted_keyframe() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut s = Session::new(Box::new(fake), &cfg(30_000)).unwrap();
    s.set_rate(keyframe_rate(24.0, 60_000, 10)).unwrap();
    assert_eq!(kbps(&log.lock().unwrap().reset[0]), 719_994);
}

/// A frame rate the runtime reports as a fraction (30000/1001) is kept as one where it turns
/// a rate asked per second at the stream's picture rate into TargetKbps.
#[test]
fn a_fractional_frame_rate_is_kept_as_a_fraction() {
    let (mut fake, log) = Fake::new(Codec::H264);
    fake.frame_rate = Some((30_000, 1_001));
    let mut s = Session::new(Box::new(fake), &cfg(2_000)).unwrap();
    s.set_rate(RateConfig::new(24.0, 12_000, 10)).unwrap();
    // 12000 kbit/s at 10 pictures a second is 1200 kbit a picture: 35964 kbit/s at
    // 30000/1001 (34800 if that were taken for 29).
    assert_eq!(kbps(&log.lock().unwrap().reset[0]), 35_964);
}

/// A rate change that fails after the runtime took the Reset (here the read-back fails)
/// takes the encoder out of use, and it no longer counts as holding its cap.
#[test]
fn a_rate_change_whose_read_back_fails_takes_the_encoder_out_of_use() {
    let (mut fake, _) = Fake::new(Codec::H264);
    fake.video_param = VecDeque::from([0, 0, 0, 0, -1]);
    let mut s = Session::new(Box::new(fake), &cfg(30_000)).unwrap();
    s.set_rate(RateConfig::new(24.0, 12_000, 30)).unwrap();
    assert!(s.holds_cap());
    let e = s.set_rate(RateConfig::new(24.0, 6_000, 30)).unwrap_err();
    assert!(e.0.contains("GetVideoParam"), "{e}");
    assert!(!s.holds_cap());
    assert!(frame(&mut s, false).is_err());
    assert!(s.set_rate(RateConfig::new(24.0, 8_000, 30)).is_err());
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

/// A frame rate the runtime leaves undefined (FrameRateExtD 0, or FrameRateExtN 0) is not
/// made into a number: a rate change, which would have to scale by it, takes the encoder
/// out of use, and no Reset goes out.
#[test]
fn an_undefined_frame_rate_takes_no_rate_change() {
    for (n, d) in [(30_000, 0), (0, 1)] {
        let (mut fake, log) = Fake::new(Codec::H264);
        fake.frame_rate = Some((n, d));
        let mut s = Session::new(Box::new(fake), &cfg(30_000)).unwrap();
        let e = s.set_rate(RateConfig::new(24.0, 12_000, 30)).unwrap_err();
        assert!(e.0.contains("no defined frame rate"), "{n}/{d}: {e}");
        assert!(
            log.lock().unwrap().reset.is_empty(),
            "no Reset scaled by it"
        );
        assert!(frame(&mut s, false).is_err(), "out of use");
    }
}

/// MFX_WRN_INCOMPATIBLE_VIDEO_PARAM: the runtime took the call and changed what it asked.
const WRN_INCOMPATIBLE_VIDEO_PARAM: i32 = 5;

/// A constant-bitrate Init the runtime takes but leaves HRD conformance on (it corrected
/// it back, answering MFX_WRN_INCOMPATIBLE_VIDEO_PARAM), or doesn't say, is not a session
/// to keep: its bitrate could not change through Reset. Constant QP needs no Reset of the
/// bitrate, so it opens either way.
#[test]
fn an_init_that_leaves_hrd_conformance_on_opens_no_session() {
    for (hrd, says) in [(ffi::CODINGOPTION_ON, "on"), (0, "unconfirmed")] {
        let (mut fake, _) = Fake::new(Codec::H264);
        fake.init = VecDeque::from([WRN_INCOMPATIBLE_VIDEO_PARAM]);
        fake.hrd_report = Some(hrd);
        let e = Session::new(Box::new(fake), &cfg(30_000)).err().unwrap();
        assert!(e.0.contains(&format!("HRD conformance {says}")), "{e}");
    }
    let (mut fake, _) = Fake::new(Codec::H264);
    fake.hrd_report = Some(ffi::CODINGOPTION_ON);
    let s = Session::new(Box::new(fake), &cfg(0)).unwrap();
    assert!(!s.holds_cap(), "constant QP");
}

/// A Reset the runtime takes but that leaves HRD conformance reading back on takes the
/// encoder out of use: the next change would be refused.
#[test]
fn a_reset_that_leaves_hrd_conformance_on_takes_the_encoder_out_of_use() {
    let (mut fake, log) = Fake::new(Codec::H264);
    fake.hrd_after_reset = Some(ffi::CODINGOPTION_ON);
    let mut s = Session::new(Box::new(fake), &cfg(30_000)).unwrap();
    let e = s.set_rate(RateConfig::new(24.0, 12_000, 30)).unwrap_err();
    assert!(e.0.contains("HRD conformance on"), "{e}");
    assert_eq!(log.lock().unwrap().reset.len(), 1, "the Reset went through");
    assert!(!s.holds_cap());
    assert!(frame(&mut s, false).is_err(), "out of use");
}

/// What Init asks for a 30 Mbit/s cap, with `change` applied.
fn asked(change: impl FnOnce(&mut VideoParam)) -> VideoParam {
    let rc = Rc::new(Codec::H264, &cfg(30_000).rate, None);
    let mut p = super::session::params(Codec::H264, (W, H), &rc, false);
    change(&mut p);
    p
}

/// What GetVideoParam says after Init: what Init asked, with `change` applied.
fn reads_back(change: impl FnOnce(&mut VideoParam)) -> Fake {
    let (mut fake, _) = Fake::new(Codec::H264);
    fake.got = Some(asked(change));
    fake
}

/// An Init the runtime takes but that runs another rate control method than the one asked
/// (constant bitrate turned into a variable one) opens no session.
#[test]
fn an_init_that_runs_another_rate_control_method_opens_no_session() {
    let fake = reads_back(|p| p.mfx.rate_control_method = 2);
    let e = Session::new(Box::new(fake), &cfg(30_000)).err().unwrap();
    assert!(e.0.contains("rate control method 2"), "{e}");
}

/// An Init that codes another picture size or format, or another codec, than the one asked
/// opens no session.
#[test]
fn an_init_at_another_picture_size_opens_no_session() {
    let fake = reads_back(|p| p.mfx.frame_info.crop_h = 1088);
    let e = Session::new(Box::new(fake), &cfg(30_000)).err().unwrap();
    assert!(e.0.contains("at 1920x1088, not NV12 at 1920x1080"), "{e}");
    let fake = reads_back(|p| p.mfx.codec_id = ffi::CODEC_HEVC);
    let e = Session::new(Box::new(fake), &cfg(30_000)).err().unwrap();
    assert!(e.0.contains("runs codec"), "{e}");
}

/// What the encoder reads back before a rate change is held to what Init's was: one that
/// now runs another rate control method, or holds HRD conformance again, takes no Reset and
/// is out of use.
#[test]
fn a_session_read_back_otherwise_before_a_rate_change_takes_no_reset() {
    let (mut fake, log) = Fake::new(Codec::H264);
    fake.got_from = Some((2, asked(|p| p.mfx.rate_control_method = 2)));
    let mut s = Session::new(Box::new(fake), &cfg(30_000)).unwrap();
    let e = s.set_rate(RateConfig::new(24.0, 12_000, 30)).unwrap_err();
    assert!(
        e.0.contains("out of use")
            && e.0.contains("before a rate change")
            && e.0.contains("rate control method 2"),
        "{e}"
    );
    assert!(log.lock().unwrap().reset.is_empty(), "no Reset");
    assert!(!s.holds_cap());
    assert!(frame(&mut s, false).is_err(), "out of use");
}
