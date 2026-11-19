//! X11 screen capture: MIT-SHM GetImage into a persistent BGRX frame, driven by XDamage.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt as _, EventMask, ImageFormat,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

pub static DAMAGE_EVENTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub struct ShmBuf {
    pub ptr: *mut u8,
    pub len: usize,
    shmid: i32,
    /// Attached to someone else's segment (never removed by us).
    foreign: bool,
}

unsafe impl Send for ShmBuf {}

impl ShmBuf {
    fn new(len: usize) -> Result<Self, String> {
        unsafe {
            let shmid = libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600);
            if shmid < 0 {
                return Err("shmget failed".into());
            }
            let ptr = libc::shmat(shmid, std::ptr::null(), 0);
            if ptr as isize == -1 {
                return Err("shmat failed".into());
            }
            Ok(ShmBuf {
                ptr: ptr as *mut u8,
                len,
                shmid,
                foreign: false,
            })
        }
    }

    fn attach_readonly(shmid: i32, len: usize) -> Option<Self> {
        unsafe {
            let ptr = libc::shmat(shmid, std::ptr::null(), libc::SHM_RDONLY);
            if ptr as isize == -1 {
                return None;
            }
            Some(ShmBuf {
                ptr: ptr as *mut u8,
                len,
                shmid,
                foreign: true,
            })
        }
    }
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for ShmBuf {
    fn drop(&mut self) {
        unsafe {
            libc::shmdt(self.ptr as *const _);
            if !self.foreign {
                libc::shmctl(self.shmid, libc::IPC_RMID, std::ptr::null_mut());
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Band {
    pub y0: u32,
    pub y1: u32,
}

pub struct Capturer {
    pub conn: RustConnection,
    root: u32,
    pub width: u32,
    pub height: u32,
    resize_pending: bool,
    pub stride: usize,
    seg: u32,
    buf: ShmBuf,
    /// Byte offset of pixel data inside `buf` (non-zero for an Xvfb XWD framebuffer).
    offset: usize,
    /// Reading Xvfb's framebuffer in place: no GetImage needed.
    pub direct: bool,
    /// Kept for the connection's lifetime; raw-rectangle events need no further requests.
    _damage: u32,
    pub damaged: bool,
    /// When the first damage since the last take arrived.
    pub damaged_at: Option<std::time::Instant>,
    /// Rows touched since the last take (raw damage rectangles, no server round trip).
    acc: Option<(u32, u32)>,
    /// Macroblocks (16x16) touched since the last take, row-major.
    mb_dirty: Vec<u8>,
    pub mb_cols: usize,
    /// Areas (x0, y0, x1, y1) changed since the video thread last took them, for `/wait`.
    pub changes: Vec<(u32, u32, u32, u32)>,
}

impl Capturer {
    pub fn new(display: Option<&str>) -> Result<Self, String> {
        let (conn, screen_num) =
            RustConnection::connect(display).map_err(|e| format!("X connect: {e}"))?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        // Subscribe before querying geometry so a resize during initialization is not lost.
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| e.to_string())?;
        let geometry = conn
            .get_geometry(root)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        let width = u32::from(geometry.width);
        let height = u32::from(geometry.height);
        if screen.root_depth != 24 && screen.root_depth != 32 {
            return Err(format!("unsupported root depth {}", screen.root_depth));
        }
        conn.extension_information(shm::X11_EXTENSION_NAME)
            .map_err(|e| e.to_string())?
            .ok_or("no MIT-SHM")?;
        conn.shm_query_version()
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        conn.damage_query_version(1, 1)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        conn.xfixes_query_version(5, 0)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        let mut stride = width as usize * 4;
        let seg = conn.generate_id().map_err(|e| e.to_string())?;
        let (buf, offset, direct) = match find_xvfb_framebuffer(&conn, root, width, height) {
            Ok((buf, offset, fb_stride)) => {
                log::info!(
                    "capturing directly from the Xvfb framebuffer (shmid {})",
                    buf.shmid
                );
                stride = fb_stride;
                (buf, offset, true)
            }
            Err(why) => {
                log::warn!("direct framebuffer not used: {why}; using GetImage");
                let buf = ShmBuf::new(stride * height as usize)?;
                conn.shm_attach(seg, buf.shmid as u32, false)
                    .map_err(|e| e.to_string())?
                    .check()
                    .map_err(|e| {
                        format!(
                            "the X server refused MIT-SHM Attach ({e}): MIT-SHM needs a local X \
                             connection, so connect through the local socket (DISPLAY=:N)"
                        )
                    })?;
                // Mark for removal now; it stays alive while attached.
                unsafe { libc::shmctl(buf.shmid, libc::IPC_RMID, std::ptr::null_mut()) };
                log::info!("capturing via MIT-SHM GetImage");
                (buf, 0, false)
            }
        };
        let damage = conn.generate_id().map_err(|e| e.to_string())?;
        conn.damage_create(damage, root, damage::ReportLevel::RAW_RECTANGLES)
            .map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;
        Ok(Capturer {
            conn,
            root,
            width,
            height,
            resize_pending: false,
            stride,
            seg,
            buf,
            offset,
            direct,
            _damage: damage,
            damaged: true,
            damaged_at: None,
            acc: None,
            mb_dirty: vec![0; (width as usize).div_ceil(16) * (height as usize).div_ceil(16)],
            mb_cols: (width as usize).div_ceil(16),
            changes: Vec::new(),
        })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.conn.stream().as_fd()
    }

    /// Drain queued X events; returns true if damage was reported.
    pub fn pump(&mut self) -> Result<(), String> {
        while let Some(ev) = self.conn.poll_for_event().map_err(|e| e.to_string())? {
            if let Event::ConfigureNotify(e) = &ev {
                if e.window == self.root
                    && (u32::from(e.width), u32::from(e.height)) != (self.width, self.height)
                {
                    self.resize_pending = true;
                }
            }
            if let Event::DamageNotify(e) = ev {
                DAMAGE_EVENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if crate::compat::is_set("STARFOLD_DEBUG_DAMAGE") {
                    log::info!(
                        "dmg {:?} x{} y{} {}x{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_micros()
                            % 1_000_000,
                        e.area.x,
                        e.area.y,
                        e.area.width,
                        e.area.height
                    );
                }
                let top = (e.area.y as i32).clamp(0, self.height as i32) as u32;
                let bot =
                    (e.area.y as i32 + e.area.height as i32).clamp(0, self.height as i32) as u32;
                if bot > top {
                    let left = (e.area.x as i32).clamp(0, self.width as i32) as u32;
                    let right =
                        (e.area.x as i32 + e.area.width as i32).clamp(0, self.width as i32) as u32;
                    if right <= left {
                        continue;
                    }
                    let (x0, x1) = (left as usize / 16, (right as usize).div_ceil(16));
                    for row in (top as usize / 16)..(bot as usize).div_ceil(16) {
                        self.mb_dirty[row * self.mb_cols + x0..row * self.mb_cols + x1].fill(1);
                    }
                    self.changes.push((left, top, right, bot));
                    self.acc = Some(match self.acc {
                        Some((a, b)) => (a.min(top), b.max(bot)),
                        None => (top, bot),
                    });
                    self.damaged = true;
                    self.damaged_at.get_or_insert_with(std::time::Instant::now);
                }
            }
        }
        Ok(())
    }

    /// Take accumulated damage as a row band. Raw-rectangle reporting delivers the
    /// areas in the events themselves, so this never waits on the X server.
    pub fn take_damage(&mut self) -> Result<Option<Band>, String> {
        self.damaged = false;
        self.damaged_at = None;
        let Some((y0, y1)) = self.acc.take() else {
            return Ok(None);
        };
        // Align to even rows for 4:2:0.
        Ok(Some(Band {
            y0: y0 & !1,
            y1: ((y1 + 1) & !1).min(self.height),
        }))
    }

    /// Take the dirty-macroblock map accumulated with the current damage.
    pub fn take_mb_map(&mut self) -> Vec<u8> {
        let n = self.mb_dirty.len();
        std::mem::replace(&mut self.mb_dirty, vec![0; n])
    }

    pub fn full(&self) -> Band {
        Band {
            y0: 0,
            y1: self.height,
        }
    }

    /// Convert rows [y0, y1) of the current frame into `dst`.
    pub fn convert(&self, dst: &mut crate::convert::I420, y0: usize, y1: usize, threads: usize) {
        crate::convert::convert_band(self.pixels(), self.stride, dst, y0, y1, threads);
    }

    /// The current frame as packed BGRX (`width * 4` bytes per row), for screenshots,
    /// with the damage for exactly these pixels taken into `changes`. Nothing draws while
    /// the server is grabbed (a few ms for the copy), so a repaint can't land in the
    /// pixels without its damage, or after them with it.
    pub fn snapshot(&mut self) -> Result<Vec<u8>, String> {
        self.conn.grab_server().map_err(|e| e.to_string())?;
        let shot = self.snapshot_grabbed();
        // Release the server even if the copy failed.
        self.conn.ungrab_server().map_err(|e| e.to_string())?;
        self.conn.flush().map_err(|e| e.to_string())?;
        shot
    }

    fn snapshot_grabbed(&mut self) -> Result<Vec<u8>, String> {
        // Damage for everything drawn before the grab arrives ahead of this reply.
        self.conn
            .get_input_focus()
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        self.pump()?;
        self.grab(self.full())?;
        let (row, h) = (self.width as usize * 4, self.height as usize);
        let mut out = Vec::with_capacity(row * h);
        for line in self.pixels().chunks_exact(self.stride).take(h) {
            out.extend_from_slice(&line[..row]);
        }
        Ok(out)
    }

    /// The current frame as BGRX pixels and their stride (debug marker probe).
    pub fn bgrx(&self) -> Option<(&[u8], usize)> {
        Some((self.pixels(), self.stride))
    }

    /// Whether a root configure event changed the capture dimensions.
    pub fn needs_rebuild(&self) -> bool {
        self.resize_pending
    }

    /// A resize can race a GetImage request before its configure event is processed.
    pub fn geometry_changed(&self) -> bool {
        self.conn
            .get_geometry(self.root)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|g| (u32::from(g.width), u32::from(g.height)) != (self.width, self.height))
    }

    /// Current frame pixels (BGRX, `stride` bytes per row).
    fn pixels(&self) -> &[u8] {
        &self.buf.as_slice()[self.offset..self.offset + self.stride * self.height as usize]
    }

    /// Grab a full-width band into the shm frame at its natural offset.
    pub fn grab(&self, band: Band) -> Result<(), String> {
        let h = band.y1 - band.y0;
        if h == 0 || self.direct {
            return Ok(());
        }
        let off = band.y0 as usize * self.stride;
        self.conn
            .shm_get_image(
                self.root,
                0,
                band.y0 as i16,
                self.width as u16,
                h as u16,
                !0,
                ImageFormat::Z_PIXMAP.into(),
                self.seg,
                off as u32,
            )
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Pid of the X server at the other end of the connection, from the socket's peer
/// credentials: the kernel records them at the server's `listen()`, so no lock file or
/// display name can point at another process.
fn x_server_pid(conn: &RustConnection) -> Result<u32, String> {
    let fd = conn.stream().as_fd().as_raw_fd();
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of_val(&addr) as libc::socklen_t;
    if unsafe { libc::getsockname(fd, (&raw mut addr).cast(), &mut len) } != 0 {
        return Err(format!("getsockname: {}", std::io::Error::last_os_error()));
    }
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
    let peer = (i32::from(addr.ss_family) == libc::AF_UNIX
        && unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut cred).cast(),
                &mut len,
            )
        } == 0)
        .then_some(cred.pid);
    server_pid_from(i32::from(addr.ss_family), peer)
}

fn server_pid_from(family: i32, peer_pid: Option<i32>) -> Result<u32, String> {
    if family != libc::AF_UNIX {
        return Err("the connection is not a local socket".into());
    }
    match peer_pid {
        None => Err("no peer credentials on the socket".into()),
        Some(pid) if pid <= 0 => Err("the peer is outside this pid namespace".into()),
        Some(pid) => Ok(pid as u32),
    }
}

/// `/proc/self` names this process only when /proc belongs to our pid namespace.
fn proc_is_own(self_link: &str, own_pid: u32) -> Result<(), String> {
    if self_link.parse() == Ok(own_pid) {
        Ok(())
    } else {
        Err(format!(
            "/proc is another pid namespace's (/proc/self is {self_link:?}, this process is {own_pid})"
        ))
    }
}

/// Segments in `/proc/sysvipc/shm` created by `server_pid` and at least `min_size` bytes,
/// as (shmid, size). Without the server's pid there are none.
fn candidates(table: &str, server_pid: Option<u32>, min_size: usize) -> Vec<(i32, usize)> {
    let Some(pid) = server_pid else {
        return Vec::new();
    };
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let shmid = f.get(1)?.parse().ok()?;
            let size = f.get(3)?.parse().ok()?;
            let cpid: u32 = f.get(4)?.parse().ok()?;
            (cpid == pid && size >= min_size).then_some((shmid, size))
        })
        .collect()
}

