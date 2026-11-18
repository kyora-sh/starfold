//! Cisco's OpenH264 binary, as the `openh264-cisco` feature uses it. starfold neither contains
//! nor downloads it.
//!
//! The library never loads it. Only the `starfold` binary does, in `cisco_load.rs`, first thing
//! in its `main`, before starfold starts its logger, its panic hook or any thread, and only on
//! 64-bit Linux, x86-64 and arm64, with glibc. It hands the outcome here through [`install`];
//! the encoders read it from [`api`]. A program that embeds the library gets "available only
//! in the starfold binary" instead, and any other target gets [`NOT_AVAILABLE`]. starfold calls
//! the library only through the entry points installed here, never through openh264-sys2's
//! `DynamicAPI`.

use super::openh264::Api;
use std::ffi::c_int;
use std::path::PathBuf;
use std::sync::OnceLock;

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
];

/// The library's path when `--openh264-lib` isn't given.
pub const LIB_ENV: &str = "STARFOLD_OPENH264_LIB";

/// The text Cisco's binary licence asks software using it to display.
pub const ATTRIBUTION: &str = "OpenH264 Video Codec provided by Cisco Systems, Inc.";

/// Why a build of the feature for any other target has no OpenH264.
pub const NOT_AVAILABLE: &str = "OpenH264 (Cisco) is not available on this platform yet: \
     starfold loads Cisco's binary only on 64-bit Linux, x86-64 and arm64, with glibc";

/// Why a program that embeds starfold's library has no Cisco OpenH264.
pub const BINARY_ONLY: &str = "OpenH264 (Cisco) is available only in the starfold binary: \
     a program that embeds starfold's library does not load it";

/// Cisco's file name for a target architecture starfold loads it on.
fn file_for(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" => Some("libopenh264-2.6.0-linux64.8.so"),
        "aarch64" => Some("libopenh264-2.6.0-linux-arm64.8.so"),
        _ => None,
    }
}

/// Cisco's file name for this target, where starfold loads it.
pub fn platform_file() -> Option<&'static str> {
    file_for(std::env::consts::ARCH).filter(|_| cfg!(openh264_cisco_loadable))
}

pub type Create = unsafe extern "C" fn(*mut *mut openh264_sys2::ISVCEncoder) -> c_int;
pub type Destroy = unsafe extern "C" fn(*mut openh264_sys2::ISVCEncoder);
pub type Version = unsafe extern "C" fn() -> openh264_sys2::OpenH264Version;

/// Cisco's library as the starfold binary loaded it: its entry points, what it is (Cisco's
/// attribution, file and sha256), and where it came from.
#[doc(hidden)]
pub struct Library {
    pub create: Create,
    pub destroy: Destroy,
    pub version: Version,
    pub what: String,
    pub from: String,
}

struct Loaded {
    api: Api,
    what: String,
    from: String,
}

static LIBRARY: OnceLock<Result<Loaded, String>> = OnceLock::new();

/// The starfold binary's hand-over: what its `main` loaded, or why nothing was. The first call
/// counts; any later one, or one after the encoders have looked, changes nothing.
#[doc(hidden)]
pub fn install(loaded: Result<Library, String>) {
    let _ = LIBRARY.set(loaded.map(|l| Loaded {
        api: Api {
            create: l.create,
            destroy: l.destroy,
            version: l.version,
        },
        what: l.what,
        from: l.from,
    }));
}

fn library() -> &'static Result<Loaded, String> {
    LIBRARY.get_or_init(not_installed)
}

/// What was loaded (Cisco's attribution, file and sha256), or why OpenH264 is unavailable.
pub fn status() -> Result<&'static str, String> {
    library()
        .as_ref()
        .map(|l| l.what.as_str())
        .map_err(Clone::clone)
}

/// Where the loaded library came from: its path and the name it was loaded under.
pub fn origin() -> Option<&'static str> {
    library().as_ref().ok().map(|l| l.from.as_str())
}

/// The loaded library's entry points.
pub(crate) fn api() -> Result<Api, String> {
    library().as_ref().map(|l| l.api).map_err(Clone::clone)
}

