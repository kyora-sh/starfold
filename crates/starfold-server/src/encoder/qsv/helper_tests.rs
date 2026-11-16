//! The helper process from both sides: the protocol, the helper's request loop against the
//! fake runtime, and (on Unix) the server's `Remote` against fake helpers, shell scripts
//! that print what a helper would and then hang, die or misbehave.

use super::helper::{serve, Frames, DONE, STOPPED_EXIT};
use super::proto::{Decoder, Open, Rate, Reply, Request, Unit, MAX_MESSAGE, MAX_UNITS};
use super::remote::slot_size;
use super::session::Session;
use super::tests::{bitstream, Fake};
use crate::encoder::{Codec, EncodeError, EncoderConfig};
use std::collections::VecDeque;

fn rate() -> Rate {
    Rate {
        max_kbps: 2000,
        buf_kbit: 83,
        fps: 30,
        quality: 24.0,
    }
}

fn open(codec: Codec, w: u32, h: u32) -> Open {
    Open {
        codec,
        width: w,
        height: h,
        fps: 30,
        rate: rate(),
        slot_size: slot_size(w, h) as u32,
        slots: 2,
        section: 7,
        luid: 0x1234,
    }
}

/// The bodies of the framed messages in `bytes`.
fn bodies(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut d = Decoder::default();
    d.push(bytes);
    let mut v = Vec::new();
    while let Some(b) = d.next().unwrap() {
        v.push(b);
    }
    v
}

#[test]
fn requests_and_replies_round_trip() {
    let reqs = [
        Request::Open(open(Codec::Hevc, 1920, 1080)),
        Request::Frame {
            slot: 1,
            pts: -5,
            key: true,
        },
        Request::Rate(rate()),
    ];
    for r in reqs {
        let b = bodies(&r.bytes());
        assert_eq!(b.len(), 1);
        assert_eq!(Request::parse(&b[0]), Ok(r));
    }
    let replies = [
        Reply::Opened {
            low_power: true,
            name: "mfx-gen".into(),
        },
        Reply::Done {
            slot: 0,
            units: vec![Unit {
                key: true,
                data: vec![0, 0, 0, 1, 0x65],
            }],
        },
        Reply::Done {
            slot: 1,
            units: Vec::new(),
        },
        Reply::RateSet,
        Reply::Error {
            stopped: true,
            reason: "Quick Sync stopped: x".into(),
        },
    ];
    for r in replies {
        let b = bodies(&r.bytes());
        assert_eq!(Reply::parse(&b[0]), Ok(r));
    }
}

#[test]
fn every_length_and_count_is_capped() {
    let mut d = Decoder::default();
    d.push(&((MAX_MESSAGE + 1) as u32).to_le_bytes());
    assert!(d.next().unwrap_err().contains("at most"));
    let mut d = Decoder::default();
    d.push(&[0, 0, 0, 0]);
    assert!(d.next().is_err(), "an empty message");
    let mut d = Decoder::default();
    d.push(&[9, 0, 0, 0, 0x82]);
    assert_eq!(d.next(), Ok(None), "waits for the rest");

    let units = vec![
        Unit {
            key: false,
            data: vec![1],
        };
        MAX_UNITS + 1
    ];
    let b = bodies(&Reply::Done { slot: 0, units }.bytes());
    assert!(Reply::parse(&b[0]).unwrap_err().contains("units"));
    let mut short = Reply::Done {
        slot: 0,
        units: vec![Unit {
            key: true,
            data: vec![1, 2, 3],
        }],
    }
    .bytes();
    short.truncate(short.len() - 1);
    let len = (short.len() - 4) as u32;
    short[..4].copy_from_slice(&len.to_le_bytes());
    assert!(Reply::parse(&bodies(&short)[0])
        .unwrap_err()
        .contains("short"));
    for junk in [vec![0x99], vec![0x84, 2, 0, 0], vec![0x83, 0]] {
        assert!(Reply::parse(&junk).is_err(), "{junk:?}");
    }
    let mut long_text = vec![0x84, 1];
    long_text.extend_from_slice(&u16::MAX.to_le_bytes());
    assert!(Reply::parse(&long_text).unwrap_err().contains("text"));
}

struct Pictures(Vec<u8>);

