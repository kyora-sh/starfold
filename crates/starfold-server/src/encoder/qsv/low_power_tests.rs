//! Which encoder a Quick Sync session opens on: the full one (LowPower off) first, the
//! low-power one (VDEnc) only if the runtime refuses that, against the fake runtime.

use super::driver::STOPPED;
use super::ffi::{self, Status, VideoParam};
use super::session::Session;
use super::tests::Fake;
use crate::encoder::hw::sample_config;
use crate::encoder::{Backend, Codec, EncoderConfig, RateConfig};
use std::collections::VecDeque;

const OFF: u16 = ffi::CODINGOPTION_OFF;
const ON: u16 = ffi::CODINGOPTION_ON;
const UNSUPPORTED: Status = -3;
const INVALID_PARAM: Status = -15;

fn cfg() -> EncoderConfig {
    let mut c = sample_config(Backend::Qsv, Codec::H264, 640, 480, 30);
    c.rate = RateConfig::new(24.0, 2000, 30);
    c
}

fn modes(v: &[VideoParam]) -> Vec<u16> {
    v.iter().map(|p| p.mfx.low_power).collect()
}

/// A session on a runtime whose Query and Init answer `query` and `init` in turn: the
/// session, and the LowPower settings Query and Init were asked for.
fn open(query: &[Status], init: &[Status]) -> (Result<Session, String>, Vec<u16>, Vec<u16>) {
    let (mut fake, log) = Fake::new(Codec::H264);
    fake.query = VecDeque::from(query.to_vec());
    fake.init = VecDeque::from(init.to_vec());
    let s = Session::new(Box::new(fake), &cfg()).map_err(|e| e.0);
    let l = log.lock().unwrap();
    (s, modes(&l.queried), modes(&l.inited))
}

#[test]
fn a_runtime_that_takes_both_runs_the_full_encoder() {
    let (s, queried, inited) = open(&[], &[]);
    assert!(!s.unwrap().low_power, "LowPower off");
    assert_eq!((queried, inited), (vec![OFF], vec![OFF]));
}

#[test]
fn low_power_only_when_the_full_encoder_is_refused() {
    for (query, init, want_queried, want_inited) in [
        (vec![UNSUPPORTED], vec![], vec![OFF, ON], vec![ON]),
        (vec![], vec![INVALID_PARAM], vec![OFF, ON], vec![OFF, ON]),
    ] {
        let (s, queried, inited) = open(&query, &init);
        let s = s.unwrap();
        assert!(s.low_power, "LowPower on after {query:?} / {init:?}");
        assert_eq!((queried, inited), (want_queried, want_inited));
    }
}

#[test]
fn a_runtime_that_refuses_both_is_told_with_both_reasons() {
    let (s, _, _) = open(&[UNSUPPORTED, UNSUPPORTED], &[]);
    let e = s.err().unwrap();
    assert!(
        e.contains("LowPower off: MFXVideoENCODE_Query: MFX_ERR_UNSUPPORTED")
            && e.contains("LowPower on: MFXVideoENCODE_Query: MFX_ERR_UNSUPPORTED"),
        "{e}"
    );
    let (s, _, inited) = open(&[], &[INVALID_PARAM, INVALID_PARAM]);
    let e = s.err().unwrap();
    let name = ffi::status_name(INVALID_PARAM);
    assert!(
        e.contains(&format!("LowPower off: MFXVideoENCODE_Init: {name}"))
            && e.contains(&format!("LowPower on: MFXVideoENCODE_Init: {name}")),
        "{e}"
    );
    assert_eq!(inited, vec![OFF, ON]);
}

#[test]
fn a_stopped_device_on_the_first_try_is_not_tried_again() {
    for status in [
        ffi::ERR_DEVICE_LOST,
        ffi::ERR_DEVICE_FAILED,
        ffi::ERR_GPU_HANG,
    ] {
        let (s, queried, inited) = open(&[status], &[]);
        assert!(s.err().unwrap().starts_with(STOPPED));
        assert_eq!((queried.len(), inited.len()), (1, 0), "Query {status}");
        let (s, queried, inited) = open(&[], &[status]);
        assert!(s.err().unwrap().starts_with(STOPPED));
        assert_eq!((queried.len(), inited.len()), (1, 1), "Init {status}");
    }
}
