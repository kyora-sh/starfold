//! Every deadline a Quick Sync helper or the probe can miss, and every stopped device either
//! can report, gives Quick Sync up the same way: off with the reason, the helper gone, and
//! no helper or probe started after it. Fake helpers are shell scripts that print what a
//! helper would.

use super::probe::{Probe as Prober, Verified};
use super::proc::{self, NotStarted, Pipes, Proc, Ready};
use super::proto::{Reply, Unit};
use super::r1_tests::READS_OPEN;
use super::remote::{Policy, Remote, Timing};
use super::tests::bitstream;
use crate::encoder::caps::{Acceleration, EncoderCapability, ProbeProvider, ProbeStatus};
use crate::encoder::hw::sample_config;
use crate::encoder::isolate::{Report, VERSION};
use crate::encoder::{
    Backend, Codec, EncodeError, EncodeOptions, EncoderConfig, FrameView, RateConfig, VideoEncoder,
};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const T: Timing = Timing {
    open: Duration::from_millis(300),
    frame: Duration::from_millis(300),
    grace: Duration::from_secs(1),
};

const HANG: &str = "Quick Sync stopped: MFXVideoCORE_SyncOperation failed: MFX_ERR_GPU_HANG (-21)";

fn policy() -> &'static Policy {
    Box::leak(Box::new(Policy::new(T)))
}

fn held_back(_: &'static Policy) -> std::io::Result<()> {
    Ok(())
}

/// A policy whose watchdog never runs: only the caller holds the deadline.
fn unwatched(t: Timing) -> &'static Policy {
    Box::leak(Box::new(Policy::with_watchdog(t, held_back)))
}

fn says(r: &Reply) -> String {
    let octal: String = r.bytes().iter().map(|b| format!("\\{b:03o}")).collect();
    format!("printf '{octal}'")
}

fn opened() -> String {
    let opened = says(&Reply::Opened {
        low_power: true,
        holds_cap: true,
        name: "fake-gen".into(),
    });
    format!("{READS_OPEN}; {opened}")
}

fn done() -> String {
    says(&Reply::Done {
        slot: 0,
        units: vec![Unit {
            key: true,
            data: bitstream(Codec::H264, true, true),
        }],
    })
}

fn stop() -> String {
    says(&Reply::Error {
        stopped: true,
        reason: HANG.into(),
    })
}

/// Starts `script` under `sh`, noting its pid in `pid`.
fn sh(
    script: String,
    pid: Arc<AtomicU32>,
) -> impl Fn() -> Result<(Proc, Pipes), NotStarted> + Send + 'static {
    move || {
        let mut c = Command::new("sh");
        c.arg("-c").arg(&script);
        let (p, pipes) = proc::spawn(c)?;
        pid.store(p.id(), Ordering::SeqCst);
        Ok((p, pipes))
    }
}

fn cfg() -> EncoderConfig {
    let mut c = sample_config(Backend::Qsv, Codec::H264, 64, 48, 30);
    c.rate = RateConfig::new(24.0, 2000, 30);
    c
}

fn open(p: &'static Policy, script: String, pid: Arc<AtomicU32>) -> Remote {
    Remote::open(p, sh(script, pid), &cfg(), 1).unwrap()
}

fn frame(enc: &mut Remote) -> Result<Vec<crate::encoder::AccessUnit>, EncodeError> {
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
    enc.encode(&f, EncodeOptions::default())
}

fn faster(enc: &mut Remote) -> Result<(), EncodeError> {
    enc.set_rate(RateConfig::new(24.0, 9000, 30))
}

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

fn cap(codec: Codec, status: ProbeStatus) -> EncoderCapability {
    let device = "luid:0000000000000001 Intel(R) Graphics (mfx-gen, oneVPL API 2.9)";
    let mut c = EncoderCapability::new(codec, Backend::Qsv, Acceleration::Hardware, device);
    c.status = status;
    c
}

fn probe(p: &'static Policy, script: String, pid: Arc<AtomicU32>) -> String {
    let found = Prober {
        helpers: true,
        spawn: Box::new(sh(script, pid)),
        policy: p,
        verified: Box::leak(Box::default()),
        timeout: T.open,
    }
    .probe();
    let notes: Vec<&str> = found
        .diagnostics
        .iter()
        .map(|d| d.reason.as_str())
        .collect();
    notes.join("; ")
}

fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// What is wrong after a row, if anything: Quick Sync must be off with a reason containing
/// `said`, the helper gone within the grace, and neither a helper nor a probe may start.
fn after(p: &'static Policy, pid: u32, said: &str) -> Option<String> {
    let Some(off) = p.off() else {
        return Some("Quick Sync is still on".into());
    };
    if !off.starts_with("qsv given up: ") || !off.contains(said) {
        return Some(format!("off for another reason: {off}"));
    }
    let t = Instant::now();
    while pid != 0 && alive(pid) && t.elapsed() < T.grace {
        std::thread::sleep(Duration::from_millis(5));
    }
    if pid == 0 || alive(pid) {
        return Some(format!("helper {pid} is still there"));
    }
    let started = Arc::new(AtomicBool::new(false));
    let seen = started.clone();
    let spawn = move || -> Result<(Proc, Pipes), NotStarted> {
        seen.store(true, Ordering::SeqCst);
        Err(NotStarted::from("not started".to_string()))
    };
    let refused = Remote::open(p, spawn, &cfg(), 1).err().map(|e| e.0);
    if started.swap(false, Ordering::SeqCst) || refused.as_deref() != Some(off.as_str()) {
        return Some(format!("a helper started after the give-up ({refused:?})"));
    }
    let seen = started.clone();
    let found = Prober {
        helpers: true,
        spawn: Box::new(move || {
            seen.store(true, Ordering::SeqCst);
            Err(NotStarted::from("not started".to_string()))
        }),
        policy: p,
        verified: Box::leak(Box::default()),
        timeout: T.open,
    }
    .probe();
    if started.load(Ordering::SeqCst) {
        return Some(format!(
            "a probe started after the give-up ({:?})",
            found.diagnostics
        ));
    }
    None
}

type Drive = fn(&'static Policy, Arc<AtomicU32>) -> String;

#[test]
fn every_missed_deadline_and_stopped_device_gives_quick_sync_up() {
    let rows: [(&str, &str, Drive); 9] = [
        (
            "open deadline",
            "missed its deadline (opening)",
            |p, pid| {
                let e = Remote::open(p, sh("sleep 30".into(), pid), &cfg(), 1);
                e.err().map(|e| e.0).unwrap_or_default()
            },
        ),
        (
            "frame deadline",
            "missed its deadline (a frame)",
            |p, pid| {
                let mut enc = open(p, format!("{}; sleep 30", opened()), pid);
                frame(&mut enc).err().map(|e| e.0).unwrap_or_default()
            },
        ),
        (
            "rate deadline",
            "missed its deadline (a rate change)",
            |p, pid| {
                let mut enc = open(p, format!("{}; sleep 30", opened()), pid);
                faster(&mut enc).err().map(|e| e.0).unwrap_or_default()
            },
        ),
        (
            "probe deadline",
            "missed its deadline (probing)",
            |p, pid| probe(p, "sleep 30".into(), pid),
        ),
        (
            "probe exit deadline",
            "missed its deadline (exiting after the probe)",
            |p, pid| probe(p, "exec 1>&-; sleep 30".into(), pid),
        ),
        ("stopped at open", "MFX_ERR_GPU_HANG", |p, pid| {
            let e = Remote::open(p, sh(format!("{}; sleep 30", stop()), pid), &cfg(), 1);
            e.err().map(|e| e.0).unwrap_or_default()
        }),
        ("stopped on a frame", "MFX_ERR_GPU_HANG", |p, pid| {
            let mut enc = open(p, format!("{}; {}; sleep 30", opened(), stop()), pid);
            frame(&mut enc).err().map(|e| e.0).unwrap_or_default()
        }),
        ("stopped on a rate change", "MFX_ERR_GPU_HANG", |p, pid| {
            let mut enc = open(p, format!("{}; {}; sleep 30", opened(), stop()), pid);
            faster(&mut enc).err().map(|e| e.0).unwrap_or_default()
        }),
        (
            "stopped in the probe",
            "the probe: Quick Sync stopped",
            |p, pid| {
                let caps = vec![
                    cap(Codec::H264, ProbeStatus::Verified),
                    cap(
                        Codec::Hevc,
                        ProbeStatus::Unavailable(format!("sample encode: {HANG}")),
                    ),
                ];
                let note = format!("{HANG}; no further adapter is probed");
                probe(p, report(caps, &[&note]), pid)
            },
        ),
    ];
    let mut wrong = Vec::new();
    for (name, said, drive) in rows {
        let p = policy();
        let pid = Arc::new(AtomicU32::new(0));
        let seen = drive(p, pid.clone());
        if let Some(w) = after(p, pid.load(Ordering::SeqCst), said) {
            wrong.push(format!("{name}: {w} (the caller saw: {seen})"));
        }
    }
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}

/// Reads as `inner` does but takes `delay` to see its end, as a reader descheduled just as
/// the helper's output ends.
struct SlowEnd {
    inner: Box<dyn Ready>,
    delay: Duration,
}

impl Ready for SlowEnd {
    fn read_ready(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        let r = self.inner.read_ready(buf);
        if matches!(r, Ok(Some(0))) {
            std::thread::sleep(self.delay);
        }
        r
    }

    fn write_ready(&mut self, buf: &[u8]) -> std::io::Result<Option<usize>> {
        self.inner.write_ready(buf)
    }
}

/// `start`, then `wait` for the helper to write what it will, its output's end seen
/// `delay` late.
fn late_end(
    start: impl Fn() -> Result<(Proc, Pipes), NotStarted> + Send + 'static,
    wait: Duration,
    delay: Duration,
) -> impl Fn() -> Result<(Proc, Pipes), NotStarted> + Send + 'static {
    move || {
        let (p, Pipes { tx, rx }) = start()?;
        std::thread::sleep(wait);
        let rx = Box::new(SlowEnd { inner: rx, delay });
        Ok((p, Pipes { tx, rx }))
    }
}