impl Frames for Pictures {
    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

fn session_on(
    fake: Fake,
) -> impl FnOnce(&EncoderConfig, u64) -> Result<(Session, String), EncodeError> {
    move |cfg, luid| {
        assert_eq!(luid, 0x1234);
        Ok((Session::new(Box::new(fake), cfg)?, "fake-gen".into()))
    }
}

fn grey(o: &Open) -> Box<dyn Frames> {
    Box::new(Pictures(vec![128; o.slot_size as usize * o.slots as usize]))
}

#[test]
fn the_helper_serves_a_session() {
    let (fake, _) = Fake::new(Codec::H264);
    let o = open(Codec::H264, 640, 480);
    let mut input = Request::Open(o).bytes();
    for (slot, key) in [(0, false), (1, false), (0, true)] {
        input.extend(
            Request::Frame {
                slot,
                pts: slot as i64,
                key,
            }
            .bytes(),
        );
    }
    input.extend(
        Request::Rate(Rate {
            max_kbps: 5000,
            ..rate()
        })
        .bytes(),
    );
    let mut out = Vec::new();
    let code = serve(
        input.as_slice(),
        &mut out,
        |o| Ok(grey(o)),
        session_on(fake),
    );
    assert_eq!(code, DONE, "stdin closed");
    let replies: Vec<Reply> = bodies(&out)
        .iter()
        .map(|b| Reply::parse(b).unwrap())
        .collect();
    assert_eq!(replies.len(), 5, "{replies:?}");
    assert_eq!(
        replies[0],
        Reply::Opened {
            low_power: true,
            name: "fake-gen".into()
        }
    );
    let keys: Vec<(u8, bool)> = replies[1..4]
        .iter()
        .map(|r| match r {
            Reply::Done { slot, units } => (*slot, units[0].key),
            r => panic!("{r:?}"),
        })
        .collect();
    assert_eq!(keys, vec![(0, true), (1, false), (0, true)]);
    if let Reply::Done { units, .. } = &replies[1] {
        assert_eq!(units[0].data, bitstream(Codec::H264, true, true));
    }
    assert_eq!(replies[4], Reply::RateSet);
}

#[test]
fn a_stopped_device_is_reported_and_the_helper_leaves_without_closing_it() {
    let (mut fake, log) = Fake::new(Codec::H264);
    fake.sync = VecDeque::from([ffi_in_execution()]);
    let o = open(Codec::H264, 640, 480);
    let mut input = Request::Open(o).bytes();
    for slot in [0, 1] {
        input.extend(
            Request::Frame {
                slot,
                pts: 0,
                key: false,
            }
            .bytes(),
        );
    }
    let mut out = Vec::new();
    let code = serve(
        input.as_slice(),
        &mut out,
        |o| Ok(grey(o)),
        session_on(fake),
    );
    assert_eq!(code, STOPPED_EXIT);
    let replies: Vec<Reply> = bodies(&out)
        .iter()
        .map(|b| Reply::parse(b).unwrap())
        .collect();
    assert_eq!(replies.len(), 2, "nothing after the stop: {replies:?}");
    match &replies[1] {
        Reply::Error { stopped, reason } => {
            assert!(
                *stopped && reason.contains("did not finish a frame"),
                "{reason}"
            );
        }
        r => panic!("{r:?}"),
    }
    let l = log.lock().unwrap();
    assert!(!l.dropped, "the runtime is not closed");
    assert_eq!(l.live, 1, "nor the frame's surface released");
}

fn ffi_in_execution() -> i32 {
    super::ffi::WRN_IN_EXECUTION
}

#[test]
fn the_helper_refuses_what_it_can_not_serve() {
    let (fake, _) = Fake::new(Codec::H264);
    let frame_first = Request::Frame {
        slot: 0,
        pts: 0,
        key: false,
    }
    .bytes();
    let mut out = Vec::new();
    assert_eq!(
        serve(
            frame_first.as_slice(),
            &mut out,
            |o| Ok(grey(o)),
            session_on(fake)
        ),
        super::helper::BAD_REQUEST
    );

    let (fake, _) = Fake::new(Codec::H264);
    let mut o = open(Codec::H264, 640, 480);
    o.slot_size -= 1;
    let mut out = Vec::new();
    let code = serve(
        Request::Open(o).bytes().as_slice(),
        &mut out,
        |o| Ok(grey(o)),
        session_on(fake),
    );
    assert_eq!(code, super::helper::BAD_REQUEST);
    assert!(matches!(
        Reply::parse(&bodies(&out)[0]),
        Ok(Reply::Error { stopped: false, .. })
    ));

    let (fake, _) = Fake::new(Codec::H264);
    let o = open(Codec::H264, 640, 480);
    let mut input = Request::Open(o).bytes();
    input.extend(
        Request::Frame {
            slot: 2,
            pts: 0,
            key: false,
        }
        .bytes(),
    );
    let mut out = Vec::new();
    let code = serve(
        input.as_slice(),
        &mut out,
        |o| Ok(grey(o)),
        session_on(fake),
    );
    assert_eq!(
        code,
        super::helper::BAD_REQUEST,
        "a slot out of the section"
    );
}

#[cfg(unix)]
mod remote {
    use super::super::proc::{self, write_by, Pipes, Proc, Unwritten};
    use super::super::proto::{Reply, Unit};
    use super::super::remote::{Policy, Remote, Timing, TIMING};
    use super::super::tests::bitstream;
    use crate::encoder::hw::sample_config;
    use crate::encoder::{
        Backend, Codec, EncodeOptions, EncoderConfig, FrameView, RateConfig, VideoEncoder,
    };
    use std::process::Command;
    use std::time::{Duration, Instant};