/// The library's path, from `--openh264-lib` in `args` (without the program name) or from
/// `$STARFOLD_OPENH264_LIB`.
#[doc(hidden)]
pub fn lib_path(args: &[String]) -> Option<PathBuf> {
    crate::parse_args(args.iter().cloned())
        .remove("openh264-lib")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os(LIB_ENV).map(PathBuf::from))
        .filter(|p| !p.as_os_str().is_empty())
}

/// Why OpenH264 is unavailable when no library was given.
#[doc(hidden)]
pub fn no_library() -> String {
    match platform_file() {
        Some(f) => format!(
            "no OpenH264 library: get Cisco's {f} from https://www.openh264.org and pass its \
             path with --openh264-lib or {LIB_ENV}"
        ),
        None => NOT_AVAILABLE.into(),
    }
}

/// Nothing was installed: the library runs without the starfold binary's `main`.
#[cfg(not(test))]
fn not_installed() -> Result<Loaded, String> {
    Err(match platform_file() {
        Some(_) => BINARY_ONLY.into(),
        None => NOT_AVAILABLE.into(),
    })
}

/// The library's tests encode with the dev-dependency's OpenH264, unless
/// `$STARFOLD_OPENH264_LIB` names Cisco's file: then with that, loaded by the binary's own
/// loader (without its one-thread check, as the test harness runs threads).
#[cfg(test)]
fn not_installed() -> Result<Loaded, String> {
    let source = || {
        Ok(Loaded {
            api: super::openh264::SOURCE,
            what: "OpenH264 compiled from source (tests only)".into(),
            from: "the dev-dependency".into(),
        })
    };
    let Some(path) = lib_path(&[]) else {
        return source();
    };
    #[cfg(openh264_cisco_loadable)]
    {
        let kept = cisco_load::open_checked(&path, PINS, platform_file().unwrap(), || {})?.keep();
        Ok(Loaded {
            api: Api {
                create: kept.create,
                destroy: kept.destroy,
                version: kept.version,
            },
            what: format!("{ATTRIBUTION} ({}, sha256 {})", kept.file, kept.sha256),
            from: format!("{}, loaded as {}", path.display(), kept.name),
        })
    }
    #[cfg(not(openh264_cisco_loadable))]
    {
        let _ = path;
        source()
    }
}

#[cfg(all(test, openh264_cisco_loadable))]
#[path = "../cisco_load.rs"]
mod cisco_load;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn pins_are_sha256_and_in_openh264_sys2s_own_list() {
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
        let v = unsafe { (api().unwrap().version)() };
        let cisco = std::env::var_os(LIB_ENV).is_some();
        let want = if cisco { (2, 6) } else { (2, 7) };
        assert_eq!((v.uMajor, v.uMinor), want);
    }

    /// Each architecture starfold loads Cisco's binary on has exactly one pin.
    #[test]
    fn both_architectures_have_their_pin() {
        for arch in ["x86_64", "aarch64"] {
            let f = file_for(arch).unwrap();
            assert_eq!(PINS.iter().filter(|p| p.1 == f).count(), 1, "{arch}: {f}");
        }
        assert_eq!(PINS.len(), 2);
    }

    #[test]
    fn only_64_bit_linux_gnu_x86_64_and_arm64_load_it() {
        assert_eq!(
            platform_file().is_some(),
            cfg!(all(
                target_os = "linux",
                target_env = "gnu",
                target_pointer_width = "64",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))
        );
        assert!(NOT_AVAILABLE.starts_with("OpenH264 (Cisco) is not available on this platform yet"));
        assert!(
            BINARY_ONLY.starts_with("OpenH264 (Cisco) is available only in the starfold binary")
        );
    }

    /// The path comes from `--openh264-lib` in either form, before the environment's.
    #[test]
    fn the_path_is_the_flags_then_the_environments() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            lib_path(&args(&["--openh264-lib=/a/lib.so"])),
            Some(PathBuf::from("/a/lib.so"))
        );
        assert_eq!(
            lib_path(&args(&["--port", "1", "--openh264-lib", "/b/lib.so"])),
            Some(PathBuf::from("/b/lib.so"))
        );
        assert_eq!(lib_path(&args(&["--openh264-lib="])), None);
    }
}
