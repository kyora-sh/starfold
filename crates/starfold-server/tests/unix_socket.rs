//! The starfold binary with --listen-unix, started as a server: who gets through the
//! socket, what a startup that fails or is stopped leaves behind, and the signal mask.
//! Linux only. These tests need Xvfb, python3, unprivileged user namespaces (`unshare
//! --user`), passwordless `sudo -n` (to make a throwaway user and run as it) and bindfs
//! (a FUSE mount); without them they fail, they don't skip.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Set for a run of this test binary as the second same-uid process of
/// [`a_second_process_of_the_same_user_gets_ws_and_a_screenshot`]: the socket to use.
const SAME_UID_CLIENT: &str = "STARFOLD_TEST_SAME_UID_CLIENT";

const REFUSAL: &str = "--listen-unix: refusing: /proc/self/uid_map holds ";

fn scratch(name: &str, mode: u32) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sf-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir(&d).unwrap();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode)).unwrap();
    d
}

fn wait_for<T>(what: &str, within: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(start.elapsed() < within, "{what}: not within {within:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn exited(child: &mut Child, within: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    while start.elapsed() < within {
        if let Some(s) = child.try_wait().unwrap() {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn sudo_works() {
    let ok = Command::new("sudo")
        .args(["-n", "true"])
        .status()
        .is_ok_and(|s| s.success());
    assert!(
        ok,
        "this test needs passwordless `sudo -n`: it runs a process as another uid"
    );
}

fn own_uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// An Xvfb display of its own for one test, on the first free display number.
struct Display {
    xvfb: Child,
    name: String,
}

impl Display {
    fn start() -> Display {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let (r, w) = (fds[0], fds[1]);
        let mut cmd = Command::new("Xvfb");
        cmd.args(["-displayfd", &w.to_string()])
            .args([
                "-screen",
                "0",
                "1280x800x24",
                "-ac",
                "-nolisten",
                "tcp",
                "-noreset",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            cmd.pre_exec(move || match libc::fcntl(w, libc::F_SETFD, 0) {
                0 => Ok(()),
                _ => Err(std::io::Error::last_os_error()),
            });
        }
        let spawned = cmd.spawn();
        unsafe { libc::close(w) };
        let xvfb = spawned.unwrap_or_else(|e| panic!("these tests need Xvfb: {e}"));
        let mut from = unsafe { std::fs::File::from_raw_fd(r) };
        let mut number = Vec::new();
        let mut b = [0u8];
        while from.read(&mut b).unwrap() == 1 && b[0] != b'\n' {
            number.push(b[0]);
        }
        let number = String::from_utf8(number).unwrap();
        assert!(!number.is_empty(), "Xvfb named no display");
        Display {
            xvfb,
            name: format!(":{number}"),
        }
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.xvfb.kill();
        let _ = self.xvfb.wait();
    }
}

/// The binary with login, audio and the encoder probe off, its log (info) in `log`, on
/// `display` if there is one.
fn starfold(display: Option<&Display>, args: &[String], log: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_starfold"));
    cmd.args([
        "--enable-basic-auth=false",
        "--audio-enabled=false",
        "--encoder-probe=false",
    ])
    .args(args)
    .env("RUST_LOG", "info")
    .env_remove("DISPLAY")
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(std::fs::File::create(log).unwrap());
    if let Some(d) = display {
        cmd.env("DISPLAY", &d.name);
    }
    cmd
}

fn read_log(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

/// Writes `request` and reads to the end (or the first error, as a reset).
fn exchange(socket: &Path, request: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut s = UnixStream::connect(socket)?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    s.write_all(request)?;
    let mut reply = Vec::new();
    s.read_to_end(&mut reply)?;
    Ok(reply)
}

const HEALTHZ: &[u8] = b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

fn healthy(socket: &Path) -> bool {
    exchange(socket, HEALTHZ).is_ok_and(|r| r.starts_with(b"HTTP/1.1 200"))
}

/// A running server with --listen-unix `dir/sf`, stopped with SIGTERM when dropped.
struct Server {
    child: Child,
    log: PathBuf,
    socket: PathBuf,
}

impl Server {
    fn start(display: &Display, scratch: &Path, args: &[&str]) -> Server {
        let cmd = starfold(
            Some(display),
            &Self::args(scratch, args),
            &scratch.join("log"),
        );
        Self::start_with(cmd, scratch)
    }

    fn args(scratch: &Path, args: &[&str]) -> Vec<String> {
        let mut all = vec![format!("--listen-unix={}", scratch.join("sf").display())];
        all.extend(args.iter().map(|a| a.to_string()));
        all
    }

    fn start_with(mut cmd: Command, scratch: &Path) -> Server {
        let mut s = Server {
            child: cmd.spawn().unwrap(),
            log: scratch.join("log"),
            socket: scratch.join("sf").join("s"),
        };
        wait_for("the server listening", Duration::from_secs(90), || {
            if let Some(st) = s.child.try_wait().unwrap() {
                panic!("the server ended ({st}):\n{}", read_log(&s.log));
            }
            read_log(&s.log)
                .contains("listening on unix:")
                .then_some(())
        });
        assert!(healthy(&s.socket), "{}", read_log(&s.log));
        s
    }

    fn pid(&self) -> libc::pid_t {
        self.child.id() as libc::pid_t
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        unsafe { libc::kill(self.pid(), libc::SIGTERM) };
        if exited(&mut self.child, Duration::from_secs(10)).is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A user made for one test (`useradd` through `sudo -n`), removed with `userdel` when
/// dropped.
struct Throwaway {
    name: String,
    uid: u32,
}

impl Throwaway {
    fn create() -> Throwaway {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("sf-test-{}-{n}", std::process::id());
        let made = Command::new("sudo")
            .args([
                "-n",
                "useradd",
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
            ])
            .arg(&name)
            .output()
            .unwrap();
        // useradd refuses a name that exists (exit 9): an account this test didn't make is
        // never touched, and only one it made is deleted on drop.
        assert!(
            made.status.success(),
            "useradd {name}: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        let id = Command::new("id").args(["-u", &name]).output().unwrap();
        let uid = String::from_utf8(id.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        Throwaway { name, uid }
    }
}

impl Drop for Throwaway {
    fn drop(&mut self) {
        let gone = Command::new("sudo")
            .args(["-n", "userdel", &self.name])
            .status()
            .is_ok_and(|s| s.success());
        if !gone {
            eprintln!("userdel {} failed", self.name);
        }
    }
}

/// In a user namespace of its own (`unshare --user`), with no uid mapped and with only
/// root mapped to this uid, the binary refuses --listen-unix before it binds, quoting the
/// uid_map it read; the directory is never made.
#[test]
fn without_the_identity_uid_map_the_unix_listener_is_refused() {
    let at = Path::new("/tmp").join(format!("sf-it-userns-{}-sf", std::process::id()));
    for (flags, map) in [
        (&["--user"][..], String::new()),
        (
            &["--user", "--map-root-user"][..],
            format!("{:>10} {:>10} {:>10}\\n", 0, own_uid(), 1),
        ),
    ] {
        let probe = Command::new("unshare")
            .args(flags)
            .args(["--", "true"])
            .output()
            .unwrap_or_else(|e| panic!("this test needs unshare (util-linux): {e}"));
        assert!(
            probe.status.success(),
            "this test needs unprivileged user namespaces: {}",
            String::from_utf8_lossy(&probe.stderr)
        );
        let out = Command::new("unshare")
            .args(flags)
            .arg("--")
            .arg(env!("CARGO_BIN_EXE_starfold"))
            .arg(format!("--listen-unix={}", at.display()))
            .args(["--listen-tcp=false", "--enable-basic-auth=false"])
            .env_remove("DISPLAY")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{flags:?}: {said}");
        assert!(
            said.contains(&format!("{REFUSAL}\"{map}\";")),
            "{flags:?}: {said}"
        );
        assert!(!at.exists(), "{flags:?}: {} was made", at.display());
    }
}

/// Runs the binary with --listen-unix at `at` and TCP off, and returns its exit code and
/// what it printed; it is refused before any desktop is touched.
fn refused_start(at: &Path) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_starfold"))
        .arg(format!("--listen-unix={}", at.display()))
        .args(["--listen-tcp=false", "--enable-basic-auth=false"])
        .env_remove("DISPLAY")
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Another user makes the directory first (in sticky /tmp, where it then can't be moved by
/// us or replaced by anyone else): starfold refuses to start and names its owner and mode,
/// and the directory is left as it was. Clients that connect by path check the listener's
/// uid themselves (README), so the impostor gets nothing from them either.
#[test]
fn a_directory_another_user_made_first_is_refused() {
    sudo_works();
    let user = Throwaway::create();
    let at = Path::new("/tmp").join(format!("sf-it-theirs-{}", std::process::id()));
    let as_them = |args: &[&str]| {
        let ok = Command::new("sudo")
            .args(["-n", "-u", &user.name, "--"])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "{args:?} as {}", user.name);
    };
    use std::os::unix::fs::MetadataExt;
    // 0700: starfold can't open it, and still names its owner and mode.
    for mode in [0o755, 0o700] {
        as_them(&["mkdir", "-m", &format!("{mode:o}"), at.to_str().unwrap()]);
        let (code, said) = refused_start(&at);
        assert_eq!(code, Some(1), "{mode:o}: {said}");
        assert!(
            said.contains(&format!(
                "{} is in the way (a directory, owner uid {}, mode {mode:04o})",
                at.display(),
                user.uid
            )),
            "{mode:o}: {said}"
        );
        let meta = std::fs::metadata(&at).unwrap();
        assert_eq!((meta.uid(), meta.mode() & 0o7777), (user.uid, mode));
        as_them(&["rmdir", at.to_str().unwrap()]);
    }
}

/// A symlink on the way that belongs to another user (here in a sticky directory of ours,
/// where anyone may add entries) is not followed: starfold refuses to start, naming it, and
/// makes nothing where it points.
#[test]
fn a_symlink_another_user_owns_is_not_followed() {
    sudo_works();
    let user = Throwaway::create();
    let d = scratch("their-link", 0o1777);
    let target = scratch("their-link-target", 0o700);
    let link = d.join("link");
    let ok = Command::new("sudo")
        .args(["-n", "-u", &user.name, "--", "ln", "-s"])
        .arg(&target)
        .arg(&link)
        .status()
        .unwrap()
        .success();
    assert!(ok, "ln -s as {}", user.name);
    let (code, said) = refused_start(&link.join("sf"));
    assert_eq!(code, Some(1), "{said}");
    assert!(
        said.contains(&format!(
            "{} is a symlink that belongs to user {}",
            link.display(),
            user.uid
        )),
        "{said}"
    );
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    let _ = Command::new("sudo")
        .args(["-n", "rm", "-f"])
        .arg(&link)
        .status();
    std::fs::remove_dir_all(&d).unwrap();
    std::fs::remove_dir_all(&target).unwrap();
}

/// A server killed with SIGKILL leaves its directory and dead socket; the next start on the
/// same path takes them back and serves.
#[test]
fn a_restart_after_a_kill_takes_the_directory_back() {
    let x = Display::start();
    let d = scratch("restart", 0o700);
    let mut first = Server::start(&x, &d, &["--listen-tcp=false"]);
    unsafe { libc::kill(first.pid(), libc::SIGKILL) };
    first.child.wait().unwrap();
    let socket = first.socket.clone();
    // Reaped already: its Drop would signal a pid that may be someone else's by now.
    std::mem::forget(first);
    assert!(socket.exists(), "a killed server left nothing behind");
    assert!(!healthy(&socket));
    std::fs::remove_file(d.join("log")).unwrap();
    let second = Server::start(&x, &d, &["--listen-tcp=false"]);
    assert!(healthy(&second.socket));
}

/// A second instance on the path of a live one is refused and takes nothing from it.
#[test]
fn a_second_instance_is_refused_and_the_first_keeps_serving() {
    let x = Display::start();
    let d = scratch("second-instance", 0o700);
    let first = Server::start(&x, &d, &["--listen-tcp=false"]);
    let (code, said) = refused_start(&d.join("sf"));
    assert_eq!(code, Some(1), "{said}");
    assert!(said.contains("another server is using it"), "{said}");
    assert!(healthy(&first.socket), "{}", read_log(&first.log));
}

/// A path on a FUSE mount (bindfs, mounted by root with allow_other over a directory of
/// ours, so every owner and mode on it looks like ours) is refused, naming the mount, and
/// nothing is made in what it mirrors. This test needs bindfs.
#[test]
fn a_path_on_a_fuse_mount_is_refused() {
    sudo_works();
    let src = scratch("fuse-src", 0o700);
    let mnt = scratch("fuse-mnt", 0o700);
    let mounted = Command::new("sudo")
        .args(["-n", "bindfs", "-o", "allow_other"])
        .arg(&src)
        .arg(&mnt)
        .status()
        .is_ok_and(|s| s.success());
    assert!(mounted, "this test needs bindfs (a FUSE filesystem)");
    struct Unmount(PathBuf);
    impl Drop for Unmount {
        fn drop(&mut self) {
            let _ = Command::new("sudo")
                .args(["-n", "umount"])
                .arg(&self.0)
                .status();
        }
    }
    let unmount = Unmount(mnt.clone());
    let (code, said) = refused_start(&mnt.join("sf"));
    assert_eq!(code, Some(1), "{said}");
    assert!(
        said.contains(&format!("{} is on a FUSE mount", mnt.display())),
        "{said}"
    );
    assert_eq!(std::fs::read_dir(&src).unwrap().count(), 0);
    drop(unmount);
    std::fs::remove_dir_all(&src).unwrap();
    std::fs::remove_dir_all(&mnt).unwrap();
}

/// A client running as a throwaway user
/// reaches the socket (mode 0666, directory 0711) of the real server and gets nothing: the
/// connection closes before a byte either way, and the server logs the refusal. The
/// server's own uid still gets in.
#[test]
fn another_user_gets_nothing_through_the_socket() {
    sudo_works();
    let user = Throwaway::create();
    assert_ne!(user.uid, own_uid());
    let x = Display::start();
    let d = scratch("other-user", 0o711);
    let server = Server::start(&x, &d, &["--listen-unix-mode=0666", "--listen-tcp=false"]);
    // The server closes with the request unread, so the read ends with EOF or a reset:
    // either way, what counts is the bytes that came before it.
    let client = "import socket, sys\n\
        s = socket.socket(socket.AF_UNIX)\n\
        s.connect(sys.argv[1])\n\
        s.settimeout(5)\n\
        data = b''\n\
        try:\n    s.sendall(b'GET /healthz HTTP/1.1\\r\\nHost: localhost\\r\\nConnection: close\\r\\n\\r\\n')\n    \
        while True:\n        b = s.recv(100)\n        if not b:\n            break\n        \
        data += b\n\
        except (ConnectionResetError, BrokenPipeError, TimeoutError):\n    pass\n\
        print(len(data))\n";
    let out = Command::new("sudo")
        .args(["-n", "-u", &user.name, "--", "python3", "-c", client])
        .arg(&server.socket)
        .current_dir("/")
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(said.trim(), "0", "the other user read {said} bytes");
    let refused = format!("refused a connection from uid {}", user.uid);
    wait_for("the refusal in the log", Duration::from_secs(10), || {
        read_log(&server.log).contains(&refused).then_some(())
    });
    assert!(healthy(&server.socket), "{}", read_log(&server.log));
    drop(server);
    std::fs::remove_dir_all(&d).unwrap();
}

/// A second process of the server's uid (this test binary run again, with
/// [`SAME_UID_CLIENT`] set) connects to the real server's socket directly: `/ws` upgrades,
/// the server's hello arrives and a ping is answered, and `GET /screenshot` returns a PNG.
#[test]
fn a_second_process_of_the_same_user_gets_ws_and_a_screenshot() {
    let x = Display::start();
    let d = scratch("same-user", 0o700);
    let server = Server::start(&x, &d, &["--listen-tcp=false"]);
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "same_uid_client",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(SAME_UID_CLIENT, &server.socket)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{said}\n{}\n{}",
        String::from_utf8_lossy(&out.stderr),
        read_log(&server.log)
    );
    assert!(
        said.contains("same-uid client ok: screenshot png, ws hello and pong"),
        "{said}"
    );
    drop(server);
    std::fs::remove_dir_all(&d).unwrap();
}

/// The second process of [`a_second_process_of_the_same_user_gets_ws_and_a_screenshot`];
/// does nothing unless [`SAME_UID_CLIENT`] is set.
#[tokio::test]
async fn same_uid_client() {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    let Some(socket) = std::env::var_os(SAME_UID_CLIENT).map(PathBuf::from) else {
        return;
    };
    let shot = {
        let socket = socket.clone();
        tokio::task::spawn_blocking(move || {
            exchange(
                &socket,
                b"GET /screenshot HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
        })
        .await
        .unwrap()
        .unwrap()
    };
    let head_end = shot
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("an HTTP reply");
    let head = String::from_utf8_lossy(&shot[..head_end]).to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(head.contains("content-type: image/png"), "{head}");
    assert!(shot[head_end + 4..].starts_with(b"\x89PNG\r\n\x1a\n"));

    let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let (mut ws, _) = tokio_tungstenite::client_async("ws://localhost/ws", stream)
        .await
        .unwrap();
    loop {
        if let Message::Text(t) = next(&mut ws).await {
            let j: serde_json::Value = serde_json::from_str(&t).unwrap();
            if j["t"] == "hello" {
                break;
            }
        }
    }
    ws.send(Message::Ping(bytes::Bytes::from_static(b"same-uid")))
        .await
        .unwrap();
    loop {
        if let Message::Pong(p) = next(&mut ws).await {
            assert_eq!(&p[..], b"same-uid");
            break;
        }
    }
    let _ = ws.close(None).await;
    println!("same-uid client ok: screenshot png, ws hello and pong");
}

async fn next(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>,
) -> tokio_tungstenite::tungstenite::Message {
    use futures_util::StreamExt;
    tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("nothing within 10 s")
        .expect("the server hung up")
        .expect("a WebSocket error")
}

/// Sends a WebSocket upgrade for /ws with these headers and returns the reply's status.
fn upgrade_status(mut s: impl Read + Write, host: &str, origin: Option<&str>) -> u16 {
    let origin = origin.map_or(String::new(), |o| format!("Origin: {o}\r\n"));
    write!(
        s,
        "GET /ws HTTP/1.1\r\nHost: {host}\r\n{origin}Connection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    )
    .unwrap();
    let mut line = Vec::new();
    let mut b = [0u8];
    while !line.ends_with(b"\r\n") && s.read(&mut b).unwrap() == 1 {
        line.push(b[0]);
    }
    let line = String::from_utf8_lossy(&line);
    line.split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status in {line:?}"))
}

/// On `/ws` over the real server's unix socket, `Host: localhost` with no `Origin` is
/// served, and another site's `Origin` or a host name not listed is refused: the same
/// answers as over its TCP listener.
#[test]
fn hosts_and_origins_on_ws_are_checked_over_the_socket_as_over_tcp() {
    let x = Display::start();
    let d = scratch("host-origin", 0o700);
    let port = free_port();
    let server = Server::start(&x, &d, &["--addr=127.0.0.1", &format!("--port={port}")]);
    for (host, origin, want) in [
        ("localhost", None, 101),
        ("localhost", Some("https://elsewhere.example"), 403),
        ("elsewhere.example", None, 403),
    ] {
        let timeout = Some(Duration::from_secs(30));
        let unix = UnixStream::connect(&server.socket).unwrap();
        unix.set_read_timeout(timeout).unwrap();
        let tcp = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        tcp.set_read_timeout(timeout).unwrap();
        let over_unix = upgrade_status(unix, host, origin);
        let over_tcp = upgrade_status(tcp, host, origin);
        assert_eq!(over_unix, want, "unix: Host {host}, Origin {origin:?}");
        assert_eq!(over_tcp, want, "tcp: Host {host}, Origin {origin:?}");
    }
    drop(server);
    std::fs::remove_dir_all(&d).unwrap();
}

/// The TCP port is taken after the unix socket is bound: the server says so and exits 1
/// through `exit`, which removes the socket and its directory.
#[test]
fn an_occupied_port_after_the_unix_bind_leaves_nothing_behind() {
    let x = Display::start();
    let d = scratch("port-taken", 0o700);
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let log = d.join("log");
    let args = Server::args(&d, &["--addr=127.0.0.1", &format!("--port={port}")]);
    let mut child = starfold(Some(&x), &args, &log).spawn().unwrap();
    let status = exited(&mut child, Duration::from_secs(90)).unwrap_or_else(|| {
        let _ = child.kill();
        panic!("still running:\n{}", read_log(&log))
    });
    let said = read_log(&log);
    assert_eq!(status.code(), Some(1), "{status}\n{said}");
    assert!(
        said.contains(&format!("listening on 127.0.0.1:{port}: ")),
        "{said}"
    );
    assert!(!d.join("sf").exists(), "{said}");
    drop(held);
    std::fs::remove_dir_all(&d).unwrap();
}

/// Startup waits on a read that never ends (--favicon is a FIFO whose writer sends nothing)
/// after the unix bind: SIGTERM still ends the process, and the socket and its directory go.
#[test]
fn sigterm_while_startup_waits_on_a_fifo_removes_the_socket() {
    let x = Display::start();
    let d = scratch("fifo", 0o700);
    let fifo = d.join("icon.ico");
    let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let log = d.join("log");
    let args = Server::args(
        &d,
        &[
            "--listen-tcp=false",
            &format!("--favicon={}", fifo.display()),
        ],
    );
    let mut child = starfold(Some(&x), &args, &log).spawn().unwrap();
    // A writer gets the FIFO without waiting only once the server has it open to read.
    let writer = wait_for(
        "the server opening the FIFO",
        Duration::from_secs(90),
        || {
            if let Some(st) = child.try_wait().unwrap() {
                panic!("the server ended ({st}):\n{}", read_log(&log));
            }
            std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo)
                .ok()
        },
    );
    assert!(d.join("sf").join("s").exists());
    assert!(!read_log(&log).contains("listening on unix:"));
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let status = exited(&mut child, Duration::from_secs(20));
    drop(writer);
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&d);
        panic!("SIGTERM did not end a startup waiting on the FIFO");
    };
    assert_eq!(status.code(), Some(0), "{status}\n{}", read_log(&log));
    assert!(!d.join("sf").exists(), "{}", read_log(&log));
    std::fs::remove_dir_all(&d).unwrap();
}

/// The main thread's blocked signals, from /proc.
fn blocked_signals(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let mask = status
        .lines()
        .find_map(|l| l.strip_prefix("SigBlk:"))
        .unwrap()
        .trim();
    u64::from_str_radix(mask, 16).unwrap()
}

/// Started with SIGTERM blocked and SIGINT not (as a parent can leave them, and as an
/// embedder's own mask can be), the server leaves both as they were once it serves:
/// TCP alone, and with the unix socket, which blocks the two for a while.
#[test]
fn the_shutdown_signals_come_out_as_they_were_given() {
    let x = Display::start();
    let bit = |sig: libc::c_int| 1u64 << (sig - 1);
    for unix in [false, true] {
        let d = scratch(if unix { "mask-unix" } else { "mask-tcp" }, 0o700);
        let log = d.join("log");
        let port = free_port();
        let args: Vec<String> = match unix {
            true => Server::args(&d, &["--listen-tcp=false"]),
            false => vec!["--addr=127.0.0.1".into(), format!("--port={port}")],
        };
        let mut cmd = starfold(Some(&x), &args, &log);
        unsafe {
            cmd.pre_exec(|| {
                let mut term: libc::sigset_t = std::mem::zeroed();
                let mut int: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut term);
                libc::sigemptyset(&mut int);
                libc::sigaddset(&mut term, libc::SIGTERM);
                libc::sigaddset(&mut int, libc::SIGINT);
                libc::pthread_sigmask(libc::SIG_BLOCK, &term, std::ptr::null_mut());
                libc::pthread_sigmask(libc::SIG_UNBLOCK, &int, std::ptr::null_mut());
                Ok(())
            });
        }
        let mut child = cmd.spawn().unwrap();
        let serving = match unix {
            true => "listening on unix:".to_string(),
            false => format!("listening on http://127.0.0.1:{port}"),
        };
        wait_for("the server listening", Duration::from_secs(90), || {
            if let Some(st) = child.try_wait().unwrap() {
                panic!("the server ended ({st}):\n{}", read_log(&log));
            }
            read_log(&log).contains(&serving).then_some(())
        });
        let mask = blocked_signals(child.id());
        let _ = child.kill();
        let _ = child.wait();
        assert_ne!(
            mask & bit(libc::SIGTERM),
            0,
            "unix {unix}: SIGTERM unblocked"
        );
        assert_eq!(mask & bit(libc::SIGINT), 0, "unix {unix}: SIGINT blocked");
        std::fs::remove_dir_all(&d).unwrap();
    }
}

/// `starfold attach dir` with its stdin, stdout and stderr piped to this test.
fn attach(dir: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_starfold"))
        .arg("attach")
        .arg(dir)
        .env("RUST_LOG", "trace")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// An attach's stdin and stdout as one stream, either end of which can be let go.
struct Piped {
    out: Option<std::process::ChildStdout>,
    into: Option<std::process::ChildStdin>,
}

impl Piped {
    fn of(child: &mut Child) -> Piped {
        Piped {
            out: child.stdout.take(),
            into: child.stdin.take(),
        }
    }
}

impl Read for Piped {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.out.as_mut().unwrap().read(buf)
    }
}

impl Write for Piped {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.into.as_mut().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.into.as_mut().unwrap().flush()
    }
}

fn stderr_of(child: &mut Child) -> String {
    let mut said = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut said)
        .unwrap();
    said
}

type Relayed = tokio_tungstenite::tungstenite::WebSocket<Piped>;

/// The next message on a WebSocket through attach, and whether it was one of the server's
/// pings.
fn next_through(ws: &mut Relayed) -> (tokio_tungstenite::tungstenite::Message, bool) {
    let m = ws.read().expect("the WebSocket through attach");
    let ping = matches!(&m, tokio_tungstenite::tungstenite::Message::Text(t) if t.starts_with(r#"{"t":"ping""#));
    (m, ping)
}

/// A WebSocket on `/ws` through a new attach to `dir`, past the server's hello.
fn ws_through(dir: &Path) -> (Child, Relayed) {
    let mut child = attach(dir);
    let (mut ws, reply) =
        tokio_tungstenite::tungstenite::client("ws://localhost/ws", Piped::of(&mut child))
            .expect("the upgrade through attach");
    assert_eq!(reply.status(), 101);
    loop {
        if let (tokio_tungstenite::tungstenite::Message::Text(t), _) = next_through(&mut ws) {
            let j: serde_json::Value = serde_json::from_str(&t).unwrap();
            if j["t"] == "hello" {
                break;
            }
        }
    }
    (child, ws)
}

/// Through `starfold attach` over a pipe pair: an HTTP GET gets the server's reply, and
/// nothing else, on stdout, with stdin held open until the server closes, after which
/// attach exits 0 with nothing on stderr; a WebSocket upgrade then carries the hello, the
/// server's pings, a ping of ours answered and a key. The end of stdin (no close frame)
/// ends the server's session, its cleanup runs (the client is logged as disconnected
/// after the keys are released), and attach exits 0.
#[test]
fn attach_relays_http_and_a_websocket() {
    use tokio_tungstenite::tungstenite::Message;
    let x = Display::start();
    let d = scratch("attach", 0o700);
    let server = Server::start(&x, &d, &["--listen-tcp=false"]);
    let dir = d.join("sf");

    let mut child = attach(&dir);
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(HEALTHZ).unwrap();
    let mut reply = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut reply)
        .unwrap();
    let status = exited(&mut child, Duration::from_secs(10)).expect("attach still running");
    drop(stdin);
    let said = stderr_of(&mut child);
    assert!(status.success(), "{status}: {said}");
    assert_eq!(said, "");
    let reply = String::from_utf8(reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 200 OK\r\n"), "{reply}");
    assert!(reply.ends_with("\r\n\r\nok"), "{reply}");

    let (mut child, mut ws) = ws_through(&dir);
    let mut pings = 0;
    while pings < 2 {
        pings += usize::from(next_through(&mut ws).1);
    }
    ws.send(Message::Ping(bytes::Bytes::from_static(b"through attach")))
        .unwrap();
    loop {
        if let (Message::Pong(p), _) = next_through(&mut ws) {
            assert_eq!(&p[..], b"through attach");
            break;
        }
    }
    ws.send(Message::Text("k,65,1".into())).unwrap();
    ws.flush().unwrap();
    let before = read_log(&server.log).matches(" disconnected").count();
    let pipes = ws.get_mut();
    drop(pipes.into.take());
    let mut out = pipes.out.take().unwrap();
    let drained = std::thread::spawn(move || {
        let mut rest = Vec::new();
        out.read_to_end(&mut rest).map(|_| ())
    });
    let status = exited(&mut child, Duration::from_secs(10)).expect("attach outlived stdin");
    let said = stderr_of(&mut child);
    assert!(status.success(), "{status}: {said}");
    assert_eq!(said, "");
    drained.join().unwrap().unwrap();
    wait_for("the session's cleanup", Duration::from_secs(10), || {
        (read_log(&server.log).matches(" disconnected").count() > before).then_some(())
    });
    drop(server);
    std::fs::remove_dir_all(&d).unwrap();
}

/// Attach to a directory with nothing listening in it, or with a bad command line: exit 1
/// (2 for the command line), the error on stderr and not a byte on stdout.
#[test]
fn attach_errors_go_to_stderr_alone() {
    let d = scratch("attach-nothing", 0o700);
    std::fs::create_dir(d.join("sf")).unwrap();
    std::fs::set_permissions(d.join("sf"), std::fs::Permissions::from_mode(0o700)).unwrap();
    for (args, code, want) in [
        (
            vec![d.join("sf").into_os_string()],
            1,
            format!("starfold attach {0}: {0}/s: ", d.join("sf").display()),
        ),
        (vec![], 2, "usage: starfold attach DIR".to_string()),
        (
            vec!["a".into(), "b".into()],
            2,
            "usage: starfold attach DIR".to_string(),
        ),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_starfold"))
            .arg("attach")
            .args(&args)
            .env("RUST_LOG", "trace")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(code), "{args:?}: {said}");
        assert!(said.starts_with(&want), "{args:?}: {said}");
        assert!(out.stdout.is_empty(), "{args:?}: stdout {:?}", out.stdout);
    }
    std::fs::remove_dir_all(&d).unwrap();
}

/// Another user puts a listening socket at `DIR/s` (here `DIR` is ours and sticky, so
/// the walk passes and anyone may add an entry): attach connects, reads the listener's
/// uid, refuses with exit 1 naming that uid, and sends nothing; the impostor reads 0
/// bytes.
#[test]
fn attach_refuses_a_listener_of_another_uid() {
    sudo_works();
    let user = Throwaway::create();
    assert_ne!(user.uid, own_uid());
    let d = scratch("attach-impostor", 0o711);
    let dir = d.join("sf");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).unwrap();
    let socket = dir.join("s");
    let impostor = "import os, socket, sys\n\
        s = socket.socket(socket.AF_UNIX)\n\
        s.bind(sys.argv[1])\n\
        os.chmod(sys.argv[1], 0o666)\n\
        s.listen(1)\n\
        s.settimeout(30)\n\
        c, _ = s.accept()\n\
        c.settimeout(10)\n\
        data = b''\n\
        try:\n    \
        while True:\n        b = c.recv(100)\n        if not b:\n            break\n        \
        data += b\n\
        except (ConnectionResetError, TimeoutError):\n    pass\n\
        print(len(data))\n";
    let them = Command::new("sudo")
        .args(["-n", "-u", &user.name, "--", "python3", "-c", impostor])
        .arg(&socket)
        .current_dir("/")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for("the impostor's socket", Duration::from_secs(10), || {
        std::fs::metadata(&socket)
            .is_ok_and(|m| m.permissions().mode() & 0o777 == 0o666)
            .then_some(())
    });
    let mut child = attach(&dir);
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(HEALTHZ);
    let mut out = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
    let status = exited(&mut child, Duration::from_secs(10)).expect("attach still running");
    drop(stdin);
    let said = stderr_of(&mut child);
    assert_eq!(status.code(), Some(1), "{said}");
    assert!(
        said.contains(&format!(
            "refusing: the listener is uid {}, not this process's uid {}",
            user.uid,
            own_uid()
        )),
        "{said}"
    );
    assert!(out.is_empty(), "stdout {out:?}");
    let them = them.wait_with_output().unwrap();
    assert!(
        them.status.success(),
        "{}",
        String::from_utf8_lossy(&them.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&them.stdout).trim(), "0");
    std::fs::remove_dir_all(&d).unwrap();
}

/// A WebSocket through attach left idle by its client for ten minutes stays up: the
/// server's pings keep arriving through it, and a ping of ours is still answered at the
/// end. Ignored by default for its length: `cargo test --test unix_socket -- --ignored
/// attach_keeps_an_idle_websocket_up --nocapture`.
#[test]
#[ignore = "takes ten minutes"]
fn attach_keeps_an_idle_websocket_up() {
    use tokio_tungstenite::tungstenite::Message;
    let x = Display::start();
    let d = scratch("attach-idle", 0o700);
    let server = Server::start(&x, &d, &["--listen-tcp=false"]);
    let (mut child, mut ws) = ws_through(&d.join("sf"));
    let start = Instant::now();
    let (mut pings, mut last) = (0u64, Instant::now());
    let mut longest = Duration::ZERO;
    while start.elapsed() < Duration::from_secs(600) {
        if next_through(&mut ws).1 {
            pings += 1;
            longest = longest.max(last.elapsed());
            last = Instant::now();
        }
    }
    println!(
        "idle {:?} through attach: {pings} server pings, longest gap {longest:?}",
        start.elapsed()
    );
    assert!(pings >= 600, "{pings} pings");
    assert!(longest < Duration::from_secs(5), "a gap of {longest:?}");
    ws.send(Message::Ping(bytes::Bytes::from_static(b"still here")))
        .unwrap();
    loop {
        if let (Message::Pong(p), _) = next_through(&mut ws) {
            assert_eq!(&p[..], b"still here");
            break;
        }
    }
    assert!(child.try_wait().unwrap().is_none(), "attach ended");
    drop(ws);
    let _ = exited(&mut child, Duration::from_secs(10));
    let _ = child.kill();
    let _ = child.wait();
    drop(server);
    std::fs::remove_dir_all(&d).unwrap();
}