    const QUICK: Timing = Timing {
        open: Duration::from_secs(5),
        frame: Duration::from_millis(400),
        grace: Duration::from_secs(1),
    };

    fn policy(t: Timing) -> &'static Policy {
        Box::leak(Box::new(Policy::new(t)))
    }

    /// A shell `printf` of `r` as the helper would write it.
    fn says(r: &Reply) -> String {
        let octal: String = r.bytes().iter().map(|b| format!("\\{b:03o}")).collect();
        format!("printf '{octal}'")
    }

    fn opened() -> String {
        says(&Reply::Opened {
            low_power: true,
            name: "fake-gen".into(),
        })
    }

    fn done(slot: u8, key: bool) -> String {
        says(&Reply::Done {
            slot,
            units: vec![Unit {
                key,
                data: bitstream(Codec::H264, key, true),
            }],
        })
    }

    fn helper(script: String) -> impl FnOnce() -> Result<(Proc, Pipes), String> {
        move || {
            let mut c = Command::new("sh");
            c.arg("-c").arg(script);
            proc::spawn(c)
        }
    }

    fn cfg() -> EncoderConfig {
        let mut c = sample_config(Backend::Qsv, Codec::H264, 64, 48, 30);
        c.rate = RateConfig::new(24.0, 2000, 30);
        c
    }

    fn start(p: &'static Policy, script: String) -> Remote {
        Remote::open(p, helper(script), &cfg(), 0x1234).unwrap()
    }

    fn frame(
        enc: &mut Remote,
        pts: i64,
    ) -> Result<Vec<crate::encoder::AccessUnit>, crate::encoder::EncodeError> {
        let (y, uv) = (vec![16u8; 64 * 48], vec![128u8; 32 * 24]);
        let f = FrameView {
            width: 64,
            height: 48,
            y: &y,
            u: &uv,
            v: &uv,
            stride_y: 64,
            stride_uv: 32,
        };
        enc.encode(
            &f,
            EncodeOptions {
                pts_ms: pts,
                ..Default::default()
            },
        )
    }

