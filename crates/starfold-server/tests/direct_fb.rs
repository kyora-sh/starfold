//! Xvfbs with blank screens. By default the capture uses GetImage and attaches no segment
//! of theirs. With STARFOLD_DIRECT_FB=1 it reads the served screen's own framebuffer, never
//! another server's or another screen's, whatever the lock files say.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ConnectionExt as _};

/// The tests pick free display numbers and must not race for them.
static DISPLAYS: Mutex<()> = Mutex::new(());

/// Stopped with SIGTERM, so Xvfb removes its lock file.
struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if let Ok(Some(_)) = self.0.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Xvfb leaves its segments behind when it exits; they go once the last process detaches.
struct Segments(Vec<i32>);

impl Drop for Segments {
    fn drop(&mut self) {
        for &id in &self.0 {
            unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) };
        }
    }
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

fn free_displays() -> (u32, u32) {
    let free = |n: &u32| {
        !Path::new(&format!("/tmp/.X{n}-lock")).exists()
            && !Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
    };
    let mut it = (40..200).filter(free);
    (it.next().unwrap(), it.next().unwrap())
}

fn wait_for(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !ok() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// An Xvfb with a shared-memory framebuffer per screen (`screens` of 640x480), and each
/// screen's segment id, in screen order.
fn xvfb(n: u32, screens: usize, dir: &Path) -> (Proc, Vec<i32>) {
    let err = dir.join(format!("xvfb-{n}.err"));
    let mut args = vec![format!(":{n}"), "-shmem".into()];
    for s in 0..screens {
        args.extend(["-screen".into(), s.to_string(), "640x480x24".into()]);
    }
    let child = Command::new("Xvfb")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&err).unwrap())
        .spawn()
        .unwrap();
    let p = Proc(child);
    let mut ids = Vec::new();
    wait_for(&format!("Xvfb :{n}"), 20, || {
        let text = std::fs::read_to_string(&err).unwrap_or_default();
        ids = (0..screens)
            .filter_map(|s| {
                text.split(&format!("screen {s} shmid "))
                    .nth(1)?
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            })
            .collect();
        ids.len() == screens && Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
    });
    (p, ids)
}

fn paint(display: &str, rgb: u32) {
    let (conn, screen) = x11rb::connect(Some(display)).unwrap();
    let root = conn.setup().roots[screen].root;
    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().background_pixel(rgb),
    )
    .unwrap();
    conn.clear_area(false, root, 0, 0, 0, 0).unwrap();
    conn.get_input_focus().unwrap().reply().unwrap();
}

fn read_log(log: &Path) -> String {
    let mut s = std::fs::read_to_string(log).unwrap_or_default();
    s += &std::fs::read_to_string(log.with_extension("err")).unwrap_or_default();
    s
}

/// starfold on `display`, with the direct path asked for or not.
fn starfold(display: &str, direct: bool, port: u16, log: &Path) -> Proc {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_starfold"));
    cmd.args([
        "--addr=127.0.0.1",
        &format!("--port={port}"),
        "--enable-basic-auth=false",
        "--audio-enabled=false",
    ])
    .env("DISPLAY", display)
    .env("RUST_LOG", "info")
    .env_remove("STARFOLD_NO_DIRECT_FB")
    .env_remove("VOXL_NO_DIRECT_FB")
    .stdout(std::fs::File::create(log).unwrap())
    .stderr(std::fs::File::create(log.with_extension("err")).unwrap());
    if direct {
        cmd.env("STARFOLD_DIRECT_FB", "1");
    } else {
        cmd.env_remove("STARFOLD_DIRECT_FB");
    }
    let p = Proc(cmd.spawn().unwrap());
    wait_for("starfold to listen", 60, || {
        let s = read_log(log);
        s.contains("listening on")
            && (s.contains("capturing directly") || s.contains("capturing via MIT-SHM GetImage"))
    });
    p
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The centre pixel of `/screenshot`, as 0xRRGGBB.
fn centre_pixel(port: u16) -> u32 {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s.write_all(b"GET /screenshot HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut res = Vec::new();
    s.read_to_end(&mut res).unwrap();
    let split = res.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&res[..split]);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let mut reader = png::Decoder::new(std::io::Cursor::new(&res[split + 4..]))
        .read_info()
        .unwrap();
    let mut img = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut img).unwrap();
    let (w, h) = (info.width as usize, info.height as usize);
    let i = (h / 2 * w + w / 2) * 3;
    u32::from_be_bytes([0, img[i], img[i + 1], img[i + 2]])
}

/// The segment the capture reads directly, if any.
fn chosen(log: &str) -> Option<i32> {
    log.split("framebuffer (shmid ")
        .nth(1)
        .and_then(|s| s.split(')').next()?.parse().ok())
}

