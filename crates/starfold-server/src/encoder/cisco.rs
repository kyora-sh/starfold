//! Cisco's OpenH264 binary, loaded at run time (the `openh264-cisco` feature, on a target with a
//! pinned Cisco build). starfold neither contains nor downloads it.
//!
//! One rule on every platform: the process loads only bytes it wrote itself after hashing them.
//! The file from `--openh264-lib` or `$STARFOLD_OPENH264_LIB` is read once into memory (a
//! regular file of at most 16 MiB, opened without blocking), its sha256 must be the pinned Cisco
//! build for this platform, and those bytes are written under a random name into a new directory
//! starfold creates for this user alone, read back and hashed again, and loaded from there. The
//! copy is removed once loaded; on Windows, where a loaded DLL can't be removed, it is held open
//! against writing and deleting until the process ends, and a later start removes it.

use openh264_sys2 as sys;
use sha2::Digest;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use sys::API;

/// Cisco's OpenH264 2.6.0 builds starfold loads: (sha256, Cisco's file name). Each is also in
/// openh264-sys2's own list of the builds its bindings fit.
pub const PINS: &[(&str, &str)] = &[
    (
        "2f0cde7c6a6abcf5cae76942894ea42897fa677bce4ed6c91a24dd1b041d5f04",
        "libopenh264-2.6.0-linux64.8.so",
    ),
    (
        "12e7b33623667cdab0e575170c147b1b36eadb77d0d2aa7ceb5afd3e58902140",
        "libopenh264-2.6.0-linux-arm64.8.so",
    ),
    (
        "e3dc8bc01fe69363f61fd3c02fd27798537a585eadd38cd808f303d1ee505a19",
        "libopenh264-2.6.0-mac-x64.dylib",
    ),
    (
        "052e98bfcf7a9167d22f3bbb3f5988ef79065591f36af8b52924b22b13624551",
        "libopenh264-2.6.0-mac-arm64.dylib",
    ),
    (
        "2076cb5675ec6c1a4c70e7a2a322552f547b6eeed649d6dfcd9e02a543b24691",
        "openh264-2.6.0-win64.dll",
    ),
];

/// The library's path when `--openh264-lib` isn't given.
pub const LIB_ENV: &str = "STARFOLD_OPENH264_LIB";

/// The text Cisco's binary licence asks software using it to display.
pub const ATTRIBUTION: &str = "OpenH264 Video Codec provided by Cisco Systems, Inc.";

/// Larger than any pinned build (the biggest is under 2 MiB).
const MAX_BYTES: u64 = 16 << 20;

/// The prefix of the private directories the checked copies are written to.
const DIR_PREFIX: &str = "starfold-openh264-";

/// Cisco's file name for this platform, if a build is pinned for it.
pub fn platform_file() -> Option<&'static str> {
    let name = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "libopenh264-2.6.0-linux64.8.so",
        ("linux", "aarch64") => "libopenh264-2.6.0-linux-arm64.8.so",
        ("macos", "x86_64") => "libopenh264-2.6.0-mac-x64.dylib",
        ("macos", "aarch64") => "libopenh264-2.6.0-mac-arm64.dylib",
        ("windows", "x86_64") => "openh264-2.6.0-win64.dll",
        _ => return None,
    };
    Some(name)
}

fn platform_pins() -> Vec<(&'static str, &'static str)> {
    PINS.iter()
        .copied()
        .filter(|(_, name)| Some(*name) == platform_file())
        .collect()
}

struct Loaded {
    api: sys::DynamicAPI,
    what: String,
    /// Windows: the copy, held open until the process ends.
    _copy: Option<PrivateCopy>,
}

// The library's entry points are plain C functions, callable from any thread.
unsafe impl Send for Loaded {}
unsafe impl Sync for Loaded {}

static LIBRARY: OnceLock<Result<Loaded, String>> = OnceLock::new();

/// Loads OpenH264 from `path`, else from `$STARFOLD_OPENH264_LIB`, for the life of the
/// process, and says what was loaded; later calls return the first call's outcome.
pub fn load(path: Option<&Path>) -> Result<&'static str, String> {
    LIBRARY
        .get_or_init(|| open(path))
        .as_ref()
        .map(|l| l.what.as_str())
        .map_err(Clone::clone)
}

