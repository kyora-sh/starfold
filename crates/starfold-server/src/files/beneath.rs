//! Paths under the files root, resolved one directory handle at a time. On Unix every
//! step goes through a held directory fd (`openat2` with `RESOLVE_BENEATH` and
//! `RESOLVE_NO_SYMLINKS` on Linux where the kernel has it, else an `O_NOFOLLOW` walk),
//! and folders, files and renames are made relative to it, so a symlink, wherever it
//! is, is never followed. Windows has no `*at` calls: each step is checked with
//! `symlink_metadata` (symlinks and junctions refused) and then used by path.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug)]
pub enum Refused {
    Missing,
    NotFolder,
    Link,
    Special,
    Io(io::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Link,
    Other,
}

pub struct Entry {
    pub name: OsString,
    pub kind: Kind,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

pub enum Opened {
    File(std::fs::File),
    Dir(Dir),
}

pub struct Root {
    path: PathBuf,
    dir: Dir,
}

impl Root {
    pub fn open(path: &Path) -> io::Result<Root> {
        let path = std::fs::canonicalize(path)?;
        if !std::fs::metadata(&path)?.is_dir() {
            return Err(io::Error::new(io::ErrorKind::NotADirectory, "not a folder"));
        }
        let dir = Dir::open_root(&path)?;
        Ok(Root { path, dir })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The folder at `parts` (plain names) under the root; with `create`, missing
    /// folders are made on the way.
    pub fn dir(&self, parts: &[&str], create: bool) -> Result<Dir, Refused> {
        #[cfg(target_os = "linux")]
        if let Some(d) = self.dir.beneath(parts, create)? {
            return Ok(d);
        }
        let mut d = self.dir.try_clone()?;
        for p in parts {
            d = d.child(OsStr::new(p), create)?;
        }
        Ok(d)
    }
}

impl From<io::Error> for Refused {
    fn from(e: io::Error) -> Self {
        Refused::Io(e)
    }
}

#[cfg(unix)]
pub use unix::Dir;
#[cfg(windows)]
pub use windows::Dir;

#[cfg(unix)]
mod unix {
    use super::{Entry, Kind, Opened, Refused};
    use nix::errno::Errno;
    use nix::fcntl::{openat, renameat, AtFlags, OFlag};
    use nix::sys::stat::{fstat, fstatat, mkdirat, FileStat, Mode};
    use nix::unistd::{unlinkat, UnlinkatFlags};
    use std::ffi::OsStr;
    use std::os::fd::{AsFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    pub struct Dir(OwnedFd);

    const DIR: OFlag = OFlag::O_RDONLY
        .union(OFlag::O_DIRECTORY)
        .union(OFlag::O_NOFOLLOW)
        .union(OFlag::O_CLOEXEC);

    fn kind(st: &FileStat) -> Kind {
        match st.st_mode as libc::mode_t & libc::S_IFMT {
            libc::S_IFDIR => Kind::Dir,
            libc::S_IFREG => Kind::File,
            libc::S_IFLNK => Kind::Link,
            _ => Kind::Other,
        }
    }

    fn io(e: Errno) -> Refused {
        Refused::Io(e.into())
    }

    impl Dir {
        pub(super) fn open_root(path: &Path) -> std::io::Result<Dir> {
            Ok(Dir(nix::fcntl::open(path, DIR, Mode::empty())?))
        }

        pub(super) fn try_clone(&self) -> std::io::Result<Dir> {
            Ok(Dir(self.0.try_clone()?))
        }

        /// What a failed `O_NOFOLLOW` open of `name` met.
        fn why(&self, name: &OsStr, e: Errno) -> Refused {
            match e {
                Errno::ENOENT => Refused::Missing,
                Errno::ELOOP | Errno::EMLINK => Refused::Link,
                Errno::ENOTDIR => match fstatat(self.0.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW)
                {
                    Ok(st) if kind(&st) == Kind::Link => Refused::Link,
                    _ => Refused::NotFolder,
                },
                e => io(e),
            }
        }

        /// `parts` in one `openat2` that refuses symlinks and anything outside this
        /// folder; None where the kernel can't, or (with `create`) something is missing.
        #[cfg(target_os = "linux")]
        pub(super) fn beneath(&self, parts: &[&str], create: bool) -> Result<Option<Dir>, Refused> {
            use nix::fcntl::{openat2, OpenHow, ResolveFlag};
            if parts.is_empty() {
                return Ok(None);
            }
            let how = OpenHow::new().flags(DIR).resolve(
                ResolveFlag::RESOLVE_BENEATH
                    | ResolveFlag::RESOLVE_NO_SYMLINKS
                    | ResolveFlag::RESOLVE_NO_MAGICLINKS,
            );
            match openat2(self.0.as_fd(), parts.join("/").as_str(), how) {
                Ok(fd) => Ok(Some(Dir(fd))),
                Err(
                    Errno::ENOSYS | Errno::EPERM | Errno::EINVAL | Errno::E2BIG | Errno::EAGAIN,
                ) => Ok(None),
                Err(Errno::ENOENT) if create => Ok(None),
                Err(Errno::ENOENT) => Err(Refused::Missing),
                Err(Errno::ELOOP | Errno::EXDEV) => Err(Refused::Link),
                Err(Errno::ENOTDIR) => Err(Refused::NotFolder),
                Err(e) => Err(io(e)),
            }
        }

        /// The folder `name` in this one, made first if missing and `create`.
        pub fn child(&self, name: &OsStr, create: bool) -> Result<Dir, Refused> {
            match openat(self.0.as_fd(), name, DIR, Mode::empty()) {
                Ok(fd) => Ok(Dir(fd)),
                Err(Errno::ENOENT) if create => {
                    match mkdirat(self.0.as_fd(), name, Mode::from_bits_truncate(0o777)) {
                        Ok(()) | Err(Errno::EEXIST) => {}
                        Err(e) => return Err(io(e)),
                    }
                    openat(self.0.as_fd(), name, DIR, Mode::empty())
                        .map(Dir)
                        .map_err(|e| self.why(name, e))
                }
                Err(e) => Err(self.why(name, e)),
            }
        }

        /// `name` opened for reading: a regular file or a folder, never through a symlink.
        pub fn open(&self, name: &OsStr) -> Result<Opened, Refused> {
            // O_NONBLOCK: a FIFO swapped in opens at once (and is then refused).
            let flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
            let fd = openat(self.0.as_fd(), name, flags, Mode::empty())
                .map_err(|e| self.why(name, e))?;
            match kind(&fstat(fd.as_fd()).map_err(io)?) {
                Kind::Dir => Ok(Opened::Dir(Dir(fd))),
                Kind::File => Ok(Opened::File(fd.into())),
                _ => Err(Refused::Special),
            }
        }

        /// A new file `name`, failing if anything (a symlink included) is there already.
        pub fn create_new(&self, name: &OsStr) -> std::io::Result<std::fs::File> {
            let flags = OFlag::O_WRONLY
                | OFlag::O_CREAT
                | OFlag::O_EXCL
                | OFlag::O_NOFOLLOW
                | OFlag::O_CLOEXEC;
            Ok(openat(self.0.as_fd(), name, flags, Mode::from_bits_truncate(0o666))?.into())
        }

        pub fn exists(&self, name: &OsStr) -> bool {
            fstatat(self.0.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW).is_ok()
        }

        pub fn rename(&self, from: &OsStr, to: &OsStr) -> std::io::Result<()> {
            Ok(renameat(self.0.as_fd(), from, self.0.as_fd(), to)?)
        }

        pub fn remove(&self, name: &OsStr) -> std::io::Result<()> {
            Ok(unlinkat(self.0.as_fd(), name, UnlinkatFlags::NoRemoveDir)?)
        }

        /// What this folder holds, symlinks reported as links and not followed.
        pub fn entries(&self) -> std::io::Result<Vec<Entry>> {
            let fresh = openat(self.0.as_fd(), ".", DIR, Mode::empty())?;
            let mut d = nix::dir::Dir::from_fd(fresh)?;
            let mut v = Vec::new();
            for e in d.iter() {
                let e = e?;
                let name = OsStr::from_bytes(e.file_name().to_bytes());
                if name == "." || name == ".." {
                    continue;
                }
                let Ok(st) = fstatat(self.0.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW) else {
                    continue;
                };
                let modified = (st.st_mtime >= 0).then(|| {
                    SystemTime::UNIX_EPOCH
                        + Duration::new(st.st_mtime as u64, st.st_mtime_nsec as u32)
                });
                v.push(Entry {
                    name: name.to_owned(),
                    kind: kind(&st),
                    len: st.st_size as u64,
                    modified,
                });
            }
            Ok(v)
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::{Entry, Kind, Opened, Refused};
    use std::ffi::OsStr;
    use std::fs::Metadata;
    use std::path::{Path, PathBuf};

    pub struct Dir(PathBuf);

    fn kind(md: &Metadata) -> Kind {
        let t = md.file_type();
        if t.is_symlink() {
            Kind::Link
        } else if t.is_dir() {
            Kind::Dir
        } else if t.is_file() {
            Kind::File
        } else {
            Kind::Other
        }
    }

    fn look(p: &Path) -> Result<Kind, Refused> {
        match std::fs::symlink_metadata(p) {
            Ok(md) => Ok(kind(&md)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Refused::Missing),
            Err(e) => Err(Refused::Io(e)),
        }
    }

    impl Dir {
        pub(super) fn open_root(path: &Path) -> std::io::Result<Dir> {
            Ok(Dir(path.to_path_buf()))
        }

        pub(super) fn try_clone(&self) -> std::io::Result<Dir> {
            Ok(Dir(self.0.clone()))
        }

        pub fn child(&self, name: &OsStr, create: bool) -> Result<Dir, Refused> {
            let p = self.0.join(name);
            let k = match look(&p) {
                Err(Refused::Missing) if create => {
                    if let Err(e) = std::fs::create_dir(&p) {
                        if e.kind() != std::io::ErrorKind::AlreadyExists {
                            return Err(Refused::Io(e));
                        }
                    }
                    look(&p)?
                }
                k => k?,
            };
            match k {
                Kind::Dir => Ok(Dir(p)),
                Kind::Link => Err(Refused::Link),
                _ => Err(Refused::NotFolder),
            }
        }

        pub fn open(&self, name: &OsStr) -> Result<Opened, Refused> {
            let p = self.0.join(name);
            match look(&p)? {
                Kind::Dir => Ok(Opened::Dir(Dir(p))),
                Kind::File => Ok(Opened::File(std::fs::File::open(&p)?)),
                Kind::Link => Err(Refused::Link),
                Kind::Other => Err(Refused::Special),
            }
        }

        pub fn create_new(&self, name: &OsStr) -> std::io::Result<std::fs::File> {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.0.join(name))
        }

        pub fn exists(&self, name: &OsStr) -> bool {
            std::fs::symlink_metadata(self.0.join(name)).is_ok()
        }

        pub fn rename(&self, from: &OsStr, to: &OsStr) -> std::io::Result<()> {
            std::fs::rename(self.0.join(from), self.0.join(to))
        }

        pub fn remove(&self, name: &OsStr) -> std::io::Result<()> {
            std::fs::remove_file(self.0.join(name))
        }

        pub fn entries(&self) -> std::io::Result<Vec<Entry>> {
            let mut v = Vec::new();
            for e in std::fs::read_dir(&self.0)? {
                let e = e?;
                let Ok(md) = std::fs::symlink_metadata(e.path()) else {
                    continue;
                };
                v.push(Entry {
                    name: e.file_name(),
                    kind: kind(&md),
                    len: md.len(),
                    modified: md.modified().ok(),
                });
            }
            Ok(v)
        }
    }
}
