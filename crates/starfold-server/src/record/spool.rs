//! Where a recording's video goes while it runs: a file on disk that has no name. Linux
//! opens it with O_TMPFILE; elsewhere (and on a Linux filesystem without it) it is created
//! in a 0700 directory of the server's own inside `--recording-dir` and unlinked at once
//! (Windows: deleted when closed). A file that lands on a memory filesystem is refused: a
//! recording there is RAM by another name.
//!
//! Each spool reserves the disk space it may fill from a [`Budget`] shared by the server's
//! recordings, at its start: the free space above the floor that no other recording has
//! reserved and not yet written. So the server's own recordings never take the disk below
//! the floor together; another process writing to the same filesystem still can, which the
//! writer's checks while it writes catch.
//!
//! A [`Spool`] is closed on the blocking pool wherever it is dropped
//! ([`super::off_workers`]): closing a file is a file operation, and it never runs on an
//! async worker.

use parking_lot::Mutex;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The free space a recording leaves on its filesystem: the smaller of 1 GiB and a tenth
/// of the filesystem.
pub fn floor(total: u64) -> u64 {
    (1u64 << 30).min(total / 10)
}

/// The least a recording reserves (or its whole size limit, when that is smaller): with
/// less room above the floor a start is refused.
pub const MIN_ROOM: u64 = 64 << 20;

/// Where recordings spool by default. Linux: /var/tmp, the temporary directory that is
/// kept on disk (/tmp is often memory). Elsewhere: the system temporary directory, which
/// is on disk on macOS (a per-user folder) and Windows (%TEMP%).
pub fn default_dir() -> PathBuf {
    if cfg!(target_os = "linux") {
        PathBuf::from("/var/tmp")
    } else {
        std::env::temp_dir()
    }
}

/// The directory of the server's own inside `dir` that holds named spools while they are
/// created.
pub const OWN: &str = ".starfold-recordings";

/// Why a spool couldn't be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The directory can't take a file.
    NotWritable,
    /// The file landed on memory (tmpfs, ramfs).
    InMemory,
    /// Less room above the floor than a recording reserves.
    LowSpace,
}

/// Free and total bytes of the filesystem holding the open `file` (`dir` on Windows).
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // 32-bit fields on some platforms
pub fn space(file: &File, _dir: &Path) -> io::Result<(u64, u64)> {
    let st = nix::sys::statvfs::fstatvfs(file).map_err(io::Error::from)?;
    let unit = st.fragment_size() as u64;
    Ok((
        st.blocks_available() as u64 * unit,
        st.blocks() as u64 * unit,
    ))
}

#[cfg(windows)]
pub fn space(_file: &File, dir: &Path) -> io::Result<(u64, u64)> {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let (mut free, mut total) = (0u64, 0u64);
    let path = windows::core::HSTRING::from(dir.as_os_str());
    unsafe { GetDiskFreeSpaceExW(&path, Some(&mut free), Some(&mut total), None) }
        .map_err(|e| io::Error::other(e.message()))?;
    Ok((free, total))
}

/// Whether the open `file` is on a filesystem that lives in memory: the file itself, so a
/// memory filesystem mounted below `--recording-dir` (at the server's own directory) is
/// seen too.
#[cfg(target_os = "linux")]
fn in_memory(file: &File) -> io::Result<bool> {
    const TMPFS: u32 = 0x0102_1994;
    const RAMFS: u32 = 0x8584_58f6;
    let st = nix::sys::statfs::fstatfs(file).map_err(io::Error::from)?;
    // f_type is 64 bits wide (signed on glibc, unsigned on musl); the magic numbers are 32.
    Ok(matches!(st.filesystem_type().0 as u32, TMPFS | RAMFS))
}

