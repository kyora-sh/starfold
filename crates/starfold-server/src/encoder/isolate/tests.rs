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
        command: Box::new(move || sh(&script)),
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
    assert_eq!(child_main(&["bogus".into()]), 2);
    assert_eq!(child_main(&[]), 2);
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