/// A probe that verifies H.264, started by `spawn` under `p`: what it said, and whether
/// H.264 came out verified.
fn verifying_probe(
    p: &'static Policy,
    spawn: impl Fn() -> Result<(Proc, Pipes), NotStarted> + Send + 'static,
    timeout: Duration,
) -> String {
    let verified: &'static Verified = Box::leak(Box::default());
    let found = Prober {
        helpers: true,
        spawn: Box::new(spawn),
        policy: p,
        verified,
        timeout,
    }
    .probe();
    let notes: Vec<&str> = found
        .diagnostics
        .iter()
        .map(|d| d.reason.as_str())
        .collect();
    match verified.adapter(Codec::H264) {
        Some(_) => format!("{}; H.264 VERIFIED", notes.join("; ")),
        None => notes.join("; "),
    }
}

fn verified_report() -> String {
    report(vec![cap(Codec::H264, ProbeStatus::Verified)], &[])
}

type Late = fn(Arc<AtomicU32>) -> (&'static Policy, String);

/// Work that ends at or past its deadline, seen by the caller before any watchdog looks,
/// is a miss however it ended: a closed pipe, the probe's output ending, or its exit.
#[test]
fn work_that_ends_past_its_deadline_is_a_miss() {
    let rows: [(&str, &str, Late); 3] = [
        (
            "a frame's helper closes its stdout late",
            "missed its deadline (a frame)",
            |pid| {
                let p = unwatched(T);
                let start = sh(format!("{}; exec 1>&-; sleep 30", opened()), pid);
                let spawn = late_end(start, Duration::ZERO, T.frame + T.frame);
                let mut enc = Remote::open(p, spawn, &cfg(), 1).unwrap();
                (p, frame(&mut enc).err().map(|e| e.0).unwrap_or_default())
            },
        ),
        (
            "the probe's output ends late",
            "missed its deadline (probing)",
            |pid| {
                let p = unwatched(T);
                let wait = Duration::from_millis(300);
                let spawn = late_end(sh(verified_report(), pid), wait, wait);
                (p, verifying_probe(p, spawn, Duration::from_millis(100)))
            },
        ),
        (
            "the probe exits late",
            "missed its deadline (exiting after the probe)",
            |pid| {
                let p = unwatched(Timing {
                    grace: Duration::ZERO,
                    ..T
                });
                let wait = Duration::from_millis(300);
                let spawn = late_end(sh(verified_report(), pid), wait, Duration::ZERO);
                (p, verifying_probe(p, spawn, T.open))
            },
        ),
    ];
    let mut wrong = Vec::new();
    for (name, said, drive) in rows {
        let pid = Arc::new(AtomicU32::new(0));
        let (p, seen) = drive(pid.clone());
        let w = if seen.contains("H.264 VERIFIED") {
            Some("H.264 verified".to_string())
        } else {
            after(p, pid.load(Ordering::SeqCst), said)
        };
        if let Some(w) = w {
            wrong.push(format!("{name}: {w} (the caller saw: {seen})"));
        }
    }
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}

#[test]
fn a_reply_read_after_its_deadline_is_a_miss() {
    let p = unwatched(Timing {
        frame: Duration::ZERO,
        ..T
    });
    let pid = Arc::new(AtomicU32::new(0));
    let mut enc = open(
        p,
        format!("{}; {}; sleep 30", opened(), done()),
        pid.clone(),
    );
    std::thread::sleep(Duration::from_millis(300));
    let r = frame(&mut enc);
    assert!(
        r.as_ref()
            .is_err_and(|e| e.0.contains("missed its deadline (a frame)")),
        "a reply waiting when the deadline passed was taken: {:?}",
        r.map(|u| u.len())
    );
    let pid = pid.load(Ordering::SeqCst);
    assert_eq!(after(p, pid, "missed its deadline (a frame)"), None);
}

#[test]
fn probe_output_read_after_its_deadline_is_a_miss() {
    let p = unwatched(T);
    let pid = Arc::new(AtomicU32::new(0));
    let verified: &'static Verified = Box::leak(Box::default());
    let start = sh(
        report(vec![cap(Codec::H264, ProbeStatus::Verified)], &[]),
        pid.clone(),
    );
    let found = Prober {
        helpers: true,
        spawn: Box::new(move || {
            let r = start();
            std::thread::sleep(Duration::from_millis(300));
            r
        }),
        policy: p,
        verified,
        timeout: Duration::ZERO,
    }
    .probe();
    assert_eq!(verified.adapter(Codec::H264), None, "{:?}", found.caps);
    assert!(
        !found.caps.iter().any(|c| c.status == ProbeStatus::Verified),
        "{:?}",
        found.caps
    );
    let pid = pid.load(Ordering::SeqCst);
    assert_eq!(after(p, pid, "missed its deadline (probing)"), None);
}