#[cfg(target_os = "macos")]
fn in_memory(file: &File) -> io::Result<bool> {
    let st = nix::sys::statfs::fstatfs(file).map_err(io::Error::from)?;
    Ok(matches!(st.filesystem_type_name(), "tmpfs" | "ramfs"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn in_memory(_file: &File) -> io::Result<bool> {
    Ok(false)
}

/// The server's own directory in `dir`, created 0700 at the first recording. One that is
/// there already must be a directory (not a link) that only this user can enter.
fn own_dir(dir: &Path) -> io::Result<PathBuf> {
    let own = dir.join(OWN);
    #[cfg(unix)]
    let b = {
        use std::os::unix::fs::DirBuilderExt;
        let mut b = std::fs::DirBuilder::new();
        b.mode(0o700);
        b
    };
    #[cfg(not(unix))]
    let b = std::fs::DirBuilder::new();
    match b.create(&own) {
        Ok(()) => return Ok(own),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let m = std::fs::symlink_metadata(&own)?;
    if !m.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            own.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        if m.uid() != me || m.mode() & 0o077 != 0 {
            return Err(io::Error::other(format!(
                "{} is not this user's alone",
                own.display()
            )));
        }
    }
    Ok(own)
}

/// A file in `dir` that has no name, so nothing is left behind whatever happens to the
/// server (on Windows: once it is closed).
pub fn anonymous(dir: &Path) -> io::Result<File> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(f) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_TMPFILE)
            .open(dir)
        {
            return Ok(f);
        }
    }
    let own = own_dir(dir)?;
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).map_err(io::Error::other)?;
    let name: String = id.iter().map(|b| format!("{b:02x}")).collect();
    let path = own.join(name);
    let mut o = std::fs::OpenOptions::new();
    o.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
        o.share_mode(0).custom_flags(FILE_FLAG_DELETE_ON_CLOSE);
    }
    let f = o.open(&path)?;
    #[cfg(not(windows))]
    std::fs::remove_file(&path)?;
    Ok(f)
}

/// The disk space the server's recordings have reserved and not yet written.
#[derive(Default)]
pub struct Budget {
    outstanding: Mutex<u64>,
}

impl Budget {
    #[cfg(test)]
    pub fn outstanding(&self) -> u64 {
        *self.outstanding.lock()
    }
}

/// What one spool reserved and has not written yet; given back when it is dropped.
struct Reservation {
    budget: Arc<Budget>,
    left: u64,
}

impl Reservation {
    fn wrote(&mut self, n: u64) {
        let n = n.min(self.left);
        self.left -= n;
        *self.budget.outstanding.lock() -= n;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        *self.budget.outstanding.lock() -= self.left;
    }
}

/// How a spool measures its filesystem: [`space`] in the server; tests give their own.
pub type Space = dyn Fn(&File, &Path) -> io::Result<(u64, u64)> + Send + Sync;

/// What a spool may hold and how it measures its disk.
pub struct Rules<'a> {
    pub floor: &'a (dyn Fn(u64) -> u64 + Send + Sync),
    pub space: &'a Space,
    /// The most video a recording holds.
    pub max_bytes: u64,
}

/// One recording's file and the disk space it reserved. Writing to it counts against the
/// reservation; dropping it closes the file off the async workers and gives back what was
/// not written (what was stays reserved until the file is closed: it is on the disk).
pub struct Spool {
    file: Option<File>,
    reserved: Option<Reservation>,
    /// The bytes it may hold: what it reserved.
    limit: u64,
}

impl Spool {
    pub fn file(&self) -> &File {
        self.file
            .as_ref()
            .expect("a spool's file is there until it is dropped")
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// A spool of `file` with nothing reserved (tests).
    #[cfg(test)]
    pub fn bare(file: File) -> Spool {
        Spool {
            file: Some(file),
            reserved: None,
            limit: u64::MAX,
        }
    }
}

impl Write for Spool {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let n = self.file.as_ref().expect("open").write(b)?;
        if let Some(r) = &mut self.reserved {
            r.wrote(n as u64);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_ref().expect("open").flush()
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        // The file closes first, then its unwritten room goes back.
        super::off_workers((self.file.take(), self.reserved.take()));
    }
}

/// A spool for one recording in `dir`, with its room reserved, or why not. Blocks: call it
/// off the async workers.
pub fn open(dir: &Path, rules: &Rules, budget: &Arc<Budget>) -> Result<Spool, (Refused, String)> {
    let file = anonymous(dir).map_err(|e| (Refused::NotWritable, e.to_string()))?;
    match in_memory(&file) {
        Ok(true) => return Err((Refused::InMemory, dir.display().to_string())),
        Ok(false) => {}
        Err(e) => return Err((Refused::NotWritable, e.to_string())),
    }
    let mut outstanding = budget.outstanding.lock();
    let (free, total) =
        (rules.space)(&file, dir).map_err(|e| (Refused::NotWritable, e.to_string()))?;
    let floor = (rules.floor)(total);
    let room = free.saturating_sub(floor).saturating_sub(*outstanding);
    let limit = room.min(rules.max_bytes);
    if limit < rules.max_bytes.min(MIN_ROOM) {
        return Err((
            Refused::LowSpace,
            format!(
                "{free} bytes free, {floor} left free, {} reserved by recordings running",
                *outstanding
            ),
        ));
    }
    *outstanding += limit;
    drop(outstanding);
    Ok(Spool {
        file: Some(file),
        reserved: Some(Reservation {
            budget: budget.clone(),
            left: limit,
        }),
        limit,
    })
}

/// Reads up to `buf.len()` bytes of `file` at `offset`.
pub fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::FileExt::seek_read(file, buf, offset)
    }
}
