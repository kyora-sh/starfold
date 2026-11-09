//! Paths under the files root, resolved one directory handle at a time. On Unix every
//! step goes through a held directory fd (`openat2` with `RESOLVE_BENEATH` and
//! `RESOLVE_NO_SYMLINKS` on Linux where the kernel has it, else an `O_NOFOLLOW` walk),
//! and folders, files and renames are made relative to it, so a symlink, wherever it
//! is, is never followed. Windows has no `*at` calls: each name must be a plain one
//! ([`windows_plain`]), the joined path must stay under the root, and each step is
//! checked with `symlink_metadata` (symlinks and junctions refused) and then used by path.

use std::ffi::{OsStr, OsString};
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug)]
pub enum Refused {
    Missing,
    NotFolder,
    Link,
    Special,
    #[cfg_attr(unix, allow(dead_code))]
    BadName,
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

/// Neither an exclusive rename nor a hard link: an upload can't be saved without risking
/// replacing a file another upload saved under the same name at the same moment.
#[cfg(unix)]
fn no_exclusive_rename() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "this filesystem can't save an upload without risking replacing another one",
    )
}

#[cfg(test)]
thread_local! {
    /// Entries looked up by [`Dir::entries`] on this thread.
    pub static STATS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether Windows takes `name` as a plain name inside its folder: no drive, UNC or
/// stream syntax (any `:`), no separator or wildcard, no device name (`NUL`, `COM1.txt`)
/// and no trailing dot or space, which Windows would drop.
#[cfg_attr(unix, allow(dead_code))]
pub fn windows_plain(name: &OsStr) -> bool {
    let Some(s) = name.to_str() else {
        return false;
    };
    if s.is_empty()
        || s.ends_with(['.', ' '])
        || s.chars()
            .any(|c| c < ' ' || matches!(c, ':' | '/' | '\\' | '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return false;
    }
    let stem = s.split('.').next().unwrap_or("").trim_end().to_uppercase();
    let numbered = |p: &str| {
        stem.strip_prefix(p).is_some_and(|n| {
            let mut c = n.chars();
            matches!(
                (c.next(), c.next()),
                (Some('0'..='9' | '¹' | '²' | '³'), None)
            )
        })
    };
    !matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) && !numbered("COM")
        && !numbered("LPT")
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
    use super::{no_exclusive_rename, ControlFlow, Entry, Kind, Opened, Refused};
    use nix::errno::Errno;
    use nix::fcntl::{openat, AtFlags, OFlag};
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

    /// What a failed `openat2` means: Ok to take the component walk instead (the kernel
    /// can't, the joined path is too long, a non-folder is on the way, or with `create`
    /// something is missing), else the refusal.
    #[cfg(target_os = "linux")]
    pub(super) fn openat2_failed(e: Errno, create: bool) -> Result<(), Refused> {
        match e {
            Errno::ENOSYS
            | Errno::EPERM
            | Errno::EINVAL
            | Errno::E2BIG
            | Errno::EAGAIN
            | Errno::ENAMETOOLONG => Ok(()),
            Errno::ENOENT if create => Ok(()),
            // A file or a symlink on the way: the walk tells which.
            Errno::ENOTDIR => Ok(()),
            Errno::ENOENT => Err(Refused::Missing),
            Errno::ELOOP | Errno::EXDEV => Err(Refused::Link),
            e => Err(io(e)),
        }
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
        /// folder; None where the kernel can't, where a non-folder is in the way, or
        /// (with `create`) something is missing.
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
                Err(e) => openat2_failed(e, create).map(|()| None),
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

        /// The device and inode of this folder.
        pub fn id(&self) -> Option<(u64, u64)> {
            let st = fstat(self.0.as_fd()).ok()?;
            Some((st.st_dev as u64, st.st_ino as u64))
        }

        /// Renames `from` to `to`, failing with `AlreadyExists` if `to` is there, however
        /// close together two uploads finish. Without an exclusive rename it links and
        /// unlinks; a filesystem with neither is `Unsupported`, never a replacing rename.
        pub fn publish(&self, from: &OsStr, to: &OsStr) -> std::io::Result<()> {
            let fd = self.0.as_fd();
            #[cfg(all(target_os = "linux", target_env = "gnu"))]
            {
                use nix::fcntl::{renameat2, RenameFlags};
                match renameat2(fd, from, fd, to, RenameFlags::RENAME_NOREPLACE) {
                    // EPERM: a seccomp filter that allows the rest (the resolver falls back too).
                    Err(Errno::EINVAL | Errno::ENOSYS | Errno::EPERM) => {}
                    r => return Ok(r?),
                }
            }
            #[cfg(target_os = "macos")]
            {
                use std::os::fd::AsRawFd;
                let (from, to) = (cstr(from)?, cstr(to)?);
                let r = unsafe {
                    libc::renameatx_np(
                        fd.as_raw_fd(),
                        from.as_ptr(),
                        fd.as_raw_fd(),
                        to.as_ptr(),
                        libc::RENAME_EXCL,
                    )
                };
                match Errno::result(r) {
                    Err(Errno::ENOTSUP | Errno::EINVAL) => {}
                    r => return Ok(r.map(drop)?),
                }
            }
            match nix::unistd::linkat(fd, from, fd, to, AtFlags::empty()) {
                Ok(()) => Ok(unlinkat(fd, from, UnlinkatFlags::NoRemoveDir)?),
                // ENOTSUP and EOPNOTSUPP are one value on Linux, two on macOS.
                Err(e)
                    if [
                        Errno::EPERM,
                        Errno::ENOTSUP,
                        Errno::EOPNOTSUPP,
                        Errno::ENOSYS,
                    ]
                    .contains(&e) =>
                {
                    Err(no_exclusive_rename())
                }
                Err(e) => Err(e.into()),
            }
        }

        pub fn remove(&self, name: &OsStr) -> std::io::Result<()> {
            Ok(unlinkat(self.0.as_fd(), name, UnlinkatFlags::NoRemoveDir)?)
        }

        /// What this folder holds, one entry at a time until `each` breaks, symlinks
        /// reported as links and not followed.
        pub fn entries(
            &self,
            mut each: impl FnMut(Entry) -> ControlFlow<()>,
        ) -> std::io::Result<()> {
            let fresh = openat(self.0.as_fd(), ".", DIR, Mode::empty())?;
            let mut d = nix::dir::Dir::from_fd(fresh)?;
            for e in d.iter() {
                let e = e?;
                let name = OsStr::from_bytes(e.file_name().to_bytes());
                if name == "." || name == ".." {
                    continue;
                }
                #[cfg(test)]
                super::STATS.with(|n| n.set(n.get() + 1));
                let st = match fstatat(self.0.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
                    Ok(st) => st,
                    // Gone since it was listed.
                    Err(Errno::ENOENT) => continue,
                    Err(e) => return Err(e.into()),
                };
                let modified = (st.st_mtime >= 0).then(|| {
                    SystemTime::UNIX_EPOCH
                        + Duration::new(st.st_mtime as u64, st.st_mtime_nsec as u32)
                });
                let e = Entry {
                    name: name.to_owned(),
                    kind: kind(&st),
                    len: st.st_size as u64,
                    modified,
                };
                if each(e).is_break() {
                    break;
                }
            }
            Ok(())
        }
    }

    #[cfg(target_os = "macos")]
    fn cstr(s: &OsStr) -> std::io::Result<std::ffi::CString> {
        std::ffi::CString::new(s.as_bytes()).map_err(std::io::Error::other)
    }
}

#[cfg(windows)]
mod windows {
    use super::{windows_plain, ControlFlow, Entry, Kind, Opened, Refused};
    use std::ffi::OsStr;
    use std::fs::Metadata;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    pub struct Dir {
        path: PathBuf,
        root: Arc<Path>,
    }

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
            Ok(Dir {
                path: path.to_path_buf(),
                root: path.into(),
            })
        }

        pub(super) fn try_clone(&self) -> std::io::Result<Dir> {
            Ok(self.under(self.path.clone()))
        }

        fn under(&self, path: PathBuf) -> Dir {
            Dir {
                path,
                root: self.root.clone(),
            }
        }

        /// `name` in this folder: a plain name, and the result still under the root.
        fn at(&self, name: &OsStr) -> Result<PathBuf, Refused> {
            if !windows_plain(name) {
                return Err(Refused::BadName);
            }
            let p = self.path.join(name);
            if p.parent() != Some(self.path.as_path()) || !p.starts_with(&self.root) {
                return Err(Refused::BadName);
            }
            Ok(p)
        }

        fn at_io(&self, name: &OsStr) -> std::io::Result<PathBuf> {
            self.at(name)
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid name"))
        }

        pub fn id(&self) -> Option<(u64, u64)> {
            None
        }

        pub fn child(&self, name: &OsStr, create: bool) -> Result<Dir, Refused> {
            let p = self.at(name)?;
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
                Kind::Dir => Ok(self.under(p)),
                Kind::Link => Err(Refused::Link),
                _ => Err(Refused::NotFolder),
            }
        }

        pub fn open(&self, name: &OsStr) -> Result<Opened, Refused> {
            let p = self.at(name)?;
            match look(&p)? {
                Kind::Dir => Ok(Opened::Dir(self.under(p))),
                Kind::File => Ok(Opened::File(std::fs::File::open(&p)?)),
                Kind::Link => Err(Refused::Link),
                Kind::Other => Err(Refused::Special),
            }
        }

        pub fn create_new(&self, name: &OsStr) -> std::io::Result<std::fs::File> {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.at_io(name)?)
        }

        pub fn exists(&self, name: &OsStr) -> bool {
            self.at(name)
                .is_ok_and(|p| std::fs::symlink_metadata(p).is_ok())
        }

        /// `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`: it fails if `to` is there.
        pub fn publish(&self, from: &OsStr, to: &OsStr) -> std::io::Result<()> {
            use windows::core::HSTRING;
            use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVE_FILE_FLAGS};
            let (from, to) = (self.at_io(from)?, self.at_io(to)?);
            let (from, to) = (HSTRING::from(from.as_path()), HSTRING::from(to.as_path()));
            unsafe { MoveFileExW(&from, &to, MOVE_FILE_FLAGS(0)) }.map_err(|e| {
                let code = e.code().0 as u32;
                if code >> 16 == 0x8007 {
                    std::io::Error::from_raw_os_error((code & 0xffff) as i32)
                } else {
                    std::io::Error::other(e)
                }
            })
        }

        pub fn remove(&self, name: &OsStr) -> std::io::Result<()> {
            std::fs::remove_file(self.at_io(name)?)
        }

        pub fn entries(
            &self,
            mut each: impl FnMut(Entry) -> ControlFlow<()>,
        ) -> std::io::Result<()> {
            for e in std::fs::read_dir(&self.path)? {
                let e = e?;
                let md = match std::fs::symlink_metadata(e.path()) {
                    Ok(md) => md,
                    // Gone since it was listed.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e),
                };
                let e = Entry {
                    name: e.file_name(),
                    kind: kind(&md),
                    len: md.len(),
                    modified: md.modified().ok(),
                };
                if each(e).is_break() {
                    break;
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_takes_only_plain_names() {
        for ok in [
            "a.txt",
            "report 2024.pdf",
            ".hidden",
            "com10",
            "console",
            "nul2.txt",
            "Ünïcode",
        ] {
            assert!(windows_plain(OsStr::new(ok)), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "C:",
            "C:outside",
            "file.txt:stream",
            "a:b",
            "a\\b",
            "a/b",
            "\\\\server\\share",
            "trailing.",
            "trailing ",
            "CON",
            "con.txt",
            "NUL",
            "Aux.tar.gz",
            "PRN ",
            "COM1",
            "lpt9.log",
            "COM\u{b9}",
            "CONIN$",
            "a*b",
            "a?b",
            "a\u{1}b",
        ] {
            assert!(!windows_plain(OsStr::new(bad)), "{bad:?}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn openat2_failures_that_take_the_walk() {
        use nix::errno::Errno;
        use unix::openat2_failed;
        for e in [
            Errno::ENOSYS,
            Errno::EPERM,
            Errno::EINVAL,
            Errno::E2BIG,
            Errno::EAGAIN,
            Errno::ENAMETOOLONG,
            Errno::ENOTDIR,
        ] {
            assert!(openat2_failed(e, false).is_ok(), "{e}");
        }
        assert!(openat2_failed(Errno::ENOENT, true).is_ok());
        assert!(matches!(
            openat2_failed(Errno::ENOENT, false),
            Err(Refused::Missing)
        ));
        assert!(matches!(
            openat2_failed(Errno::ELOOP, false),
            Err(Refused::Link)
        ));
        assert!(matches!(
            openat2_failed(Errno::EXDEV, true),
            Err(Refused::Link)
        ));
        assert!(matches!(
            openat2_failed(Errno::EACCES, false),
            Err(Refused::Io(_))
        ));
    }
}
