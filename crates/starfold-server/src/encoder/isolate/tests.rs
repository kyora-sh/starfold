use super::*;
use crate::encoder::caps::{Acceleration, Chroma, ProbeStatus, ProfileCapability};
#[cfg(unix)]
use crate::encoder::caps::{Registry, Software};
use crate::encoder::Codec;

fn good(backend: Backend) -> EncoderCapability {
    let mut c = EncoderCapability::new(Codec::H264, backend, Acceleration::Hardware, "gpu0");
    c.profiles = vec![ProfileCapability::new("high", 8, Chroma::Yuv420)];
    c.low_latency = Some(true);
    c.keyframe_control = Some(true);
    c.status = ProbeStatus::Verified;
    c
}

fn report(backend: Backend, caps: Vec<EncoderCapability>) -> String {
    serde_json::to_string(&Report {
        probe: VERSION,
        backend,
        caps,
        diagnostics: Vec::new(),
    })
    .unwrap()
}

#[cfg(unix)]
fn sh(script: &str) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg(script);
    c
}

#[cfg(unix)]
fn child(backend: Backend, script: String, timeout: Duration) -> Child {
    Child {
        backend,
        command: Box::new(move || Ok(sh(&script))),
        timeout,
    }
}

fn reason(p: &Probe) -> &str {
    assert!(p.caps.is_empty(), "{:?}", p.caps);
    assert_eq!(p.diagnostics.len(), 1, "{:?}", p.diagnostics);
    &p.diagnostics[0].reason
}

