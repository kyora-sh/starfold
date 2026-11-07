//! The real binary as a probe child: what the server runs at startup.

use starfold::encoder::isolate::{run, FLAG};
use starfold::encoder::Backend;
use std::process::Command;
use std::time::Duration;

fn starfold() -> Command {
    Command::new(env!("CARGO_BIN_EXE_starfold"))
}

#[test]
fn unknown_backend_is_refused() {
    let s = starfold().args([FLAG, "bogus"]).output().unwrap();
    assert_eq!(s.status.code(), Some(2));
    assert!(s.stdout.is_empty());
}

#[test]
fn experimental_probe_is_refused_without_the_flag() {
    let s = starfold().args([FLAG, "nvenc"]).output().unwrap();
    assert_eq!(s.status.code(), Some(2));
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
