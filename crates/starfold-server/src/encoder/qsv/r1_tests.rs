//! Termination, delivery, probing and rate-buffer cases, against fake helpers (shell
//! scripts that print what a helper would) and the fake runtime.

use super::probe::{Probe as Prober, Verified};
use super::proc;
use super::proto::{Reply, Unit};
use super::remote::{Policy, Remote, Timing};
use super::session::Session;
use super::tests::{bitstream, Fake};
use crate::encoder::caps::{Acceleration, EncoderCapability, ProbeProvider, ProbeStatus};
use crate::encoder::hw::sample_config;
use crate::encoder::isolate::{Report, VERSION};
use crate::encoder::{
    AccessUnit, Backend, Codec, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig,
    VideoEncoder,
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

fn policy() -> &'static Policy {
    Box::leak(Box::new(Policy::new(QUICK)))
}

/// A shell `printf` of `r` as the helper would write it.
fn says(r: &Reply) -> String {
    let octal: String = r.bytes().iter().map(|b| format!("\\{b:03o}")).collect();
    format!("printf '{octal}'")
}

/// A shell line that reads the server's Open request (its four-byte length, read as the
/// host's little-endian u32, then that many bytes) a byte at a time. A fake helper that
/// starts with it can't exit, crash or close its stdin before that write has landed, which
/// would fail the open with a broken pipe.
pub(super) const READS_OPEN: &str = concat!(
    "n=$(( $(dd bs=1 count=4 2>/dev/null | od -An -tu4) )); ",
    "dd bs=1 count=$n of=/dev/null 2>/dev/null"
);