#[cfg(unix)]
fn alive(pid: libc::pid_t) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(unix)]
fn gone_soon(pid: libc::pid_t) -> bool {
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn no_child_unless_the_executable_takes_the_flag() {
    assert!(super::child(Backend::Vaapi, true, Duration::from_secs(1)).is_none());
}

#[test]
fn unknown_backend_exits_2() {
    assert_eq!(probe_main(&["bogus".into()]), 2);
    assert_eq!(probe_main(&[]), 2);
}

#[test]
fn report_round_trips() {
    let p = parse(
        report(Backend::Nvenc, vec![good(Backend::Nvenc)]).as_bytes(),
        Backend::Nvenc,
    );
    assert!(p.diagnostics.is_empty());
    assert_eq!(p.caps, vec![good(Backend::Nvenc)]);
}

#[test]
fn report_is_the_last_line() {
    let out = format!(
        "libva info: driver chatter\n{}\n\n",
        report(Backend::Vaapi, vec![good(Backend::Vaapi)])
    );
    let p = parse(out.as_bytes(), Backend::Vaapi);
    assert_eq!(p.caps.len(), 1);
}

#[test]
fn unreadable_or_foreign_reports_are_diagnostics() {
    let b = Backend::Vaapi;
    assert!(reason(&parse(b"", b)).contains("no report"));
    assert!(reason(&parse(b"hello\n", b)).contains("unreadable"));
    let other = report(Backend::Nvenc, Vec::new());
    assert!(reason(&parse(other.as_bytes(), b)).contains("nvenc"));
    let smuggled = report(b, vec![good(Backend::OpenH264)]);
    assert!(reason(&parse(smuggled.as_bytes(), b)).contains("openh264"));
    let old = report(b, Vec::new()).replace("\"probe\":1", "\"probe\":0");
    assert!(reason(&parse(old.as_bytes(), b)).contains("version 0"));
}

#[cfg(unix)]
#[test]
fn good_child_reports_its_encoders() {
    let line = report(Backend::Vaapi, vec![good(Backend::Vaapi)]);
    let p = run(
        sh(&format!("echo noise; echo '{line}'")),
        Backend::Vaapi,
        Duration::from_secs(10),
    );
    assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
    assert_eq!(p.caps, vec![good(Backend::Vaapi)]);
}

#[cfg(unix)]
#[test]
fn crashed_child_is_a_signal_diagnostic() {
    let p = run(sh("kill -SEGV $$"), Backend::Vaapi, Duration::from_secs(10));
    assert_eq!(reason(&p), "probe exited with signal 11");
}

#[cfg(unix)]
#[test]
fn failed_child_is_a_status_diagnostic() {
    let line = report(Backend::Vaapi, vec![good(Backend::Vaapi)]);
    let p = run(
        sh(&format!("echo '{line}'; exit 3")),
        Backend::Vaapi,
        Duration::from_secs(10),
    );
    assert_eq!(reason(&p), "probe exited with status 3");
}

#[test]
fn missing_executable_is_a_diagnostic() {
    let p = run(
        Command::new("/nonexistent/starfold-probe"),
        Backend::Nvenc,
        Duration::from_secs(1),
    );
    assert!(reason(&p).starts_with("probe didn't start"));
}

/// The hung child and the process it started are killed and the child reaped; nothing
/// else is touched.
#[cfg(unix)]
#[test]
fn hung_child_is_killed_at_the_timeout() {
    let dir = std::env::temp_dir().join(format!("starfold-probe-hang-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pids = dir.join("pids");
    let script = format!(
        "sleep 60 & echo $! > {p}.tmp; echo $$ >> {p}.tmp; mv {p}.tmp {p}; wait",
        p = pids.display()
    );
    let t = Instant::now();
    let p = run(sh(&script), Backend::Nvenc, Duration::from_millis(1500));
    let took = t.elapsed();
    assert!(reason(&p).contains("was killed"), "{:?}", p.diagnostics);
    assert!(
        took >= Duration::from_millis(1500) && took < Duration::from_secs(4),
        "{took:?}"
    );
    let listed = std::fs::read_to_string(&pids).unwrap();
    let ids: Vec<libc::pid_t> = listed.lines().map(|l| l.trim().parse().unwrap()).collect();
    assert_eq!(ids.len(), 2, "{listed:?}");
    for pid in ids {
        assert!(gone_soon(pid), "pid {pid} survived the probe timeout");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A vendor probe that crashes and one that hangs cost only themselves: the software
/// encoder and a hardware backend probed in its own child stay usable.
#[cfg(unix)]
#[test]
fn registry_keeps_the_rest_when_children_fail() {
    let vt = report(Backend::VideoToolbox, vec![good(Backend::VideoToolbox)]);
    let providers: Vec<Box<dyn ProbeProvider>> = vec![
        Box::new(Software(Backend::OpenH264)),
        Box::new(child(
            Backend::VideoToolbox,
            format!("echo '{vt}'"),
            Duration::from_secs(10),
        )),
        Box::new(child(
            Backend::Vaapi,
            "kill -SEGV $$".into(),
            Duration::from_secs(10),
        )),
        Box::new(child(
            Backend::Nvenc,
            "exec sleep 60".into(),
            Duration::from_millis(500),
        )),
    ];
    let r = Registry::probe(providers, Duration::from_secs(10));
    assert!(r.check(Backend::OpenH264, Codec::H264).is_ok());
    let vt = r
        .encoders
        .iter()
        .find(|c| c.backend == Backend::VideoToolbox)
        .unwrap();
    assert_eq!(
        vt.implemented,
        Backend::VideoToolbox.implements(Codec::H264)
    );
    let why = |b: Backend| {
        r.diagnostics
            .iter()
            .find(|d| d.backend == b)
            .map(|d| d.reason.clone())
            .unwrap_or_default()
    };
    assert_eq!(why(Backend::Vaapi), "probe exited with signal 11");
    assert!(
        why(Backend::Nvenc).contains("was killed"),
        "{:?}",
        r.diagnostics
    );
}

/// What a crashed child left running in its process group goes with it.
#[cfg(unix)]
#[test]
fn crashed_childs_leftovers_are_killed() {
    let dir = std::env::temp_dir().join(format!("starfold-probe-crash-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pid_file = dir.join("pid");
    let script = format!(
        "sleep 60 & echo $! > {p}.tmp; mv {p}.tmp {p}; kill -SEGV $$",
        p = pid_file.display()
    );
    let p = run(sh(&script), Backend::Vaapi, Duration::from_secs(10));
    assert_eq!(reason(&p), "probe exited with signal 11");
    let pid: libc::pid_t = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(gone_soon(pid), "pid {pid} outlived the crashed probe");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unbuildable_command_is_a_diagnostic() {
    let c = Child {
        backend: Backend::Nvenc,
        command: Box::new(|| Err("no path to this executable: gone".into())),
        timeout: Duration::from_secs(1),
    };
    assert!(c.bounded());
    let p = c.probe();
    assert_eq!(
        reason(&p),
        "probe didn't start: no path to this executable: gone"
    );
}

#[cfg(unix)]
#[test]
fn flooding_child_is_cut_off() {
    let t = Instant::now();
    let p = run(
        sh("head -c 4000000 /dev/zero; sleep 60"),
        Backend::Vaapi,
        Duration::from_secs(20),
    );
    assert_eq!(reason(&p), "probe printed more than 1024 KiB");
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
}

/// A process that left the child's process group and kept its stdout costs the grace
/// period, not a reader blocked for as long as it lives.
#[cfg(target_os = "linux")]
#[test]
fn output_held_open_by_an_escaped_process() {
    let dir = std::env::temp_dir().join(format!("starfold-probe-escape-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ready = dir.join("ready");
    let line = report(Backend::Vaapi, vec![good(Backend::Vaapi)]);
    let script = format!(
        "setsid sh -c 'echo $$ > {r}.tmp; mv {r}.tmp {r}; exec sleep 4' & \
         while [ ! -s {r} ]; do sleep 0.01; done; echo '{line}'",
        r = ready.display()
    );
    let t = Instant::now();
    let p = run(sh(&script), Backend::Vaapi, Duration::from_secs(10));
    let took = t.elapsed();
    let pid: libc::pid_t = std::fs::read_to_string(&ready)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(reason(&p), "probe output stayed open after it exited");
    assert!(took < Duration::from_secs(3), "{took:?}");
    assert_ne!(unsafe { libc::getsid(pid) }, unsafe { libc::getsid(0) });
    std::fs::remove_dir_all(&dir).unwrap();
}

const FAKE: &str = "STARFOLD_FAKE_PROBE";

/// This test binary again, as [`fake_child`] in `mode`.
fn fake(mode: &str, dir: &std::path::Path) -> Command {
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.args([
        "--exact",
        "encoder::isolate::tests::fake_child",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(FAKE, mode)
    .env("STARFOLD_FAKE_DIR", dir);
    c
}

/// A probe child that waits for [`GO`] as the real one does, then (`gate`) records that it
/// went and reports, or starts a process holding its stdout, waits for it to record its id,
/// and reports or hangs. Does nothing unless run by [`fake`].
#[test]
fn fake_child() {
    let Ok(mode) = std::env::var(FAKE) else {
        return;
    };
    let dir = std::path::PathBuf::from(std::env::var_os("STARFOLD_FAKE_DIR").unwrap());
    if mode == "sleep" {
        std::fs::write(dir.join("pid.tmp"), std::process::id().to_string()).unwrap();
        std::fs::rename(dir.join("pid.tmp"), dir.join("pid")).unwrap();
        std::thread::sleep(Duration::from_secs(60));
        std::process::exit(0);
    }
    let mut go = [0u8];
    if std::io::stdin().read_exact(&mut go).is_err() || go != [GO] {
        std::process::exit(NO_GO);
    }
    if mode == "gate" {
        std::fs::write(dir.join("went"), "").unwrap();
        std::process::exit(0);
    }
    let _held = fake("sleep", &dir).stdin(Stdio::null()).spawn().unwrap();
    while !dir.join("pid").exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
    if mode == "hang" {
        std::thread::sleep(Duration::from_secs(60));
    }
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "\n{}",
        report(Backend::Vaapi, vec![good(Backend::Vaapi)])
    )
    .unwrap();
    out.flush().unwrap();
    std::process::exit(0);
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("starfold-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn held_pid(dir: &std::path::Path) -> u32 {
    std::fs::read_to_string(dir.join("pid"))
        .expect("the fake probe never started its descendant")
        .trim()
        .parse()
        .unwrap()
}

#[cfg(unix)]
fn exits_soon(pid: u32) -> bool {
    gone_soon(pid as libc::pid_t)
}

#[cfg(windows)]
fn exits_soon(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    let Ok(h) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
        return true;
    };
    let r = unsafe { WaitForSingleObject(h, 5000) };
    unsafe {
        let _ = CloseHandle(h);
    }
    r == WAIT_OBJECT_0
}

/// A descendant that keeps the child's stdout dies with the child, so the report it
/// printed is read rather than lost to a pipe that stays open.
#[test]
fn descendant_holding_the_output_goes_with_the_child() {
    let dir = scratch("probe-held");
    let p = run(
        fake("report", &dir),
        Backend::Vaapi,
        Duration::from_secs(30),
    );
    assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
    assert_eq!(p.caps, vec![good(Backend::Vaapi)]);
    let pid = held_pid(&dir);
    assert!(exits_soon(pid), "pid {pid} outlived the probe");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn hung_childs_descendants_are_killed() {
    let dir = scratch("probe-hung");
    let p = run(fake("hang", &dir), Backend::Nvenc, Duration::from_secs(5));
    assert!(reason(&p).contains("was killed"), "{:?}", p.diagnostics);
    let pid = held_pid(&dir);
    assert!(exits_soon(pid), "pid {pid} survived the probe timeout");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A pipe whose writer stays open costs the grace period, never a blocked read.
#[test]
fn output_held_open_costs_the_grace() {
    let (r, mut w) = std::io::pipe().unwrap();
    w.write_all(b"partial").unwrap();
    let mut out = Output::new(Some(r)).unwrap();
    assert!(!out.pump());
    let t = Instant::now();
    assert!(matches!(
        out.finish(Duration::from_millis(300)),
        Err(OutputError::Open)
    ));
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    drop(w);
}

#[test]
fn output_is_read_to_its_end() {
    let (r, mut w) = std::io::pipe().unwrap();
    w.write_all(b"one\ntwo").unwrap();
    drop(w);
    let out = Output::new(Some(r)).unwrap();
    assert_eq!(out.finish(Duration::from_secs(5)).unwrap(), b"one\ntwo");
}

/// The child gets [`GO`] only after it was confined, and runs on once it has.
#[test]
fn go_follows_confinement() {
    let dir = scratch("probe-go");
    let mut cmd = fake("gate", &dir);
    cmd.stdout(Stdio::null());
    let went = dir.join("went");
    let mut c = start(&mut cmd, |_| {
        std::thread::sleep(Duration::from_millis(500));
        match went.exists() {
            true => Err("went before it was confined".into()),
            false => Ok(()),
        }
    })
    .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match c.try_wait().unwrap() {
            Some(s) => break Some(s),
            None if Instant::now() >= end => break None,
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    if status.is_none() {
        let _ = c.kill();
        let _ = c.wait();
    }
    assert!(status.is_some_and(|s| s.success()), "{status:?}");
    assert!(went.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A child that can't be confined never gets [`GO`] and is gone when `start` returns.
#[test]
fn unconfined_child_never_goes() {
    let dir = scratch("probe-nogo");
    let mut cmd = fake("gate", &dir);
    cmd.stdout(Stdio::null());
    let pid = std::cell::Cell::new(0);
    let e = start(&mut cmd, |c| {
        pid.set(c.id());
        Err("no job".into())
    })
    .unwrap_err();
    assert_eq!(e, "no job");
    assert!(exits_soon(pid.get()));
    assert!(!dir.join("went").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}
