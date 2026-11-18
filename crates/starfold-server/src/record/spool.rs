//! Where a recording's video goes while it runs: a file on disk that has no name, written
//! by a thread of its own behind a bounded queue, so the stream never waits on the disk.

use bytes::Bytes;
use parking_lot::Mutex;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

/// What the writer gathers before each write.
pub const BUFFER: usize = 256 << 10;

/// The free space a recording leaves on its filesystem: the smaller of 1 GiB and a tenth
/// of the filesystem.
pub fn floor(total: u64) -> u64 {
    (1u64 << 30).min(total / 10)
}

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

/// Why a spool couldn't be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The directory can't take a file.
    NotWritable,
    /// The directory is memory (tmpfs, ramfs): a recording there is RAM by another name.
    InMemory,
    /// Less free space than [`floor`].
    LowSpace,
}

/// Limits one spool writes within.
#[derive(Clone)]
pub struct Limits {
    /// The most video bytes one recording holds.
    pub max_bytes: u64,
    /// What the writer gathers before each write ([`BUFFER`]).
    pub buffer: usize,
    /// The most bytes queued for the writer; past it the recording ends (`disk_slow`).
    pub queue_bytes: usize,
    /// How often (bytes written) the free space is checked again.
    pub check_every: u64,
    /// The free space to leave, from the filesystem's size ([`floor`]).
    pub floor: Arc<dyn Fn(u64) -> u64 + Send + Sync>,
    /// Runs as the writer takes each sample, with the bytes written so far; an error ends
    /// the recording as a write error would. None in the server; tests block or fail here.
    pub before_write: Option<Arc<dyn Fn(u64) -> io::Result<()> + Send + Sync>>,
}

/// Why the writer stopped before it was told to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The recording reached its byte limit.
    Full,
    /// The writer fell behind by more than the queue holds.
    Slow,
    /// The filesystem went below the free space to leave.
    Space,
    /// A write failed (full disk, I/O error, the directory gone).
    Error(String),
}

/// Free and total bytes of the filesystem holding `dir`.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // 32-bit fields on some platforms
fn space(dir: &Path) -> io::Result<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let unit = st.f_frsize as u64;
    Ok((st.f_bavail as u64 * unit, st.f_blocks as u64 * unit))
}

#[cfg(windows)]
fn space(dir: &Path) -> io::Result<(u64, u64)> {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let (mut free, mut total) = (0u64, 0u64);
    let path = windows::core::HSTRING::from(dir.as_os_str());
    unsafe { GetDiskFreeSpaceExW(&path, Some(&mut free), Some(&mut total), None) }
        .map_err(|e| io::Error::other(e.message()))?;
    Ok((free, total))
}

/// Whether `dir` is on a filesystem that lives in memory.
#[cfg(target_os = "linux")]
fn in_memory(dir: &Path) -> io::Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    const TMPFS: u32 = 0x0102_1994;
    const RAMFS: u32 = 0x8584_58f6;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(matches!(st.f_type as u64 as u32, TMPFS | RAMFS))
}

