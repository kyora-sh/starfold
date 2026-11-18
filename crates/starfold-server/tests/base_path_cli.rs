//! `--base-path` through the real binary: every spelling, form and repetition of the flag
//! either stops startup with exit code 2 and a message on stderr, or serves under exactly the
//! normalised prefix and nothing at the root. No case where the flag was given ends serving
//! at the root.
//!
//! The cases that stop need nothing. The cases that serve need an X display (the server
//! captures one); they run when `STARFOLD_SERVE_TESTS` is set, as CI's "Startup on Xvfb"
//! step does, and are skipped otherwise.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_starfold");

/// A server process, killed when dropped.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Flags every run gets besides the ones under test: loopback only, no password, no audio,
/// no hardware probe.
fn quiet(port: u16) -> Vec<String> {
    [
        "--addr=127.0.0.1".to_string(),
        format!("--port={port}"),
        "--enable-basic-auth=false".into(),
        "--audio-enabled=false".into(),
        "--encoder-probe=false".into(),
    ]
    .into()
}

/// Runs the binary with `args` until it exits, at most 15 s.
fn run_to_exit(args: &[String]) -> Output {
    let mut child = Command::new(BIN)
        .args(args)
        .env("RUST_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start starfold");
    let end = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > end {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!(
                "{args:?} still running after 15 s; stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

/// The status of a GET of `path`, or None if nothing answers.
fn status(port: u16, path: &str) -> Option<u16> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf)
        .split(' ')
        .nth(1)?
        .parse()
        .ok()
}

/// The ways to write the flag once with `value`: both spellings, `=` and space forms.
fn writings(value: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for flag in ["--base-path", "--base_path"] {
        out.push(vec![format!("{flag}={value}")]);
        out.push(vec![flag.to_string(), value.to_string()]);
    }
    out
}

/// What a case should do: stop with exit 2, or serve under this prefix.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Want {
    Refused,
    Serves(&'static str),
}

/// The matrix: the flag once in every writing of each value, and twice in the repetitions
/// that must not be resolved by taking one of them.
fn cases() -> Vec<(Vec<String>, Want)> {
    let once: [(&str, Want); 9] = [
        ("", Want::Refused),
        ("/", Want::Refused),
        ("//", Want::Refused),
        ("/d", Want::Serves("/d")),
        ("/d/", Want::Serves("/d")),
        ("desk", Want::Refused),
        ("/a//b", Want::Refused),
        ("-h", Want::Refused),
        ("--help", Want::Refused),
    ];
    let mut out = Vec::new();
    for (value, want) in once {
        for w in writings(value) {
            out.push((w, want));
        }
    }
    let twice: [(&str, &str); 5] = [
        ("/d", "/d"),
        ("/d", "/e"),
        ("/d", ""),
        ("//", "/d"),
        ("/d", "/"),
    ];
    for (first, second) in twice {
        for a in writings(first) {
            for b in writings(second) {
                out.push(([a.clone(), b].concat(), Want::Refused));
            }
        }
    }
    out.push((vec!["--base-path".into()], Want::Refused));
    out.push((
        vec!["--base-path".into(), "--read-only".into()],
        Want::Refused,
    ));
    out
}

#[test]
fn every_refused_writing_stops_with_exit_2_and_says_why() {
    let mut n = 0;
    for (flags, want) in cases() {
        if want != Want::Refused {
            continue;
        }
        let args = [quiet(free_port()), flags.clone()].concat();
        let out = run_to_exit(&args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{flags:?}: stderr {err}");
        assert!(err.contains("--base-path"), "{flags:?}: stderr {err}");
        n += 1;
    }
    assert!(n > 100, "{n} cases");
}

#[test]
fn help_and_version_never_hide_a_refused_prefix() {
    for (flags, code, stdout) in [
        (&["--base-path", "--help"][..], 2, ""),
        (&["--base-path=//", "--version"], 2, ""),
        (&["-h", "--base-path="], 2, ""),
        (&["--base-path", "-h"], 2, ""),
        (&["--help", "--base-path=/ok"], 0, "Usage:"),
        (&["--base-path=/ok", "-h"], 0, "Usage:"),
        (&["--version", "--base_path", "/ok"], 0, "starfold "),
    ] {
        let args: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
        let out = run_to_exit(&args);
        let (o, e) = (
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        assert_eq!(out.status.code(), Some(code), "{flags:?}: {o}{e}");
        assert!(o.contains(stdout), "{flags:?}: {o}");
        if code == 2 {
            assert!(e.contains("--base-path"), "{flags:?}: {e}");
        }
    }
}

#[test]
fn every_accepted_writing_serves_under_its_prefix_and_not_at_the_root() {
    if std::env::var_os("STARFOLD_SERVE_TESTS").is_none() {
        eprintln!("skipped: serving needs a display (set STARFOLD_SERVE_TESTS with one)");
        return;
    }
    let mut n = 0;
    for (flags, want) in cases() {
        let Want::Serves(prefix) = want else { continue };
        let port = free_port();
        let args = [quiet(port), flags.clone()].concat();
        let server = Server(
            Command::new(BIN)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("start starfold"),
        );
        let inside = format!("{prefix}/healthz");
        let end = Instant::now() + Duration::from_secs(60);
        let mut got = None;
        while Instant::now() < end {
            got = status(port, &inside);
            if got.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(got, Some(200), "{flags:?}: {inside}");
        assert_eq!(status(port, "/healthz"), Some(404), "{flags:?}: /healthz");
        assert_eq!(status(port, prefix), Some(308), "{flags:?}: {prefix}");
        drop(server);
        n += 1;
    }
    assert_eq!(n, 8);
}
