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

/// A probe child loads no OpenH264, even with a library path in its environment: a library
/// handed over in a probe child makes it exit 3, and the binary skips the load there.
#[test]
fn a_probe_child_loads_no_openh264() {
    let s = starfold()
        .args([FLAG, "bogus"])
        .env("STARFOLD_OPENH264_LIB", "/nonexistent/libopenh264.so")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&s.stderr);
    assert_eq!(s.status.code(), Some(2), "{err}");
    assert!(!err.contains("must not have Cisco's OpenH264"), "{err}");
}

/// Whether running `cmd` opened `path`, as inotify on it saw.
#[cfg(target_os = "linux")]
fn opens(path: &std::path::Path, mut cmd: Command) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let ino = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    assert!(ino >= 0, "{}", std::io::Error::last_os_error());
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert!(unsafe { libc::inotify_add_watch(ino, c.as_ptr(), libc::IN_OPEN) } >= 0);
    let out = cmd.output().unwrap();
    let mut buf = [0u8; 4096];
    let n = unsafe { libc::read(ino, buf.as_mut_ptr().cast(), buf.len()) };
    let err = std::io::Error::last_os_error();
    unsafe { libc::close(ino) };
    assert!(
        n > 0 || err.kind() == std::io::ErrorKind::WouldBlock,
        "{err}; {}",
        String::from_utf8_lossy(&out.stderr)
    );
    n > 0
}

/// A probe child never opens the library path it is given, so it can't load the library, not
/// even to discard it before a hand-over. The control: the same binary not as a probe child
/// opens it, where it loads Cisco's binary.
#[cfg(target_os = "linux")]
#[test]
fn a_probe_child_never_opens_the_library_it_is_given() {
    let dir = std::env::temp_dir().join(format!("sf-probe-lib-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let lib = dir.join("libopenh264.so");
    std::fs::write(&lib, b"not a library").unwrap();
    let mut child = starfold();
    child
        .args([FLAG, "bogus"])
        .env("STARFOLD_OPENH264_LIB", &lib);
    assert!(!opens(&lib, child), "a probe child opened the library");
    let mut server = starfold();
    server.arg("--version").env("STARFOLD_OPENH264_LIB", &lib);
    assert_eq!(opens(&lib, server), cfg!(openh264_cisco_loadable));
    std::fs::remove_dir_all(&dir).unwrap();
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