#[cfg(target_os = "macos")]
fn in_memory(dir: &Path) -> io::Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let name = unsafe { std::ffi::CStr::from_ptr(st.f_fstypename.as_ptr()) };
    Ok(matches!(name.to_bytes(), b"tmpfs" | b"ramfs"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn in_memory(_dir: &Path) -> io::Result<bool> {
    Ok(false)
}

/// A file in `dir` that has no name, so nothing is left behind whatever happens to the
/// server: Linux's O_TMPFILE where the filesystem has it, else a new file removed at once
/// (Unix) or deleted when closed (Windows).
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
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).map_err(io::Error::other)?;
    let name: String = id.iter().map(|b| format!("{b:02x}")).collect();
    let path = dir.join(format!(".starfold-recording-{name}"));
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

/// A spool for one recording in `dir`, or why not.
pub fn open(dir: &Path, limits: &Limits) -> Result<File, (Refused, String)> {
    match in_memory(dir) {
        Ok(true) => return Err((Refused::InMemory, dir.display().to_string())),
        Ok(false) => {}
        Err(e) => return Err((Refused::NotWritable, e.to_string())),
    }
    let (free, total) = space(dir).map_err(|e| (Refused::NotWritable, e.to_string()))?;
    if free < (limits.floor)(total) {
        return Err((Refused::LowSpace, format!("{free} bytes free")));
    }
    anonymous(dir).map_err(|e| (Refused::NotWritable, e.to_string()))
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

/// What a finished writer leaves: the file, how many samples reached it, and why it
/// stopped early if it did.
pub struct Done {
    pub file: File,
    pub samples: usize,
    pub stop: Option<Stop>,
}

/// Writes samples to a spool on a thread of its own. [`Writer::push`] never waits.
pub struct Writer {
    tx: Option<mpsc::Sender<Bytes>>,
    queued: Arc<AtomicUsize>,
    stopped: Arc<Mutex<Option<Stop>>>,
    accepted: u64,
    limits: Limits,
    thread: Option<std::thread::JoinHandle<Done>>,
}

impl Writer {
    pub fn spawn(file: File, dir: PathBuf, limits: Limits) -> io::Result<Writer> {
        let (tx, rx) = mpsc::channel::<Bytes>();
        let queued = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(Mutex::new(None));
        let (q, s, l) = (queued.clone(), stopped.clone(), limits.clone());
        let thread = std::thread::Builder::new()
            .name("recording-spool".into())
            .spawn(move || write_all(file, rx, q, s, dir, l))?;
        Ok(Writer {
            tx: Some(tx),
            queued,
            stopped,
            accepted: 0,
            limits,
            thread: Some(thread),
        })
    }

    /// Queues one sample, or says why the recording has to end instead. Never waits.
    pub fn push(&mut self, sample: Bytes) -> Result<(), Stop> {
        if let Some(stop) = self.stopped.lock().clone() {
            return Err(stop);
        }
        let len = sample.len();
        if self.accepted + len as u64 > self.limits.max_bytes {
            return Err(Stop::Full);
        }
        if self.queued.load(Ordering::Acquire) + len > self.limits.queue_bytes {
            return Err(Stop::Slow);
        }
        self.queued.fetch_add(len, Ordering::AcqRel);
        let sent = self.tx.as_ref().map(|tx| tx.send(sample));
        if !matches!(sent, Some(Ok(()))) {
            self.queued.fetch_sub(len, Ordering::AcqRel);
            let stop = self.stopped.lock().clone();
            return Err(stop.unwrap_or_else(|| Stop::Error("the writer stopped".into())));
        }
        self.accepted += len as u64;
        Ok(())
    }

    /// Lets the writer write what is queued and stop; blocks until it has.
    pub fn finish(mut self) -> io::Result<Done> {
        self.tx.take();
        match self.thread.take().map(|t| t.join()) {
            Some(Ok(done)) => Ok(done),
            _ => Err(io::Error::other("the recording's writer failed")),
        }
    }
}

/// The writer thread: samples into a buffer, the buffer into the file. The samples that
/// count are those whose bytes reached the file.
fn write_all(
    mut file: File,
    rx: mpsc::Receiver<Bytes>,
    queued: Arc<AtomicUsize>,
    stopped: Arc<Mutex<Option<Stop>>>,
    dir: PathBuf,
    limits: Limits,
) -> Done {
    let mut buf: Vec<u8> = Vec::with_capacity(limits.buffer);
    let (mut bytes, mut samples, mut pending, mut since_check) = (0u64, 0usize, 0usize, 0u64);
    let mut flush = |buf: &mut Vec<u8>,
                     pending: &mut usize,
                     bytes: &mut u64,
                     samples: &mut usize|
     -> Result<(), Stop> {
        if buf.is_empty() {
            return Ok(());
        }
        file.write_all(buf).map_err(|e| Stop::Error(e.to_string()))?;
        *bytes += buf.len() as u64;
        since_check += buf.len() as u64;
        *samples += std::mem::take(pending);
        buf.clear();
        if since_check >= limits.check_every {
            since_check = 0;
            let (free, total) = space(&dir).map_err(|e| Stop::Error(e.to_string()))?;
            if free < (limits.floor)(total) {
                return Err(Stop::Space);
            }
        }
        Ok(())
    };
    let mut stop = None;
    while let Ok(s) = rx.recv() {
        if let Some(hook) = &limits.before_write {
            if let Err(e) = hook(bytes) {
                stop = Some(Stop::Error(e.to_string()));
                break;
            }
        }
        queued.fetch_sub(s.len(), Ordering::AcqRel);
        buf.extend_from_slice(&s);
        pending += 1;
        if buf.len() >= limits.buffer {
            if let Err(e) = flush(&mut buf, &mut pending, &mut bytes, &mut samples) {
                stop = Some(e);
                break;
            }
        }
    }
    if stop.is_none() {
        stop = flush(&mut buf, &mut pending, &mut bytes, &mut samples).err();
    }
    *stopped.lock() = stop.clone();
    drop(rx);
    Done {
        file,
        samples,
        stop,
    }
}
