//! Loads Windows DLLs at runtime without the default search order, which looks in the
//! executable's directory and possibly the current directory before System32. A DLL
//! planted there would otherwise run in the server's process.

#[cfg(windows)]
use libloading::os::windows::{
    Library as WinLibrary, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
#[cfg(windows)]
use libloading::Library;

/// A system DLL by file name, from System32 only.
#[cfg(windows)]
pub unsafe fn system(name: &str) -> Result<Library, String> {
    check_bare_name(name)?;
    WinLibrary::load_with_flags(name, LOAD_LIBRARY_SEARCH_SYSTEM32)
        .map(Library::from)
        .map_err(|e| format!("{name}: {e}"))
}

/// A driver runtime at an absolute path (e.g. in the DriverStore). Its own imports
/// resolve from its directory and System32.
#[cfg(windows)]
#[allow(dead_code, reason = "for driver runtimes outside System32, none loaded yet")]
pub unsafe fn absolute(path: &str) -> Result<Library, String> {
    check_absolute(path)?;
    WinLibrary::load_with_flags(
        path,
        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
    )
    .map(Library::from)
    .map_err(|e| format!("{path}: {e}"))
}

fn check_bare_name(name: &str) -> Result<(), String> {
    let bare = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', ':', '\0']);
    if bare {
        Ok(())
    } else {
        Err(format!("{name:?}: not a bare DLL file name"))
    }
}

/// `C:\...` (either separator) or a UNC / `\\?\` path. Drive-relative (`C:x`) and
/// root-relative (`\x`) paths depend on the current drive or directory, so they fail.
fn check_absolute(path: &str) -> Result<(), String> {
    let b = path.as_bytes();
    let sep = |c: u8| c == b'\\' || c == b'/';
    let drive = b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && sep(b[2]);
    let unc = b.len() > 2 && sep(b[0]) && sep(b[1]);
    if (drive || unc) && !path.contains('\0') {
        Ok(())
    } else {
        Err(format!("{path:?}: not an absolute DLL path"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_names_pass() {
        for name in ["mfplat.dll", "nvEncodeAPI64.dll", "nvcuda.dll", "x.dll"] {
            assert_eq!(check_bare_name(name), Ok(()), "{name}");
        }
    }

    #[test]
    fn names_with_a_path_fail() {
        for name in [
            "",
            ".",
            "..",
            "dir\\mfplat.dll",
            "dir/mfplat.dll",
            ".\\mfplat.dll",
            "..\\mfplat.dll",
            "C:mfplat.dll",
            "C:\\Windows\\System32\\mfplat.dll",
            "\\\\host\\share\\mfplat.dll",
            "mfplat.dll:stream",
            "mfplat\0.dll",
        ] {
            let err = check_bare_name(name).unwrap_err();
            assert!(err.contains("not a bare DLL file name"), "{name:?}: {err}");
        }
    }

    #[test]
    fn absolute_paths_pass() {
        for path in [
            "C:\\Windows\\System32\\DriverStore\\FileRepository\\x\\libmfx64-gen.dll",
            "c:/drivers/x.dll",
            "\\\\?\\C:\\drivers\\x.dll",
            "\\\\host\\share\\x.dll",
        ] {
            assert_eq!(check_absolute(path), Ok(()), "{path}");
        }
    }

    #[test]
    fn relative_paths_fail() {
        for path in [
            "",
            "x.dll",
            "drivers\\x.dll",
            ".\\x.dll",
            "..\\x.dll",
            "C:x.dll",
            "C:",
            "C:\\",
            "\\drivers\\x.dll",
            "/drivers/x.dll",
            "1:\\x.dll",
            "C:\\drivers\\x\0.dll",
        ] {
            let err = check_absolute(path).unwrap_err();
            assert!(err.contains("not an absolute DLL path"), "{path:?}: {err}");
        }
    }
}
