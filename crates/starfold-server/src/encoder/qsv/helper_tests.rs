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
            low_power: false,
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

#[test]
fn a_helper_is_created_in_its_job_inheriting_only_its_three_handles() {
    use super::proc::{attributes, HANDLE_LIST, JOB_LIST};
    assert_eq!((HANDLE_LIST, JOB_LIST), (0x0002_0002, 0x0002_000D));
    let (inherit, job) = ([1usize, 2, 3], [9usize]);
    let a = attributes(&inherit, &job);
    let ids: Vec<usize> = a.iter().map(|x| x.0).collect();
    assert_eq!(ids, vec![HANDLE_LIST, JOB_LIST], "both, in one list");
    let w = std::mem::size_of::<usize>();
    assert_eq!((a[0].1, a[0].2), (inherit.as_ptr(), 3 * w));
    assert_eq!((a[1].1, a[1].2), (job.as_ptr(), w));
}

#[test]
fn stats_say_why_quick_sync_is_off_as_it_stands_now() {
    use crate::encoder::{turned_off, with_live, Backend};
    let snapshot = serde_json::json!({
        "mode": "qsv",
        "chosen": [],
        "failed": [
            {"codec": "h264", "backend": "qsv", "reason": "qsv given up: helper 7 missed its deadline (a frame)"},
            {"codec": "h264", "backend": "x264", "reason": "keeps failing"},
        ],
    });
    let now = "qsv given up: helper 7 missed its deadline (a frame); 1 helper(s) did not exit";
    let e = with_live(&snapshot, &[(Backend::Qsv, now.into())]);
    assert_eq!(e["failed"][0]["reason"], now);
    assert_eq!(e["failed"][1]["reason"], "keeps failing");
    assert_eq!(
        e["off"],
        serde_json::json!([{"backend": "qsv", "reason": now}])
    );
    assert_eq!(e["chosen"], snapshot["chosen"]);
    let quiet = with_live(&snapshot, &[]);
    assert_eq!(quiet["failed"], snapshot["failed"]);
    assert_eq!(quiet["off"], serde_json::json!([]));
    assert!(turned_off().is_empty(), "no Quick Sync on this platform");
}

#[cfg(unix)]
mod remote {
    use super::super::probe::{self, luid_of, Verified};
    use super::super::proc::{self, end_all, write_by, NotStarted, Pipes, Proc, Unwritten};
    use super::super::proto::{Reply, Unit};
    use super::super::remote::{Policy, Remote, Timing, TIMING};
    use super::super::tests::bitstream;
    use crate::encoder::caps::{Acceleration, EncoderCapability, ProbeProvider, ProbeStatus};
    use crate::encoder::hw::sample_config;
    use crate::encoder::isolate::{Report, VERSION};
    use crate::encoder::{
        Backend, Codec, EncodeOptions, EncoderConfig, FrameView, RateConfig, VideoEncoder,
    };
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;
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