fn scratch(case: &str) -> Option<PathBuf> {
    if !on_path("Xvfb") {
        eprintln!("skipped: Xvfb is not on PATH");
        return None;
    }
    let dir =
        std::env::temp_dir().join(format!("starfold-direct-fb-{}-{case}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    Some(dir)
}

/// Two Xvfbs, `a` started first; starfold serves `b`.
struct Layout {
    dir: PathBuf,
    a: u32,
    b: u32,
    shm_b: i32,
    pid_a: u32,
    _x: (Proc, Proc),
    _segments: Segments,
}

impl Drop for Layout {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(format!("/tmp/.X{}-lock", self.b));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn layout(case: &str) -> Option<Layout> {
    let dir = scratch(case)?;
    let (a, b) = free_displays();
    let (xa, shm_a) = xvfb(a, 1, &dir);
    let (xb, shm_b) = xvfb(b, 1, &dir);
    let pid_a = xa.0.id();
    Some(Layout {
        dir,
        a,
        b,
        shm_b: shm_b[0],
        pid_a,
        _x: (xa, xb),
        _segments: Segments(vec![shm_a[0], shm_b[0]]),
    })
}

/// Xvfb `b` without its lock file, as with a /tmp starfold does not share.
fn no_lock(case: &str) -> Option<Layout> {
    let l = layout(case)?;
    std::fs::remove_file(format!("/tmp/.X{}-lock", l.b)).unwrap();
    Some(l)
}

/// Xvfb `b`'s lock file naming `a`'s pid.
fn forged_lock(case: &str) -> Option<Layout> {
    let l = layout(case)?;
    let lock = format!("/tmp/.X{}-lock", l.b);
    std::fs::remove_file(&lock).unwrap();
    std::fs::write(&lock, format!("{:>10}\n", l.pid_a)).unwrap();
    Some(l)
}

/// Starts starfold on `l.b` with both screens blank, checks which path it took, then
/// paints the screens apart and reads the served one's colour back.
fn check(l: &Layout, case: &str, direct: bool) {
    let port = free_port();
    let log = l.dir.join(format!("starfold-{case}.log"));
    let sf = starfold(&format!(":{}", l.b), direct, port, &log);
    let text = read_log(&log);
    let got = chosen(&text);
    println!("{case}: :{} shmid {}, chosen {got:?}", l.b, l.shm_b);
    if direct {
        assert_eq!(
            got,
            Some(l.shm_b),
            "{case}: the capture of :{} must read its own shmid {}",
            l.b,
            l.shm_b
        );
    } else {
        assert_eq!(got, None, "{case}: no segment of Xvfb is read by default");
        assert!(
            text.contains("direct framebuffer off (set STARFOLD_DIRECT_FB=1")
                && !text.contains("direct framebuffer not used"),
            "{case}: the default never looks for a segment\n{text}"
        );
    }
    paint(&format!(":{}", l.a), 0x0000ff);
    paint(&format!(":{}", l.b), 0xff0000);
    std::thread::sleep(Duration::from_millis(500));
    let px = centre_pixel(port);
    drop(sf);
    assert_eq!(
        px, 0xff0000,
        "{case}: centre pixel {px:06x}, :{} is ff0000",
        l.b
    );
}

#[test]
fn by_default_capture_without_a_lock_file_uses_getimage() {
    let _one = DISPLAYS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(l) = no_lock("default-no-lock") else {
        return;
    };
    check(&l, "default-no-lock", false);
}

#[test]
fn by_default_capture_with_a_forged_lock_file_uses_getimage() {
    let _one = DISPLAYS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(l) = forged_lock("default-forged-lock") else {
        return;
    };
    check(&l, "default-forged-lock", false);
}

#[test]
fn asked_for_capture_without_a_lock_file_reads_its_own_server() {
    let _one = DISPLAYS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(l) = no_lock("direct-no-lock") else {
        return;
    };
    check(&l, "direct-no-lock", true);
}

#[test]
fn asked_for_capture_ignores_a_lock_file_naming_another_server() {
    let _one = DISPLAYS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(l) = forged_lock("direct-forged-lock") else {
        return;
    };
    check(&l, "direct-forged-lock", true);
}

#[test]
fn asked_for_capture_of_screen_1_reads_screen_1() {
    let _one = DISPLAYS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(dir) = scratch("two-screens") else {
        return;
    };
    let (n, _) = free_displays();
    let (_x, ids) = xvfb(n, 2, &dir);
    let _segments = Segments(ids.clone());
    let port = free_port();
    let log = dir.join("starfold-two-screens.log");
    let sf = starfold(&format!(":{n}.1"), true, port, &log);
    let got = chosen(&read_log(&log));
    println!(
        "two-screens: :{n}.0 shmid {}, :{n}.1 shmid {}, chosen {got:?}",
        ids[0], ids[1]
    );
    assert_eq!(
        got,
        Some(ids[1]),
        "the capture of :{n}.1 must read screen 1's segment"
    );
    paint(&format!(":{n}.0"), 0x0000ff);
    paint(&format!(":{n}.1"), 0xff0000);
    std::thread::sleep(Duration::from_millis(500));
    let px = centre_pixel(port);
    drop(sf);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(px, 0xff0000, "centre pixel {px:06x}, :{n}.1 is ff0000");
}