fn opened() -> String {
    let opened = says(&Reply::Opened {
        low_power: true,
        holds_cap: true,
        name: "fake-gen".into(),
    });
    format!("{READS_OPEN}; {opened}")
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

fn cfg() -> EncoderConfig {
    let mut c = sample_config(Backend::Qsv, Codec::H264, 64, 48, 30);
    c.rate = RateConfig::new(24.0, 2000, 30);
    c
}

/// A helper running `script`, opened.
macro_rules! start {
    ($p:expr, $script:expr) => {{
        let script: String = $script;
        Remote::open(
            $p,
            move || {
                let mut c = Command::new("sh");
                c.arg("-c").arg(script);
                proc::spawn(c)
            },
            &cfg(),
            0x1234,
        )
    }};
}

fn frame(enc: &mut Remote, pts: i64) -> Result<Vec<AccessUnit>, EncodeError> {
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

/// Polls `cond` until it holds or `within` passes; how long it took.
fn until(within: Duration, cond: impl Fn() -> bool) -> Duration {
    let t = Instant::now();
    while !cond() && t.elapsed() < within {
        std::thread::sleep(Duration::from_millis(5));
    }
    t.elapsed()
}

/// An opened helper that has exited and been reaped behind the server's back, so it can't
/// be waited for: what the server sees of one held in the kernel by a stuck driver call.
fn unwaitable(p: &'static Policy) -> Remote {
    let enc = start!(p, format!("{}; exit 0", opened())).unwrap();
    let mut status = 0;
    unsafe { libc::waitpid(enc.pid() as libc::pid_t, &mut status, 0) };
    enc
}

/// A missed deadline signals every helper before it waits for any: one that won't be
/// reaped doesn't hold the kill of the next one back by the grace.
#[test]
fn every_helper_is_signalled_before_any_is_waited_for() {
    let p = policy();
    let _held = unwaitable(p);
    let mut b = start!(p, format!("{}; sleep 30", opened())).unwrap();
    let pid = b.pid();
    let sent = Instant::now();
    let worker = std::thread::spawn(move || {
        let _ = frame(&mut b, 1);
        b
    });
    until(Duration::from_secs(5), || !alive(pid));
    let gone = sent.elapsed();
    drop(worker.join());
    assert!(
        gone < QUICK.frame + Duration::from_millis(400),
        "helper {pid} was killed {gone:?} after its frame went out (deadline {:?})",
        QUICK.frame
    );
}

/// A helper still there after the grace stays counted, and Quick Sync stays off with it.
#[test]
fn a_helper_that_does_not_exit_is_kept_and_counted() {
    let p = policy();
    let _held = unwaitable(p);
    let mut b = start!(p, format!("{}; sleep 30", opened())).unwrap();
    let _ = frame(&mut b, 1);
    until(Duration::from_secs(4), || {
        p.off().is_some_and(|o| o.contains("did not exit"))
    });
    let off = p.off().expect("Quick Sync off");
    assert!(off.contains("1 helper(s) did not exit"), "{off}");
}

static LOG_HOLDS: AtomicU32 = AtomicU32::new(0);
static LOG_RELEASED: AtomicBool = AtomicBool::new(false);

/// A log whose output stalls (a full stderr pipe nobody reads) on the give-up line of the
/// helper in [`LOG_HOLDS`].
struct Stalled;

impl log::Log for Stalled {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, r: &log::Record<'_>) {
        let pid = LOG_HOLDS.load(Ordering::SeqCst);
        if pid == 0 || !r.args().to_string().contains(&format!("helper {pid} ")) {
            return;
        }
        let t = Instant::now();
        while !LOG_RELEASED.load(Ordering::SeqCst) && t.elapsed() < Duration::from_secs(4) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn flush(&self) {}
}

static STALLED: Stalled = Stalled;

/// Logging that blocks can't hold back the kill of a helper past its deadline.
#[test]
fn a_stalled_log_does_not_delay_the_kill() {
    assert!(
        log::set_logger(&STALLED).is_ok(),
        "no other test installs a logger"
    );
    log::set_max_level(log::LevelFilter::Error);
    let p = policy();
    let mut b = start!(p, format!("{}; sleep 30", opened())).unwrap();
    let pid = b.pid();
    LOG_HOLDS.store(pid, Ordering::SeqCst);
    let sent = Instant::now();
    let worker = std::thread::spawn(move || {
        let _ = frame(&mut b, 1);
        b
    });
    until(Duration::from_secs(5), || !alive(pid));
    let gone = sent.elapsed();
    LOG_RELEASED.store(true, Ordering::SeqCst);
    drop(worker.join());
    assert!(
        gone < QUICK.frame + QUICK.grace,
        "helper {pid} was killed {gone:?} after its frame went out, behind the log"
    );
}

/// A frame slower than the frame interval still comes back from its own call, and a
/// stream that then goes quiet is not a hang.
#[test]
fn a_slow_last_picture_reaches_the_viewer_and_idling_is_no_hang() {
    let p = policy();
    let script = format!("{}; sleep 0.15; {}; sleep 30", opened(), done(0, true));
    let mut enc = start!(p, script).unwrap();
    let got = frame(&mut enc, 1).unwrap();
    std::thread::sleep(QUICK.frame + QUICK.grace + Duration::from_millis(200));
    assert_eq!(p.off(), None, "an idle stream given up");
    assert_eq!(got.len(), 1, "the picture came back from its own call");
}

/// Each call returns at most its own picture, even after a slow one: the video path sends
/// one call's units as one chunk.
#[test]
fn one_picture_per_call_after_a_slow_frame() {
    let p = policy();
    let script = format!(
        "{}; sleep 0.15; {}; {}; sleep 30",
        opened(),
        done(0, true),
        done(1, false)
    );
    let mut enc = start!(p, script).unwrap();
    let a = frame(&mut enc, 1).unwrap();
    let b = frame(&mut enc, 2).unwrap();
    let per_call: Vec<Vec<i64>> = [&a, &b]
        .iter()
        .map(|c| c.iter().map(|u| u.pts_ms).collect())
        .collect();
    assert_eq!(per_call, vec![vec![1], vec![2]]);
}

/// A helper that dies between frames counts like one that dies on a frame.
#[test]
fn helpers_dying_while_idle_count_toward_the_give_up() {
    let p = policy();
    for _ in 0..2 {
        let enc = start!(p, format!("{}; exit 1", opened())).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        drop(enc);
    }
    let off = p.off();
    assert!(
        off.as_deref()
            .is_some_and(|o| o.contains("a second helper failed")),
        "{off:?}"
    );
}

/// A rate change never asks Reset for a larger buffer than Init sized: not the boosted
/// keyframe rate, not a higher cap at a lower frame rate.
#[test]
fn rate_changes_stay_within_the_initial_buffer() {
    let (fake, log) = Fake::new(Codec::H264);
    let mut c = sample_config(Backend::Qsv, Codec::H264, 1920, 1080, 60);
    c.rate = RateConfig::new(24.0, 30_000, 60);
    let mut s = Session::new(Box::new(fake), &c).unwrap();
    let boost = 30_000 * 4;
    let boosted = RateConfig {
        quality: 24.0,
        max_kbps: boost,
        buf_kbit: boost / 60 * 8,
        fps: 60,
    };
    for r in [boosted, RateConfig::new(24.0, 200_000, 10)] {
        s.set_rate(r).unwrap();
    }
    let kb = |p: &super::ffi::VideoParam| {
        p.mfx.buffer_size_in_kb as u32 * p.mfx.brc_param_multiplier.max(1) as u32
    };
    let (init, resets): (u32, Vec<u32>) = {
        let l = log.lock().unwrap();
        (kb(&l.inited[0]), l.reset.iter().map(kb).collect())
    };
    drop(s);
    assert_eq!(resets.len(), 2);
    for r in resets {
        assert!(r <= init, "Reset asks for {r} KB, Init sized {init} KB");
    }
}

/// A probe report, printed as a probe helper prints it.
fn report(caps: Vec<EncoderCapability>, notes: &[&str]) -> String {
    let mut found = crate::encoder::caps::Probe {
        caps,
        ..Default::default()
    };
    for n in notes {
        found.note(Backend::Qsv, Some("luid:0000000000000001 Intel"), *n);
    }
    let r = Report {
        probe: VERSION,
        backend: Backend::Qsv,
        caps: found.caps,
        diagnostics: found.diagnostics,
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

const HANG: &str = "Quick Sync stopped: MFXVideoCORE_SyncOperation failed: MFX_ERR_GPU_HANG (-21)";

/// The probe of one adapter: H.264 verified, then HEVC hangs the device.
pub(super) fn hung_probe(
    p: &'static Policy,
    verified: &'static Verified,
) -> crate::encoder::caps::Probe {
    let script = report(
        vec![
            cap(1, Codec::H264, ProbeStatus::Verified),
            cap(
                1,
                Codec::Hevc,
                ProbeStatus::Unavailable(format!("sample encode: {HANG}")),
            ),
        ],
        &[&format!("{HANG}; no further adapter is probed")],
    );
    Prober {
        helpers: true,
        spawn: Box::new(move || {
            let mut c = Command::new("sh");
            c.arg("-c").arg(script.clone());
            proc::spawn(c)
        }),
        policy: p,
        verified,
        timeout: Duration::from_secs(2),
    }
    .probe()
}

/// A device that stops answering during the probe ends Quick Sync for the process, as
/// a hang while streaming does: what passed before it is not used.
#[test]
fn a_fatal_probe_status_ends_quick_sync_whatever_passed_before() {
    let p = policy();
    let verified: &'static Verified = Box::leak(Box::default());
    let found = hung_probe(p, verified);
    let off = p.off();
    assert!(
        off.as_deref()
            .is_some_and(|o| o.contains("MFX_ERR_GPU_HANG")),
        "{off:?}"
    );
    assert_eq!(verified.adapter(Codec::H264), None, "H.264 not offered");
    assert!(
        found
            .caps
            .iter()
            .all(|c| matches!(&c.status, ProbeStatus::Unavailable(r) if r.contains("GPU_HANG"))),
        "{:?}",
        found.caps
    );
    let started = Arc::new(AtomicBool::new(false));
    let seen = started.clone();
    let spawn = move || {
        seen.store(true, Ordering::SeqCst);
        let mut c = Command::new("sh");
        c.arg("-c").arg(opened());
        proc::spawn(c)
    };
    assert!(Remote::open(p, spawn, &cfg(), 1).is_err());
    assert!(
        !started.load(Ordering::SeqCst),
        "no helper starts afterwards"
    );
}

/// One helper that won't exit counts once, however many paths end it: the give-up, then
/// its encoder dropped.
#[test]
fn a_stuck_helper_counts_once() {
    let p = policy();
    let held = unwaitable(p);
    let mut b = start!(p, format!("{}; sleep 30", opened())).unwrap();
    let _ = frame(&mut b, 1);
    until(Duration::from_secs(4), || {
        p.off().is_some_and(|o| o.contains("did not exit"))
    });
    drop(held);
    drop(b);
    let off = p.off().expect("Quick Sync off");
    assert!(off.contains("; 1 helper(s) did not exit"), "{off}");
}

/// No Quick Sync child is ever moved into its job after it exists: there is no call that
/// could, so no moment it runs outside one.
#[test]
fn no_helper_is_put_in_a_job_after_it_is_created() {
    let needle = concat!("Assign", "ProcessToJobObject");
    for (name, src) in [
        ("proc.rs", include_str!("proc.rs")),
        ("remote.rs", include_str!("remote.rs")),
        ("probe.rs", include_str!("probe.rs")),
        ("helper.rs", include_str!("helper.rs")),
        ("mod.rs", include_str!("mod.rs")),
    ] {
        assert!(!src.contains(needle), "{name} calls {needle}");
    }
}
