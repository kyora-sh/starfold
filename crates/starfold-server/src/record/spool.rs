//! Where a recording's video goes while it runs: a file on disk that has no name. Linux
//! opens it with O_TMPFILE; elsewhere (and on a Linux filesystem without it) it is created
//! in a 0700 directory of the server's own inside `--recording-dir` and unlinked at once
//! (Windows: deleted when closed). Memory filesystems are refused: a recording there is
//! RAM by another name.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

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

/// The directory of the server's own inside `dir` that holds named spools while they are
/// created.
const OWN: &str = ".starfold-recordings";

/// Whether `dir` is `root` or inside it, both resolved. A directory that doesn't exist yet
/// is taken as written, below its closest parent that does.
pub fn inside(dir: &Path, root: &Path) -> bool {
    let resolve = |p: &Path| {
        let mut rest = Vec::new();
        let mut at = p.to_path_buf();
        loop {
            if let Ok(c) = at.canonicalize() {
                return rest.into_iter().rev().fold(c, |c, r| c.join(r));
            }
            match (at.file_name().map(|n| n.to_owned()), at.parent()) {
                (Some(n), Some(parent)) => {
                    rest.push(n);
                    at = parent.to_path_buf();
                }
                _ => return p.to_path_buf(),
            }
        }
    };
    resolve(dir).starts_with(resolve(root))
}

/// Why a spool couldn't be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The directory can't take a file.
    NotWritable,
    /// The directory is memory (tmpfs, ramfs).
    InMemory,
    /// Less free space than [`floor`].
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

/// Whether `dir` is on a filesystem that lives in memory.
#[cfg(target_os = "linux")]
fn in_memory(dir: &Path) -> io::Result<bool> {
    const TMPFS: u32 = 0x0102_1994;
    const RAMFS: u32 = 0x8584_58f6;
    let st = nix::sys::statfs::statfs(dir).map_err(io::Error::from)?;
    // f_type is 64 bits wide (signed on glibc, unsigned on musl); the magic numbers are 32.
    Ok(matches!(st.filesystem_type().0 as u32, TMPFS | RAMFS))
}

#[cfg(target_os = "macos")]
fn in_memory(dir: &Path) -> io::Result<bool> {
    let st = nix::sys::statfs::statfs(dir).map_err(io::Error::from)?;
    Ok(matches!(st.filesystem_type_name(), "tmpfs" | "ramfs"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn in_memory(_dir: &Path) -> io::Result<bool> {
    Ok(false)
}

/// The server's own directory in `dir`, created 0700 at the first recording. One that is
/// there already must be a directory (not a link) that only this user can enter.
fn own_dir(dir: &Path) -> io::Result<PathBuf> {
    let own = dir.join(OWN);
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
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
/// server.
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

/// A spool for one recording in `dir`, or why not. Blocks: call it off the async workers.
pub fn open(dir: &Path, floor: &dyn Fn(u64) -> u64) -> Result<File, (Refused, String)> {
    match in_memory(dir) {
        Ok(true) => return Err((Refused::InMemory, dir.display().to_string())),
        Ok(false) => {}
        Err(e) => return Err((Refused::NotWritable, e.to_string())),
    }
    let file = anonymous(dir).map_err(|e| (Refused::NotWritable, e.to_string()))?;
    let (free, total) = space(&file, dir).map_err(|e| (Refused::NotWritable, e.to_string()))?;
    if free < floor(total) {
        return Err((Refused::LowSpace, format!("{free} bytes free")));
    }
    Ok(file)
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