    fn helper(script: String) -> impl FnOnce() -> Result<(Proc, Pipes), NotStarted> {
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

    /// Quick Sync is off in `p` with a reason containing `said`, `pid` is gone (reaped
    /// within the grace, by whichever thread gave up), and no new helper starts.
    fn given_up(p: &'static Policy, pid: u32, said: &str) {
        let off = p.off().expect("Quick Sync off");
        assert!(
            off.starts_with("qsv given up: ") && off.contains(said),
            "{off}"
        );
        let t = Instant::now();
        while alive(pid) && t.elapsed() < QUICK.grace {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!alive(pid), "helper {pid} killed and reaped");
        let spawned = p.spawned();
        let again = Remote::open(p, helper(opened()), &cfg(), 0x1234);
        assert_eq!(again.err().map(|e| e.0), Some(off));
        assert_eq!(p.spawned(), spawned, "nothing starts after the give-up");
    }

    /// The lines logged about helper `pid`, in order.
    fn lines_of(p: &'static Policy, pid: u32) -> Vec<String> {
        let me = format!("qsv helper {pid}: ");
        p.said().into_iter().filter(|l| l.starts_with(&me)).collect()
    }

    /// Helper `pid` was logged opening, then ending once, with an end line starting `ended`.
    fn opened_then(p: &'static Policy, pid: u32, ended: &[&str]) {
        let l = lines_of(p, pid);
        let open = format!("qsv helper {pid}: opened fake-gen for H.264, LowPower on");
        assert_eq!(l.len(), 2, "one open line, one end line: {l:?}");
        assert_eq!(l[0], open);
        let end = format!("qsv helper {pid}: ended: ");
        assert!(
            ended.iter().any(|e| l[1].starts_with(&format!("{end}{e}"))),
            "{l:?}"
        );
    }

    /// A helper that opens with LowPower on, then does `then`.
    fn opens_and(then: &str) -> String {
        format!("{}; {then}", opened())
    }

    #[test]
    fn the_open_line_names_the_codec_and_the_mode() {
        for (low_power, mode) in [(false, "off"), (true, "on")] {
            let p = policy(QUICK);
            let opened = says(&Reply::Opened {
                low_power,
                name: "fake-gen".into(),
            });
            let enc = start(p, format!("{opened}; sleep 30"));
            let pid = enc.pid();
            drop(enc);
            assert_eq!(
                lines_of(p, pid),
                vec![
                    format!("qsv helper {pid}: opened fake-gen for H.264, LowPower {mode}"),
                    format!("qsv helper {pid}: ended: closed"),
                ]
            );
        }
    }

    #[test]
    fn each_way_a_helper_ends_is_logged_once() {
        let p = policy(QUICK);
        let mut enc = start(p, opens_and("sleep 30"));
        let pid = enc.pid();
        let _ = frame(&mut enc, 1);
        drop(enc);
        let killed = format!("killed, Quick Sync given up (helper {pid} missed its deadline");
        opened_then(p, pid, &[&killed]);

        let p = policy(QUICK);
        let mut enc = start(p, opens_and("sleep 0.1; exit 1"));
        let pid = enc.pid();
        fails_within(&mut enc, Duration::from_secs(2));
        drop(enc);
        let died = ["exited", "closed its stdout", "closed its stdin", "could not be"];
        opened_then(p, pid, &died);

        let p = policy(QUICK);
        let idle = start(p, opens_and("sleep 30"));
        let mut hangs = start(p, opens_and("sleep 30"));
        let (idle_pid, hangs_pid) = (idle.pid(), hangs.pid());
        let _ = frame(&mut hangs, 1);
        drop(hangs);
        drop(idle);
        let by = format!("killed, Quick Sync given up (helper {hangs_pid} missed its deadline");
        opened_then(p, idle_pid, &[&by]);
    }

    /// A helper that dies while idle is noticed at the next request or when its encoder is
    /// dropped, whichever comes first, and its end is logged then, once.
    #[test]
    fn an_idle_helper_that_dies_is_logged_when_it_is_noticed() {
        let p = policy(QUICK);
        let enc = start(p, opens_and("exit 0"));
        let pid = enc.pid();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(lines_of(p, pid).len(), 1, "not noticed yet");
        drop(enc);
        opened_then(p, pid, &["exited while idle"]);

        let p = policy(QUICK);
        let mut enc = start(p, opens_and("exit 0"));
        let pid = enc.pid();
        std::thread::sleep(Duration::from_millis(300));
        assert!(frame(&mut enc, 1).is_err());
        drop(enc);
        let died = ["exited", "closed its stdout", "closed its stdin", "could not be"];
        opened_then(p, pid, &died);
    }

    #[test]
    fn a_helper_that_hangs_on_a_frame_is_killed_at_the_deadline() {
        let p = policy(QUICK);
        let mut enc = start(p, format!("{}; sleep 30", opened()));
        assert_eq!(enc.name, "fake-gen");
        let pid = enc.pid();
        let t = Instant::now();
        let e = frame(&mut enc, 1).err().unwrap();
        assert!(e.0.contains("missed its deadline (a frame)"), "{e}");
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
        let (p, mut pipes) = proc::spawn(c).ok().unwrap();
        let pid = p.id();
        let t = Instant::now();
        let big = vec![0u8; 4 << 20];
        let e = write_by(&mut *pipes.tx, &big, t + Duration::from_millis(300));
        assert_eq!(e, Err(Unwritten::Late));
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(end_all(&[Arc::new(p)], Duration::from_secs(1)).is_empty());
        assert!(!alive(pid));
    }

    #[test]
    fn each_frame_comes_back_from_its_own_call() {
        let p = policy(TIMING);
        let script = format!(
            "{}; {}; sleep 0.05; {}; {}; sleep 30",
            opened(),
            done(0, true),
            done(0, false),
            done(0, true)
        );
        let mut enc = start(p, script);
        let mut got = Vec::new();
        for (pts, key) in [(1, false), (2, false), (3, true)] {
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
            let opts = EncodeOptions {
                pts_ms: pts,
                force_keyframe: key,
                ..Default::default()
            };
            let aus = enc.encode(&f, opts).unwrap();
            assert_eq!(aus.len(), 1, "frame {pts}");
            assert!(
                pts != 1 || aus[0].config.is_some(),
                "the first carries the config"
            );
            got.push((aus[0].pts_ms, aus[0].keyframe));
        }
        assert_eq!(got, vec![(1, true), (2, false), (3, true)]);
        assert_eq!(enc.sent, vec![(1, false), (2, false), (3, true)]);
        assert!(enc.drain().unwrap().is_empty(), "nothing left in flight");
        assert!(p.off().is_none());
    }

    fn no_threads(_: &'static Policy) -> std::io::Result<()> {
        Err(std::io::Error::other("no threads left"))
    }

    #[test]
    fn a_watchdog_that_does_not_start_lets_no_helper_run() {
        let p: &'static Policy = Box::leak(Box::new(Policy::with_watchdog(QUICK, no_threads)));
        for _ in 0..2 {
            let pid = Arc::new(AtomicU32::new(0));
            let seen = pid.clone();
            let spawn = move || {
                let mut c = Command::new("sh");
                c.arg("-c").arg(format!("{}; sleep 30", opened()));
                let (proc, pipes) = proc::spawn(c)?;
                seen.store(proc.id(), Ordering::SeqCst);
                Ok((proc, pipes))
            };
            let e = Remote::open(p, spawn, &cfg(), 0x1234).err().unwrap();
            assert!(e.0.contains("watchdog didn't start"), "{e}");
            let pid = pid.load(Ordering::SeqCst);
            assert!(pid != 0 && !alive(pid), "helper {pid} ended");
        }
        assert!(
            p.off().is_none(),
            "not a give-up: the next try starts it again"
        );
    }

    #[test]
    fn a_process_left_by_a_failed_start_is_kept_and_counted() {
        let p = policy(QUICK);
        let spawn = || {
            let mut c = Command::new("sh");
            c.arg("-c").arg("exit 0");
            let (proc, _pipes) = proc::spawn(c)?;
            let mut status = 0;
            unsafe { libc::waitpid(proc.id() as libc::pid_t, &mut status, 0) };
            Err(NotStarted {
                why: "couldn't put the helper in a job object".into(),
                left: end_all(&[Arc::new(proc)], Duration::from_millis(100)),
            })
        };
        let e = Remote::open(p, spawn, &cfg(), 0x1234).err().unwrap();
        assert!(e.0.contains("job object"), "{e}");
        let off = p.off().expect("Quick Sync off");
        assert!(off.contains("1 helper(s) did not exit"), "{off}");
    }

    fn report(caps: Vec<EncoderCapability>) -> String {
        let r = Report {
            probe: VERSION,
            backend: Backend::Qsv,
            caps,
            diagnostics: Vec::new(),
        };
        let mut line = serde_json::to_string(&r).unwrap();
        line.push('\n');
        let octal: String = line.bytes().map(|b| format!("\\{b:03o}")).collect();
        format!("printf '{octal}'")
    }

    fn cap(luid: u64, codec: Codec, status: ProbeStatus) -> EncoderCapability {
        let device = format!("luid:{luid:016x} Intel(R) Graphics (mfx-gen, oneVPL API 2.9)");
        let mut c = EncoderCapability::new(codec, Backend::Qsv, Acceleration::Hardware, device);
        c.status = status;
        c
    }

    fn prober(
        p: &'static Policy,
        verified: &'static Verified,
        helpers: bool,
        script: String,
        started: Arc<AtomicBool>,
    ) -> probe::Probe {
        probe::Probe {
            helpers,
            spawn: Box::new(move || {
                started.store(true, Ordering::SeqCst);
                let mut c = Command::new("sh");
                c.arg("-c").arg(script.clone());
                proc::spawn(c)
            }),
            policy: p,
            verified,
            timeout: Duration::from_millis(600),
        }
    }

    #[test]
    fn without_helpers_quick_sync_is_unavailable_and_nothing_probes_it() {
        let verified: &'static Verified = Box::leak(Box::default());
        let started = Arc::new(AtomicBool::new(false));
        let ok = report(vec![cap(1, Codec::H264, ProbeStatus::Verified)]);
        let found = prober(policy(QUICK), verified, false, ok, started.clone()).probe();
        assert!(!started.load(Ordering::SeqCst), "no helper, no probe");
        assert!(found.caps.is_empty());
        assert!(
            found.diagnostics[0]
                .reason
                .contains("needs the helper process"),
            "{:?}",
            found.diagnostics
        );
        assert_eq!(verified.adapter(Codec::H264), None);
        let in_process: Vec<Backend> = crate::encoder::caps::hardware_providers(true)
            .iter()
            .map(|p| p.backend())
            .collect();
        assert!(!in_process.contains(&Backend::Qsv), "{in_process:?}");
    }

    #[test]
    fn sessions_open_on_the_adapter_the_probe_verified() {
        let verified: &'static Verified = Box::leak(Box::default());
        let script = report(vec![
            cap(
                0xa,
                Codec::H264,
                ProbeStatus::Unavailable("sample encode: no".into()),
            ),
            cap(0xa, Codec::Hevc, ProbeStatus::Verified),
            cap(0xb, Codec::H264, ProbeStatus::Verified),
        ]);
        let started = Arc::new(AtomicBool::new(false));
        let found = prober(policy(QUICK), verified, true, script, started).probe();
        assert_eq!(found.caps.len(), 3, "{:?}", found.diagnostics);
        assert_eq!(
            verified.adapter(Codec::H264),
            Some(0xb),
            "not the first listed"
        );
        assert_eq!(verified.adapter(Codec::Hevc), Some(0xa));
        assert_eq!(verified.adapter(Codec::Av1), None);
        assert_eq!(luid_of("luid:00000000000000ff x"), Some(0xff));
        assert_eq!(luid_of("luid:xyz"), None);
    }

    #[test]
    fn a_probe_helper_that_hangs_is_killed_at_its_deadline() {
        let verified: &'static Verified = Box::leak(Box::default());
        let p: &'static Policy = Box::leak(Box::new(Policy::with_watchdog(QUICK, |_| Ok(()))));
        let started = Arc::new(AtomicBool::new(false));
        let t = Instant::now();
        let found = prober(p, verified, true, "sleep 30".into(), started).probe();
        assert!(t.elapsed() < Duration::from_millis(600) + QUICK.grace);
        assert!(
            found.diagnostics[0].reason.contains("did not finish"),
            "{:?}",
            found.diagnostics
        );
        let off = p.off().expect("Quick Sync off");
        assert!(off.contains("missed its deadline (probing)"), "{off}");
    }

    #[test]
    fn the_watchdog_alone_gives_up_on_an_overdue_helper() {
        let p: &'static Policy = Box::leak(Box::new(Policy::with_watchdog(QUICK, |_| Ok(()))));
        let other = start(p, format!("{}; sleep 30", opened()));
        let enc = start(p, format!("{}; sleep 30", opened()));
        let (pid, other_pid) = (enc.pid(), other.pid());
        assert!(!p.look(), "nothing is due");
        enc.overdue("a frame");
        assert!(p.look());
        given_up(
            p,
            pid,
            &format!("helper {pid} missed its deadline (a frame)"),
        );
        assert!(!alive(other_pid), "every helper goes");
    }

    #[test]
    fn stats_say_why_after_a_fatal_probe() {
        let verified: &'static Verified = Box::leak(Box::default());
        let p = policy(QUICK);
        super::super::r1_tests::hung_probe(p, verified);
        let off = p.off().expect("Quick Sync off");
        let snapshot = serde_json::json!({"mode": "qsv", "chosen": [], "failed": []});
        let e = crate::encoder::with_live(&snapshot, &[(Backend::Qsv, off)]);
        let why = e["off"][0]["reason"].as_str().unwrap_or_default();
        assert!(
            why.starts_with("qsv given up: the probe: ") && why.contains("MFX_ERR_GPU_HANG"),
            "{e}"
        );
    }
}