#[derive(Debug, PartialEq)]
struct Mapping<'a> {
    start: usize,
    dev: &'a str,
    inode: u64,
    path: &'a str,
}

fn next_field<'a>(s: &mut &'a str) -> Option<&'a str> {
    let t = s.trim_start();
    let end = t.find(char::is_whitespace).unwrap_or(t.len());
    *s = &t[end..];
    (end > 0).then(|| &t[..end])
}

/// One line of `/proc/<pid>/maps`.
fn parse_mapping(line: &str) -> Option<Mapping<'_>> {
    let mut rest = line;
    let range = next_field(&mut rest)?;
    next_field(&mut rest)?;
    next_field(&mut rest)?;
    let dev = next_field(&mut rest)?;
    let inode = next_field(&mut rest)?.parse().ok()?;
    let start = usize::from_str_radix(range.split('-').next()?, 16).ok()?;
    Some(Mapping {
        start,
        dev,
        inode,
        path: rest.trim(),
    })
}

/// What the kernel says about a segment we attached at `addr`.
struct Segment<'a> {
    cpid: i32,
    addr: usize,
    own_maps: &'a str,
    server_maps: Option<&'a str>,
}

/// A segment is the X server's only if it created it and maps it now: the same
/// dev:inode as our own mapping, so a segment left by an earlier process with the same
/// pid, or one of another IPC namespace, is refused.
fn check_segment(server_pid: u32, s: &Segment) -> Result<(), String> {
    if u32::try_from(s.cpid).ok() != Some(server_pid) {
        return Err(format!(
            "created by pid {}, not the X server ({server_pid})",
            s.cpid
        ));
    }
    let ours = s
        .own_maps
        .lines()
        .filter_map(parse_mapping)
        .find(|m| m.start == s.addr)
        .ok_or("our mapping of it is not in /proc/self/maps")?;
    let maps = s
        .server_maps
        .ok_or("the X server's maps are not readable")?;
    if maps
        .lines()
        .filter_map(parse_mapping)
        .any(|m| m.dev == ours.dev && m.inode == ours.inode)
    {
        Ok(())
    } else {
        Err("the X server does not map it".into())
    }
}

fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]])
}

/// Xvfb started with `-shmem` keeps each screen as an XWD image in a SysV segment.
/// Find the one of the X server we are connected to, check the header and verify pixels
/// against the server before trusting it. After a RandR resize the header still
/// describes the size Xvfb started with: the screen is the top-left `width`x`height` of
/// it, at the header's stride.
fn find_xvfb_framebuffer(
    conn: &RustConnection,
    root: u32,
    width: u32,
    height: u32,
) -> Result<(ShmBuf, usize, usize), String> {
    if crate::compat::is_set("STARFOLD_NO_DIRECT_FB") {
        return Err("STARFOLD_NO_DIRECT_FB is set".into());
    }
    let pid = x_server_pid(conn)
        .map_err(|why| format!("the X server's pid is not known from the connection ({why})"))?;
    let self_link = std::fs::read_link("/proc/self")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    proc_is_own(&self_link, std::process::id())?;
    let table = std::fs::read_to_string("/proc/sysvipc/shm")
        .map_err(|e| format!("/proc/sysvipc/shm: {e}"))?;
    let server_maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).ok();
    let mut refused = None;
    for (shmid, size) in candidates(&table, Some(pid), (width * height * 4) as usize) {
        let Some(buf) = ShmBuf::attach_readonly(shmid, size) else {
            continue;
        };
        // Checked after the attach, so the id cannot name another segment by now.
        let mut ds: libc::shmid_ds = unsafe { std::mem::zeroed() };
        if unsafe { libc::shmctl(shmid, libc::IPC_STAT, &mut ds) } != 0 || ds.shm_segsz != size {
            continue;
        }
        let own_maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
        let seg = Segment {
            cpid: ds.shm_cpid,
            addr: buf.ptr as usize,
            own_maps: &own_maps,
            server_maps: server_maps.as_deref(),
        };
        if let Err(why) = check_segment(pid, &seg) {
            refused = Some(format!("; shmid {shmid} refused: {why}"));
            continue;
        }
        let b = buf.as_slice();
        let header_size = be32(b, 0) as usize;
        let (w, h, bpp, bpl, ncolors) = (
            be32(b, 4),
            be32(b, 5),
            be32(b, 11),
            be32(b, 12) as usize,
            be32(b, 19) as usize,
        );
        if be32(b, 1) != 7 || w < width || h < height || bpp != 32 || bpl < (w * 4) as usize {
            continue;
        }
        let offset = header_size + ncolors * 12;
        if offset + bpl * h as usize > size {
            continue;
        }
        // Compare a strip of real pixels with what the server reports. The screen may be
        // animating between the two reads, so retry and accept a near-total match.
        let (sy, sh) = (height / 2, 16u32.min(height / 2));
        let mut ok = false;
        for _ in 0..10 {
            let Ok(Ok(img)) = conn
                .get_image(
                    ImageFormat::Z_PIXMAP,
                    root,
                    0,
                    sy as i16,
                    width as u16,
                    sh as u16,
                    !0,
                )
                .map(|c| c.reply())
            else {
                break;
            };
            let mut same = 0usize;
            let total = (width * sh) as usize;
            for r in 0..sh as usize {
                let fb = &b[offset + (sy as usize + r) * bpl..][..width as usize * 4];
                let im = &img.data[r * width as usize * 4..][..width as usize * 4];
                same += fb
                    .chunks_exact(4)
                    .zip(im.chunks_exact(4))
                    .filter(|(a, c)| a[..3] == c[..3])
                    .count();
            }
            if same * 100 >= total * 98 {
                ok = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        if ok {
            return Ok((buf, offset, bpl));
        }
    }
    Err(format!(
        "no segment of the X server (pid {pid}) holds its screen (start Xvfb with -shmem){}",
        refused.unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "       key      shmid perms                  size  cpid  lpid nattch   uid   gid  cuid  cgid      atime      dtime      ctime                   rss                  swap
         0          4   777               1232032  3190  3193      1  1000  1000  1000  1000 1795063211 1795063211 1795063211               1232896                     0
         0          5   777               1232032  3195  3198      1  1000  1000  1000  1000 1795063212 1795063212 1795063212               1232896                     0
";
    const SEG: &str = "7f3b2c600000-7f3b2c72d000 r--s 00000000 00:01 5                          /SYSV00000000 (deleted)";
    const OWN_MAPS: &str =
        "55d0c0a00000-55d0c0a21000 r--p 00000000 fd:01 1048602                    /usr/bin/starfold
7f3b2c600000-7f3b2c72d000 r--s 00000000 00:01 5                          /SYSV00000000 (deleted)
7ffd5a3e1000-7ffd5a402000 rw-p 00000000 00:00 0                          [stack]
";
    const SERVER_MAPS: &str =
        "5612aa000000-5612aa2b0000 r-xp 00000000 fd:01 1050113                    /usr/bin/Xvfb
7f9e10200000-7f9e1032d000 rw-s 00000000 00:01 5                          /SYSV00000000 (deleted)
";

    fn seg<'a>(cpid: i32, server_maps: Option<&'a str>) -> Segment<'a> {
        Segment {
            cpid,
            addr: 0x7f3b2c600000,
            own_maps: OWN_MAPS,
            server_maps,
        }
    }

    #[test]
    fn a_sysv_line_of_maps_parses() {
        assert_eq!(
            parse_mapping(SEG),
            Some(Mapping {
                start: 0x7f3b2c600000,
                dev: "00:01",
                inode: 5,
                path: "/SYSV00000000 (deleted)",
            })
        );
        assert_eq!(
            parse_mapping("7ffd5a3e1000-7ffd5a402000 rw-p 00000000 00:00 0 ").map(|m| m.path),
            Some("")
        );
        assert_eq!(parse_mapping(""), None);
    }

    #[test]
    fn without_the_server_pid_no_segment_is_a_candidate() {
        assert_eq!(candidates(TABLE, None, 640 * 480 * 4), vec![]);
        assert_eq!(
            candidates(TABLE, Some(3195), 640 * 480 * 4),
            vec![(5, 1232032)]
        );
        assert_eq!(candidates(TABLE, Some(3195), 1232033), vec![]);
    }

    #[test]
    fn only_a_local_socket_with_a_peer_pid_names_the_server() {
        assert_eq!(server_pid_from(libc::AF_UNIX, Some(3195)), Ok(3195));
        assert!(server_pid_from(libc::AF_INET, Some(3195)).is_err());
        assert!(server_pid_from(libc::AF_INET6, None).is_err());
        assert!(server_pid_from(libc::AF_UNIX, Some(0)).is_err());
        assert!(server_pid_from(libc::AF_UNIX, None).is_err());
    }

    #[test]
    fn a_segment_the_server_created_and_maps_is_accepted() {
        assert_eq!(check_segment(3195, &seg(3195, Some(SERVER_MAPS))), Ok(()));
    }

    #[test]
    fn a_segment_another_process_created_is_refused() {
        assert!(check_segment(3195, &seg(3190, Some(SERVER_MAPS))).is_err());
        assert!(check_segment(3195, &seg(0, Some(SERVER_MAPS))).is_err());
    }

    #[test]
    fn a_stale_segment_of_an_earlier_process_with_the_same_pid_is_refused() {
        let other = SERVER_MAPS.replace("00:01 5 ", "00:01 7 ");
        assert!(check_segment(3195, &seg(3195, Some(&other))).is_err());
        let none = "5612aa000000-5612aa2b0000 r-xp 00000000 fd:01 1050113 /usr/bin/Xvfb\n";
        assert!(check_segment(3195, &seg(3195, Some(none))).is_err());
    }

    #[test]
    fn the_same_inode_on_another_device_is_refused() {
        let other = SERVER_MAPS.replace("00:01 5 ", "00:02 5 ");
        assert!(check_segment(3195, &seg(3195, Some(&other))).is_err());
    }

    #[test]
    fn unreadable_server_maps_prove_nothing() {
        assert!(check_segment(3195, &seg(3195, None)).is_err());
    }

    #[test]
    fn our_own_mapping_must_be_found() {
        let s = Segment {
            addr: 0x7f3b2c700000,
            ..seg(3195, Some(SERVER_MAPS))
        };
        assert!(check_segment(3195, &s).is_err());
    }

    #[test]
    fn proc_of_another_pid_namespace_is_refused() {
        assert_eq!(proc_is_own("4242", 4242), Ok(()));
        assert!(proc_is_own("1", 4242).is_err());
        assert!(proc_is_own("", 4242).is_err());
    }
}