/// The loaded library (loading it from `$STARFOLD_OPENH264_LIB` if nothing has yet).
pub fn library() -> Result<&'static sys::DynamicAPI, String> {
    LIBRARY
        .get_or_init(|| open(None))
        .as_ref()
        .map(|l| &l.api)
        .map_err(Clone::clone)
}

fn no_library() -> String {
    match platform_file() {
        Some(f) => format!(
            "no OpenH264 library: get Cisco's {f} from https://www.openh264.org and pass its \
             path with --openh264-lib or {LIB_ENV}"
        ),
        None => format!(
            "no Cisco OpenH264 build is pinned for {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    }
}

fn open(path: Option<&Path>) -> Result<Loaded, String> {
    let env = std::env::var_os(LIB_ENV).filter(|p| !p.is_empty());
    let Some(path) = path.map(Path::to_path_buf).or(env.map(PathBuf::from)) else {
        // Tests encode with the dev-dependency's OpenH264 unless $STARFOLD_OPENH264_LIB
        // names Cisco's file; a starfold binary has no such fallback.
        #[cfg(test)]
        return Ok(Loaded {
            api: sys::DynamicAPI::from_source(),
            what: "OpenH264 compiled from source (tests only)".into(),
            _copy: None,
        });
        #[cfg(not(test))]
        return Err(no_library());
    };
    #[cfg(target_os = "macos")]
    if let Some(v) = dyld_overrides(std::env::vars_os()).first() {
        return Err(format!(
            "not loading OpenH264 while {v} is set: dyld could load another library in its place"
        ));
    }
    let checked = Checked::read(&path, &platform_pins())?;
    let copy = PrivateCopy::new(&checked).map_err(|e| {
        format!(
            "OpenH264 library {}: writing the checked copy: {e}",
            path.display()
        )
    })?;
    let api = copy.load()?;
    let v = unsafe { api.WelsGetCodecVersion() };
    if (v.uMajor, v.uMinor) != (2, 6) {
        return Err(format!(
            "OpenH264 library {}: reports version {}.{}, not 2.6",
            path.display(),
            v.uMajor,
            v.uMinor
        ));
    }
    let what = format!("{ATTRIBUTION} ({}, sha256 {})", checked.file, checked.sha256);
    log::info!("{what}, from {}", path.display());
    // The mapping outlives the file everywhere but on Windows.
    #[cfg(not(windows))]
    let copy = {
        drop(copy);
        None
    };
    #[cfg(windows)]
    let copy = Some(copy);
    Ok(Loaded {
        api,
        what,
        _copy: copy,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn random_hex() -> std::io::Result<String> {
    let mut tag = [0u8; 12];
    getrandom::fill(&mut tag).map_err(std::io::Error::other)?;
    Ok(tag.iter().map(|b| format!("{b:02x}")).collect())
}

/// A candidate's bytes, read once, whose sha256 is the pinned build `file`.
struct Checked {
    bytes: Vec<u8>,
    sha256: String,
    file: &'static str,
}

impl Checked {
    fn read(path: &Path, pins: &[(&'static str, &'static str)]) -> Result<Self, String> {
        let shown = path.display();
        let fail = |e: std::io::Error| format!("OpenH264 library {shown}: {e}");
        let f = open_candidate(path).map_err(fail)?;
        let meta = f.metadata().map_err(fail)?;
        if !meta.is_file() {
            return Err(format!("OpenH264 library {shown}: not a regular file"));
        }
        let too_large = || {
            format!(
                "OpenH264 library {shown}: larger than {} MiB, not a Cisco build",
                MAX_BYTES >> 20
            )
        };
        if meta.len() > MAX_BYTES {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        f.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(fail)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(too_large());
        }
        let sha256 = sha256_hex(&bytes);
        let Some(&(_, file)) = pins.iter().find(|(h, _)| *h == sha256) else {
            return Err(match platform_file() {
                Some(f) => format!(
                    "OpenH264 library {shown}: sha256 {sha256} is not Cisco's {f}, the build \
                     starfold pins for this platform"
                ),
                None => no_library(),
            });
        };
        Ok(Checked {
            bytes,
            sha256,
            file,
        })
    }
}

/// Opens the candidate for reading without blocking (a FIFO would otherwise hang here).
fn open_candidate(path: &Path) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
    }
    #[cfg(windows)]
    File::open(path)
}

/// Checked bytes written by this process into a new directory only this user can enter, under
/// a random name, read back and hashed again. Removed when dropped.
struct PrivateCopy {
    dir: PathBuf,
    file: PathBuf,
    /// Windows: open with read sharing only, so nobody can write, rename or delete the file.
    held: Option<File>,
}

impl PrivateCopy {
    fn new(c: &Checked) -> std::io::Result<Self> {
        Self::new_in(&std::env::temp_dir(), c)
    }

    fn new_in(base: &Path, c: &Checked) -> std::io::Result<Self> {
        #[cfg(windows)]
        {
            no_reparse_points(base)?;
            remove_stale(base);
        }
        let dir = base.join(format!("{DIR_PREFIX}{}", random_hex()?));
        create_private_dir(&dir)?;
        let ext = Path::new(c.file)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("lib");
        let file = dir.join(format!("{}.{ext}", random_hex()?));
        let mut copy = PrivateCopy {
            dir,
            file,
            held: None,
        };
        let mut w = create_private_file(&copy.file)?;
        w.write_all(&c.bytes)?;
        w.sync_all()?;
        drop(w);
        let mut held = open_held(&copy.file)?;
        let mut again = Vec::new();
        (&mut held)
            .take(MAX_BYTES + 1)
            .read_to_end(&mut again)?;
        if sha256_hex(&again) != c.sha256 {
            return Err(std::io::Error::other(
                "the copy is not the bytes that were checked",
            ));
        }
        #[cfg(windows)]
        no_reparse_points(&copy.file)?;
        copy.held = Some(held);
        Ok(copy)
    }

    fn load(&self) -> Result<sys::DynamicAPI, String> {
        let shown = self.file.display();
        // An absolute path: dlopen opens exactly that file, with no search.
        #[cfg(unix)]
        let api = unsafe { sys::libloading::APILoader::new(&self.file) };
        #[cfg(windows)]
        let api = unsafe {
            let path = self
                .file
                .to_str()
                .ok_or_else(|| format!("OpenH264 copy {shown}: not a UTF-8 path"))?;
            let lib = crate::winlib::absolute(path)?;
            sys::libloading::APILoader::from_library(lib)
        };
        api.map(sys::DynamicAPI::Libloading)
            .map_err(|e| format!("OpenH264 copy {shown}: {e}"))
    }
}

impl Drop for PrivateCopy {
    fn drop(&mut self) {
        self.held.take();
        let _ = std::fs::remove_file(&self.file);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

/// In the user's own temporary directory, whose ACL keeps other users out.
#[cfg(windows)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir(dir)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(unix)]
fn open_held(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

#[cfg(windows)]
fn create_private_file(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// Others may only read the file while this handle is open.
#[cfg(windows)]
fn open_held(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// Refuses a path with a symbolic link, junction or other reparse point anywhere on it.
#[cfg(windows)]
fn no_reparse_points(path: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    for p in path.ancestors().filter(|p| p.parent().is_some()) {
        if std::fs::symlink_metadata(p)?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::other(format!(
                "{} is a reparse point",
                p.display()
            )));
        }
    }
    Ok(())
}

/// Removes copies earlier processes left (a loaded DLL can't be removed while they run); one
/// still in use refuses, and one younger than an hour may belong to a process starting now.
#[cfg(windows)]
fn remove_stale(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() > 3600);
        if old && e.file_name().to_string_lossy().starts_with(DIR_PREFIX) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// The dyld variables that can make it load another library than the path it is given.
#[cfg(any(target_os = "macos", test))]
fn dyld_overrides(
    vars: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<String> {
    const NAMES: [&str; 9] = [
        "DYLD_LIBRARY_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
        "DYLD_FRAMEWORK_PATH",
        "DYLD_FALLBACK_FRAMEWORK_PATH",
        "DYLD_VERSIONED_LIBRARY_PATH",
        "DYLD_VERSIONED_FRAMEWORK_PATH",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_IMAGE_SUFFIX",
        "DYLD_ROOT_PATH",
    ];
    vars.filter(|(k, v)| !v.is_empty() && NAMES.iter().any(|n| k == n))
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sf-cisco-{tag}-{}", random_hex().unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn pin_of(bytes: &[u8]) -> [(&'static str, &'static str); 1] {
        [(sha256_hex(bytes).leak() as &str, "lib.so")]
    }

    #[test]
    fn only_the_pinned_build_is_read() {
        let dir = scratch("refuse");
        let missing = dir.join("missing.so");
        let e = Checked::read(&missing, &platform_pins()).err().unwrap();
        assert!(e.contains("missing.so"), "{e}");
        let other = dir.join("libopenh264.so");
        std::fs::write(&other, b"not cisco").unwrap();
        let e = Checked::read(&other, &platform_pins()).err().unwrap();
        assert!(e.contains(&sha256_hex(b"not cisco")), "{e}");
        assert!(
            e.contains("not Cisco's") || platform_file().is_none(),
            "{e}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Another platform's pinned build is refused here, whatever its hash.
    #[test]
    fn only_this_platforms_pin_counts() {
        let here = platform_pins();
        assert!(here.len() <= 1);
        if let Some(f) = platform_file() {
            assert_eq!(here, vec![*PINS.iter().find(|p| p.1 == f).unwrap()]);
        }
    }

    #[test]
    fn a_large_or_special_file_is_refused_before_reading() {
        let dir = scratch("large");
        let big = dir.join("big.so");
        File::create(&big).unwrap().set_len(MAX_BYTES + 1).unwrap();
        let e = Checked::read(&big, &platform_pins()).err().unwrap();
        assert!(e.contains("larger than"), "{e}");
        #[cfg(unix)]
        {
            let e = Checked::read(Path::new("/dev/zero"), &platform_pins())
                .err()
                .unwrap();
            assert!(e.contains("not a regular file"), "{e}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A FIFO with no writer would block an ordinary open; it is refused at once.
    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let dir = scratch("fifo");
        let fifo = dir.join("lib.so");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let p = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Checked::read(&p, &platform_pins()).err());
        });
        let e = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("reading a FIFO blocked")
            .expect("a FIFO was accepted");
        assert!(e.contains("not a regular file"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The copy starfold loads is its own: a new private directory, a random name, the
    /// checked bytes; and it is gone once dropped.
    #[test]
    fn the_copy_is_private_random_and_the_checked_bytes() {
        let dir = scratch("copy");
        let given = dir.join("lib.so");
        let bytes = b"the checked bytes".to_vec();
        std::fs::write(&given, &bytes).unwrap();
        let checked = Checked::read(&given, &pin_of(&bytes)).unwrap();
        let copy = PrivateCopy::new_in(&dir, &checked).unwrap();
        assert!(copy.file.starts_with(&copy.dir));
        let leaf = copy.file.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!leaf.contains("lib") && leaf.len() > 24, "{leaf}");
        assert!(copy
            .dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(DIR_PREFIX));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&copy.dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        assert_eq!(std::fs::read(&copy.file).unwrap(), bytes);
        let (d, f) = (copy.dir.clone(), copy.file.clone());
        drop(copy);
        assert!(!f.exists() && !d.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Replacing the given file after the check, in place or by a rename over it, changes
    /// nothing that is loaded: the loader reads starfold's own copy of the checked bytes.
    #[test]
    fn a_file_swapped_after_the_check_is_not_what_loads() {
        let dir = scratch("swap");
        let given = dir.join("lib.so");
        let bytes = b"the checked bytes".to_vec();
        std::fs::write(&given, &bytes).unwrap();
        let checked = Checked::read(&given, &pin_of(&bytes)).unwrap();
        std::fs::write(&given, b"swapped in place").unwrap();
        assert_eq!(std::fs::read(&given).unwrap(), b"swapped in place");
        let other = dir.join("other");
        std::fs::write(&other, b"swapped by rename").unwrap();
        std::fs::rename(&other, &given).unwrap();
        assert_eq!(std::fs::read(&given).unwrap(), b"swapped by rename");
        let copy = PrivateCopy::new_in(&dir, &checked).unwrap();
        assert_ne!(copy.file, given);
        assert_eq!(std::fs::read(&copy.file).unwrap(), bytes);
        drop(copy);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// With Cisco's real file (`$STARFOLD_OPENH264_LIB`, as CI fetches it): after the check,
    /// the given file is replaced by bytes that are no library at all, and what loads is
    /// still Cisco's OpenH264 2.6.
    #[test]
    fn ciscos_bytes_load_after_the_given_file_is_replaced() {
        let Some(real) = std::env::var_os(LIB_ENV).filter(|p| !p.is_empty()) else {
            eprintln!("skipped: {LIB_ENV} is not set");
            return;
        };
        let dir = scratch("real");
        let given = dir.join(platform_file().unwrap());
        std::fs::copy(&real, &given).unwrap();
        let checked = Checked::read(&given, &platform_pins()).unwrap();
        std::fs::write(&given, b"not a library").unwrap();
        assert_eq!(std::fs::read(&given).unwrap(), b"not a library");
        let copy = PrivateCopy::new_in(&dir, &checked).unwrap();
        let api = copy.load().unwrap();
        let v = unsafe { api.WelsGetCodecVersion() };
        assert_eq!((v.uMajor, v.uMinor), (2, 6));
        drop(api);
        drop(copy);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dyld_path_variables_are_named() {
        let vars = [
            ("DYLD_LIBRARY_PATH", "/tmp/x"),
            ("DYLD_PRINT_LIBRARIES", "1"),
            ("DYLD_INSERT_LIBRARIES", "/tmp/y.dylib"),
            ("DYLD_FALLBACK_LIBRARY_PATH", ""),
            ("PATH", "/bin"),
        ]
        .map(|(k, v): (&str, &str)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v)));
        assert_eq!(
            dyld_overrides(vars.into_iter()),
            ["DYLD_LIBRARY_PATH", "DYLD_INSERT_LIBRARIES"]
        );
    }

    #[test]
    fn pins_are_sha256_and_in_openh264_sys2s_own_list() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let meta = std::process::Command::new(env!("CARGO"))
            .args(["metadata", "--format-version", "1", "--locked"])
            .args(["--filter-platform", env!("STARFOLD_TARGET")])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("cargo metadata");
        assert!(
            meta.status.success(),
            "{}",
            String::from_utf8_lossy(&meta.stderr)
        );
        let meta: serde_json::Value = serde_json::from_slice(&meta.stdout).unwrap();
        let manifest = meta["packages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "openh264-sys2")
            .and_then(|p| p["manifest_path"].as_str())
            .expect("openh264-sys2 in the dependency graph");
        let list = Path::new(manifest).with_file_name("src/blobs/hashes.txt");
        let list = std::fs::read_to_string(&list).unwrap();
        for (h, name) in PINS {
            assert!(
                h.len() == 64
                    && h.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "{name}"
            );
            assert!(
                list.lines().any(|l| {
                    let mut w = l.split_whitespace();
                    w.next() == Some(*h)
                        && w.next().map(|n| n.trim_start_matches('*')) == Some(*name)
                }),
                "{name} {h} is not in openh264-sys2's list"
            );
        }
    }

    /// With `$STARFOLD_OPENH264_LIB` set (CI fetches Cisco's file), every encoder test runs on
    /// it, and this one says so; set but empty, as when the fetch failed, fails rather than
    /// testing the source build instead.
    #[test]
    fn the_library_tests_run_on() {
        let v = unsafe { library().unwrap().WelsGetCodecVersion() };
        let cisco = std::env::var_os(LIB_ENV).is_some();
        let want = if cisco { (2, 6) } else { (2, 7) };
        assert_eq!((v.uMajor, v.uMinor), want);
    }
}
