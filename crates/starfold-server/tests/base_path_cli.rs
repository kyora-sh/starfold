//! `--base-path` through the real binary: every spelling, form and repetition of the flag
//! either stops startup with exit code 2 and its reason on stderr, or serves under exactly
//! the normalised prefix, prints its link with it, and serves nothing at the root. Without
//! the flag it serves at the root, as before.
//!
//! The cases that stop need nothing. The cases that serve need an X display (the server
//! captures one); they run when `STARFOLD_SERVE_TESTS` is set, as CI's "Startup on Xvfb"
//! step does, and are skipped otherwise.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_starfold");

/// A server process, killed when dropped, and what it has written to stderr so far.
struct Server(Child, Arc<Mutex<String>>);

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
fn quiet(port: u16) -> Vec<OsString> {
    [
        "--addr=127.0.0.1".to_string(),
        format!("--port={port}"),
        "--enable-basic-auth=false".into(),
        "--audio-enabled=false".into(),
        "--encoder-probe=false".into(),
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

fn os(v: &[&str]) -> Vec<OsString> {
    v.iter().map(OsString::from).collect()
}

/// Runs the binary with `args` until it exits, at most 15 s.
fn run_to_exit(args: &[OsString]) -> Output {
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

/// Asserts `out` is a refusal: exit 2, nothing on stdout, and on stderr the one line
/// `starfold: --base-path: ...` with `reason` in it.
fn assert_refused(out: &Output, reason: &str, what: &str) {
    let (o, e) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(out.status.code(), Some(2), "{what}: stdout {o} stderr {e}");
    assert!(o.is_empty(), "{what}: printed {o}");
    assert!(
        e.starts_with("starfold: --base-path: ") && e.contains(reason) && e.lines().count() == 1,
        "{what}: stderr {e:?}, wanted {reason:?}"
    );
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

/// Starts a server with `flags` and waits until `ready` answers; None if it never does.
fn serve(port: u16, flags: &[OsString], ready: &str) -> Option<Server> {
    let mut child = Command::new(BIN)
        .args([quiet(port), flags.to_vec()].concat())
        .env("RUST_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start starfold");
    let log = Arc::new(Mutex::new(String::new()));
    let (err, sink) = (child.stderr.take().unwrap(), log.clone());
    std::thread::spawn(move || {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            let mut l = sink.lock().unwrap();
            l.push_str(&line);
            l.push('\n');
        }
    });
    let server = Server(child, log);
    let end = Instant::now() + Duration::from_secs(60);
    while Instant::now() < end {
        if status(port, ready).is_some() {
            return Some(server);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// How the flag is written once.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Form {
    Equals,
    Space,
}

/// The ways to write the flag once with `value`: both spellings, `=` and space forms.
fn writings(value: &str) -> Vec<(Vec<OsString>, Form)> {
    let mut out = Vec::new();
    for flag in ["--base-path", "--base_path"] {
        out.push((os(&[&format!("{flag}={value}")]), Form::Equals));
        out.push((os(&[flag, value]), Form::Space));
    }
    out
}

/// What a case should do: stop with exit 2 for this reason, or serve under this prefix.
#[derive(Debug, Clone, PartialEq)]
enum Want {
    Refused(&'static str),
    Serves(&'static str),
}

const SHAPE: &str = "has to be / and parts of letters";
const NO_PREFIX: &str = "is no prefix; leave the flag out to serve at the root";
const NO_VALUE: &str = "needs a value, such as /desktop";
const TWICE: &str = "given 2 times; give it once";
const TOO_LONG: &str = "1025 bytes is longer than the 1024 a prefix may be";

/// The matrix: the flag once in every writing of each value, and twice in the repetitions
/// that must not be resolved by taking one of them.
fn cases() -> Vec<(Vec<OsString>, Want)> {
    let long = format!("/{}", "a".repeat(1024));
    let once: Vec<(&str, Want)> = vec![
        ("", Want::Refused(NO_PREFIX)),
        ("/", Want::Refused(NO_PREFIX)),
        ("//", Want::Refused(SHAPE)),
        ("/d", Want::Serves("/d")),
        ("/d/", Want::Serves("/d")),
        ("desk", Want::Refused(SHAPE)),
        ("/a//b", Want::Refused(SHAPE)),
        ("-h", Want::Refused(SHAPE)),
        ("--help", Want::Refused(SHAPE)),
        (long.as_str(), Want::Refused(TOO_LONG)),
    ];
    let mut out = Vec::new();
    for (value, want) in once {
        for (w, form) in writings(value) {
            // In the space form a value starting with -- is a flag of its own.
            let want = match (form, value.starts_with("--")) {
                (Form::Space, true) => Want::Refused(NO_VALUE),
                _ => want.clone(),
            };
            out.push((w, want));
        }
    }
    for (first, second) in [("/d", "/d"), ("/d", "/e"), ("/d", ""), ("//", "/d"), ("/d", "/")] {
        for (a, _) in writings(first) {
            for (b, _) in writings(second) {
                out.push(([a.clone(), b].concat(), Want::Refused(TWICE)));
            }
        }
    }
    out.push((os(&["--base-path"]), Want::Refused(NO_VALUE)));
    out.push((os(&["--base-path", "--read-only"]), Want::Refused(NO_VALUE)));
    out
}

#[test]
fn every_refused_writing_stops_with_exit_2_and_its_reason() {
    let mut n = 0;
    for (flags, want) in cases() {
        let Want::Refused(reason) = want else { continue };
        let out = run_to_exit(&[quiet(free_port()), flags.clone()].concat());
        assert_refused(&out, reason, &format!("{flags:?}"));
        n += 1;
    }
    assert_eq!(n, 114);
}

/// Exactly these help and version invocations, as the PR body lists them.
#[test]
fn help_and_version_never_hide_a_refused_prefix() {
    for (flags, reason) in [
        (&["--base-path", "--help"][..], NO_VALUE),
        (&["--base-path=//", "--version"], SHAPE),
        (&["-h", "--base-path="], NO_PREFIX),
        (&["--base-path", "-h"], SHAPE),
        (&["-V", "--base-path=/d", "--base-path=/e"], TWICE),
        (&["--help", "--base_path", "/"], NO_PREFIX),
    ] {
        assert_refused(&run_to_exit(&os(flags)), reason, &format!("{flags:?}"));
    }
    for flags in [
        &["--help", "--base-path=/ok"][..],
        &["--base-path=/ok", "-h"],
    ] {
        let out = run_to_exit(&os(flags));
        let o = String::from_utf8_lossy(&out.stdout);
        assert_eq!(out.status.code(), Some(0), "{flags:?}");
        assert!(o.contains("\nUsage: starfold"), "{flags:?}: help? {o}");
    }
    for flags in [
        &["--version", "--base_path", "/ok"][..],
        &["-V", "--base-path=/ok"],
    ] {
        let out = run_to_exit(&os(flags));
        let o = String::from_utf8_lossy(&out.stdout);
        assert_eq!(out.status.code(), Some(0), "{flags:?}");
        assert!(
            o.starts_with("starfold ") && o.contains(" (encoders: ") && o.lines().count() == 1,
            "{flags:?}: version? {o}"
        );
        assert!(!o.contains("Usage:"), "{flags:?}: printed the help: {o}");
    }
}

#[cfg(unix)]
#[test]
fn an_argument_that_is_not_utf8_stops_with_exit_2() {
    use std::os::unix::ffi::OsStringExt;
    let out = run_to_exit(&[
        OsString::from("--base-path"),
        OsString::from_vec(b"/d\xff".to_vec()),
    ]);
    let (o, e) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(out.status.code(), Some(2), "stderr {e}");
    assert!(o.is_empty(), "{o}");
    assert!(
        e.starts_with("starfold: argument 2 (") && e.contains("is not UTF-8"),
        "{e:?}"
    );
}

fn serving() -> bool {
    let on = std::env::var_os("STARFOLD_SERVE_TESTS").is_some();
    if !on {
        eprintln!("skipped: serving needs a display (set STARFOLD_SERVE_TESTS with one)");
    }
    on
}

/// The line the server logs with its link, from its stderr.
fn link_line(server: &Server) -> String {
    server
        .1
        .lock()
        .unwrap()
        .lines()
        .find(|l| l.contains("login off"))
        .unwrap_or_default()
        .to_string()
}

#[test]
fn every_accepted_writing_serves_under_its_prefix_and_not_at_the_root() {
    if !serving() {
        return;
    }
    let mut n = 0;
    for (flags, want) in cases() {
        let Want::Serves(prefix) = want else { continue };
        let port = free_port();
        let inside = format!("{prefix}/healthz");
        let server = serve(port, &flags, &inside).unwrap_or_else(|| panic!("{flags:?}: no answer"));
        assert_eq!(status(port, &inside), Some(200), "{flags:?}: {inside}");
        assert_eq!(status(port, "/healthz"), Some(404), "{flags:?}: /healthz");
        assert_eq!(status(port, prefix), Some(308), "{flags:?}: {prefix}");
        let link = format!("anyone who reaches http://127.0.0.1:{port}{prefix}/ can use");
        assert!(link_line(&server).contains(&link), "{flags:?}: {}", link_line(&server));
        n += 1;
    }
    assert_eq!(n, 8);
}

#[test]
fn without_the_flag_it_serves_at_the_root() {
    if !serving() {
        return;
    }
    let port = free_port();
    let server = serve(port, &[], "/healthz").expect("no answer");
    assert_eq!(status(port, "/healthz"), Some(200));
    assert_eq!(status(port, "/d/healthz"), Some(404));
    let link = format!("anyone who reaches http://127.0.0.1:{port}/ can use");
    assert!(link_line(&server).contains(&link), "{}", link_line(&server));
    let listening = format!("listening on http://127.0.0.1:{port}\n");
    assert!(server.1.lock().unwrap().contains(&listening), "{}", server.1.lock().unwrap());
}
