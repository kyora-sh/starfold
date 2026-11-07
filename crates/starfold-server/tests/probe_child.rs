//! The real binary as a probe child: what the server runs at startup.

use starfold::encoder::isolate::{run, FLAG, GO, NO_GO};
use starfold::encoder::Backend;
use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn starfold() -> Command {
    Command::new(env!("CARGO_BIN_EXE_starfold"))
}

fn piped(args: &[&str]) -> std::process::Child {
    starfold()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn after_go(args: &[&str]) -> Output {
    let mut c = piped(args);
    c.stdin.take().unwrap().write_all(&[GO]).unwrap();
    c.wait_with_output().unwrap()
}

#[test]
fn unknown_backend_is_refused() {
    let s = after_go(&[FLAG, "bogus"]);
    assert_eq!(s.status.code(), Some(2));
    assert!(s.stdout.is_empty());
}

#[test]
fn experimental_probe_is_refused_without_the_flag() {
    let s = after_go(&[FLAG, "nvenc"]);
    assert_eq!(s.status.code(), Some(2));
}

/// Until it reads [`GO`] the child does nothing; if its stdin closes first, as when the
/// server dies before confining it, it exits without having probed anything.
#[test]
fn child_waits_for_go_and_exits_without_it() {
    let mut c = piped(&[FLAG, "nvenc", "--experimental-hw-probes"]);
    std::thread::sleep(Duration::from_millis(300));
    #[cfg(target_os = "linux")]
    let before = std::fs::read_to_string(format!("/proc/{}/maps", c.id())).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(c.try_wait().unwrap().is_none());
    #[cfg(target_os = "linux")]
    {
        let maps = std::fs::read_to_string(format!("/proc/{}/maps", c.id())).unwrap();
        let task = std::fs::read_dir(format!("/proc/{}/task", c.id())).unwrap();
        let kids = std::fs::read_to_string(format!("/proc/{0}/task/{0}/children", c.id())).unwrap();
        assert_eq!(maps, before, "the child mapped something before go");
        assert_eq!(task.count(), 1, "the child started a thread before go");
        assert!(kids.trim().is_empty(), "the child spawned {kids} before go");
    }
    let t = Instant::now();
    drop(c.stdin.take());
    let out = c.wait_with_output().unwrap();
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert_eq!(out.status.code(), Some(NO_GO));
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
}

/// Whatever this host has, the child answers for the backend it was asked about.
#[test]
fn child_reports_on_its_backend() {
    let (backend, args): (Backend, &[&str]) = if cfg!(target_os = "macos") {
        (Backend::VideoToolbox, &[FLAG, "videotoolbox"])
    } else {
        (Backend::Nvenc, &[FLAG, "nvenc", "--experimental-hw-probes"])
    };
    let mut cmd = starfold();
    cmd.args(args);
    let p = run(cmd, backend, Duration::from_secs(30));
    assert!(p.caps.iter().all(|c| c.backend == backend), "{:?}", p.caps);
    assert!(
        p.diagnostics
            .iter()
            .all(|d| d.backend == backend && !d.reason.starts_with("probe ")),
        "{:?}",
        p.diagnostics
    );
    assert!(!p.caps.is_empty() || !p.diagnostics.is_empty());
}