    fn alive(pid: u32) -> bool {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    /// Encodes until an error, for at most `within`.
    fn fails_within(enc: &mut Remote, within: Duration) -> String {
        let end = Instant::now() + within;
        let mut pts = 100;
        loop {
            match frame(enc, pts) {
                Err(e) => return e.0,
                Ok(_) if Instant::now() < end => std::thread::sleep(Duration::from_millis(20)),
                Ok(_) => panic!("no error within {within:?}"),
            }
            pts += 1;
        }
    }

    /// Quick Sync is off in `p` with a reason containing `said`, `pid` is gone, and no new
    /// helper starts.
    fn given_up(p: &'static Policy, pid: u32, said: &str) {
        let off = p.off().expect("Quick Sync off");
        assert!(
            off.starts_with("qsv given up: ") && off.contains(said),
            "{off}"
        );
        assert!(!alive(pid), "helper {pid} killed and reaped");
        let spawned = p.spawned();
        let again = Remote::open(p, helper(opened()), &cfg(), 0x1234);
        assert_eq!(again.err().map(|e| e.0), Some(off));
        assert_eq!(p.spawned(), spawned, "nothing starts after the give-up");
    }

    #[test]
    fn a_helper_that_hangs_on_a_frame_is_killed_at_the_deadline() {
        let p = policy(QUICK);
        let mut enc = start(p, format!("{}; sleep 30", opened()));
        assert_eq!(enc.name, "fake-gen");
        let pid = enc.pid();
        let t = Instant::now();
        assert!(frame(&mut enc, 1).unwrap().is_empty(), "still in flight");
        while (p.off().is_none() || alive(pid)) && t.elapsed() < QUICK.frame + QUICK.grace * 2 {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            t.elapsed() < QUICK.frame + QUICK.grace + Duration::from_millis(500),
            "killed and reaped after {:?}",
            t.elapsed()
        );
        given_up(p, pid, "missed its deadline (a frame)");
        let e = frame(&mut enc, 2).err().unwrap();
        assert!(e.0.starts_with("qsv given up"), "{e}");
    }

    #[test]
    fn a_device_hang_reported_by_the_helper_gives_quick_sync_up() {
        let p = policy(QUICK);
        let stop = says(&Reply::Error {
            stopped: true,
            reason: "Quick Sync stopped: MFXVideoCORE_SyncOperation failed: MFX_ERR_GPU_HANG (-21)"
                .into(),
        });
        let other = start(p, format!("{}; sleep 30", opened()));
        let mut enc = start(p, format!("{}; sleep 0.3; {stop}; sleep 30", opened()));
        let (pid, other_pid) = (enc.pid(), other.pid());
        let e = fails_within(&mut enc, Duration::from_secs(2));
        assert!(e.contains("MFX_ERR_GPU_HANG"), "{e}");
        given_up(p, pid, "MFX_ERR_GPU_HANG");
        assert!(!alive(other_pid), "every helper goes");
    }

    #[test]
    fn one_death_is_survived_a_second_gives_up() {
        let p = policy(QUICK);
        let mut first = start(p, format!("{}; sleep 0.1; exit 1", opened()));
        let pid = first.pid();
        let e = fails_within(&mut first, Duration::from_secs(2));
        assert!(
            ["exited", "closed its stdout", "could not be written to"]
                .iter()
                .any(|how| e.contains(how)),
            "{e}"
        );
        assert!(p.off().is_none(), "one death");
        assert!(!alive(pid));
        let mut second = start(p, format!("{}; kill -s SEGV $$", opened()));
        let pid = second.pid();
        fails_within(&mut second, Duration::from_secs(2));
        given_up(p, pid, "a second helper failed");
    }

    #[test]
    fn crashes_are_deaths_too() {
        let p = policy(QUICK);
        for _ in 0..2 {
            let mut enc = start(p, format!("{}; sleep 0.05; kill -s SEGV $$", opened()));
            let pid = enc.pid();
            fails_within(&mut enc, Duration::from_secs(2));
            assert!(!alive(pid));
        }
        assert!(p.off().unwrap().contains("a second helper failed"));
    }

    #[test]
    fn garbage_and_oversize_replies_end_the_helper() {
        for junk in [
            "\\004\\000\\000\\000\\231\\001\\002\\003",
            "\\377\\377\\377\\177",
        ] {
            let p = policy(QUICK);
            let mut enc = start(p, format!("{}; printf '{junk}'; sleep 30", opened()));
            let pid = enc.pid();
            let e = fails_within(&mut enc, Duration::from_secs(2));
            assert!(e.contains("bad reply"), "{e}");
            assert!(!alive(pid), "killed and reaped");
            assert!(p.off().is_none(), "counted as one death");
        }
    }

    #[test]
    fn a_helper_that_stops_reading_can_not_block_a_write() {
        let mut c = Command::new("sh");
        c.arg("-c").arg("sleep 30");
        let (p, mut pipes) = proc::spawn(c).unwrap();
        let t = Instant::now();
        let big = vec![0u8; 4 << 20];
        let e = write_by(&mut *pipes.tx, &big, t + Duration::from_millis(300));
        assert_eq!(e, Err(Unwritten::Late));
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(p.end(Duration::from_secs(1)));
        assert!(!alive(p.id()));
    }

    #[test]
    fn two_frames_in_flight_then_only_the_newest_waits() {
        let p = policy(TIMING);
        let script = format!(
            "{}; sleep 0.4; {}; sleep 0.05; {}; sleep 0.05; {}; sleep 30",
            opened(),
            done(0, true),
            done(1, false),
            done(0, false)
        );
        let mut enc = start(p, script);
        for pts in 1..=4 {
            assert!(frame(&mut enc, pts).unwrap().is_empty(), "frame {pts}");
        }
        std::thread::sleep(Duration::from_millis(500));
        let aus = enc.drain().unwrap();
        let got: Vec<(i64, bool)> = aus.iter().map(|a| (a.pts_ms, a.keyframe)).collect();
        assert_eq!(
            got,
            vec![(1, true), (2, false), (4, false)],
            "frame 3 skipped"
        );
        assert_eq!(
            enc.sent,
            vec![(0, 1, false), (1, 2, false), (0, 4, false)],
            "no IDR asked for the frame after the skip"
        );
        assert!(aus[0].config.is_some());
        assert!(p.off().is_none());
    }
}
