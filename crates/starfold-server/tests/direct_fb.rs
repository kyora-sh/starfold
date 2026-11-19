//! Two Xvfbs with blank screens: the capture of the second reads its own framebuffer
//! (or falls back to GetImage), never the first's, whatever the lock files say.
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

/// An Xvfb with a shared-memory framebuffer, and that segment's id.
fn xvfb(n: u32, dir: &Path) -> (Proc, i32) {
    let err = dir.join(format!("xvfb-{n}.err"));
    let child = Command::new("Xvfb")
        .args([&format!(":{n}"), "-shmem", "-screen", "0", "640x480x24"])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&err).unwrap())
        .spawn()
        .unwrap();
    let p = Proc(child);
    let mut shmid = None;
    wait_for(&format!("Xvfb :{n}"), 20, || {
        shmid = std::fs::read_to_string(&err).ok().and_then(|s| {
            s.split("shmid ")
                .nth(1)?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        });
        shmid.is_some() && Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
    });
    (p, shmid.unwrap())
}

fn paint(n: u32, rgb: u32) {
    let (conn, screen) = x11rb::connect(Some(&format!(":{n}"))).unwrap();
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

fn starfold(n: u32, port: u16, log: &Path) -> Proc {
    let child = Command::new(env!("CARGO_BIN_EXE_starfold"))
        .args([
            "--addr=127.0.0.1",
            &format!("--port={port}"),
            "--enable-basic-auth=false",
            "--audio-enabled=false",
        ])
        .env("DISPLAY", format!(":{n}"))
        .env("RUST_LOG", "info")
        .env_remove("STARFOLD_NO_DIRECT_FB")
        .stdout(std::fs::File::create(log).unwrap())
        .stderr(std::fs::File::create(log.with_extension("err")).unwrap())
        .spawn()
        .unwrap();
    let p = Proc(child);
    wait_for("starfold to listen", 60, || {
        let s = read_log(log);
        s.contains("listening on")
            && (s.contains("capturing directly") || s.contains("using GetImage"))
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

struct Layout {
    dir: PathBuf,
    a: u32,
    b: u32,
    shm_a: i32,
    shm_b: i32,
    pid_a: u32,
    _x: (Proc, Proc),
}

/// Xvfb leaves its segment behind when it exits; it goes once the last process detaches.
impl Drop for Layout {
    fn drop(&mut self) {
        for id in [self.shm_a, self.shm_b] {
            unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) };
        }
        let _ = std::fs::remove_file(format!("/tmp/.X{}-lock", self.b));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn layout(case: &str) -> Option<Layout> {
    if !on_path("Xvfb") {
        eprintln!("skipped: Xvfb is not on PATH");
        return None;
    }
    let dir =
        std::env::temp_dir().join(format!("starfold-direct-fb-{}-{case}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (a, b) = free_displays();
    let (xa, shm_a) = xvfb(a, &dir);
    let (xb, shm_b) = xvfb(b, &dir);
    let pid_a = xa.0.id();
    Some(Layout {
        dir,
        a,
        b,
        shm_a,
        shm_b,
        pid_a,
        _x: (xa, xb),
    })
}

/// Starts starfold on `l.b` with both screens blank, then paints them apart.
fn check(l: &Layout, case: &str) {
    let port = free_port();
    let log = l.dir.join(format!("starfold-{case}.log"));
    let sf = starfold(l.b, port, &log);
    let text = read_log(&log);
    let chosen: Option<i32> = text
        .split("framebuffer (shmid ")
        .nth(1)
        .and_then(|s| s.split(')').next()?.parse().ok());
    println!(
        "{case}: :{} shmid {}, :{} shmid {}, chosen {chosen:?}",
        l.a, l.shm_a, l.b, l.shm_b
    );
    assert!(
        chosen == Some(l.shm_b) || (chosen.is_none() && text.contains("using GetImage")),
        "{case}: the capture of :{} chose shmid {chosen:?}, its own is {}",
        l.b,
        l.shm_b
    );
    paint(l.a, 0x0000ff);
    paint(l.b, 0xff0000);
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
fn capture_without_a_lock_file_reads_its_own_server() {
    let _one = DISPLAYS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(l) = layout("no-lock") else { return };
    std::fs::remove_file(format!("/tmp/.X{}-lock", l.b)).unwrap();
    check(&l, "no-lock");
}

#[test]
fn capture_ignores_a_lock_file_naming_another_server() {
    let _one = DISPLAYS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(l) = layout("forged-lock") else {
        return;
    };
    let lock = format!("/tmp/.X{}-lock", l.b);
    std::fs::remove_file(&lock).unwrap();
    std::fs::write(&lock, format!("{:>10}\n", l.pid_a)).unwrap();
    check(&l, "forged-lock");
}
