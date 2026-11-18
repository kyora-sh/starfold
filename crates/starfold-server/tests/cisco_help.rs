//! The starfold binary's `--help` after its own `main` has handed over Cisco's OpenH264, or
//! the reason the load failed: `--crf` names openh264 only after a load that succeeded.
#![cfg(openh264_cisco_loadable)]

use std::process::Command;

const CRF_READER: &str = "openh264's lowest QP in motion;";
const NOT_LOADED: &str =
    "openh264 (OpenH264 (Cisco) is not loaded: no library given, or it did not load)";

/// `--help`'s words, with the library path `lib` in the environment, or none.
fn help(lib: Option<&std::ffi::OsStr>) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_starfold"));
    cmd.arg("--help").env_remove("STARFOLD_OPENH264_LIB");
    if let Some(lib) = lib {
        cmd.env("STARFOLD_OPENH264_LIB", lib);
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// No library, and a file that fails the check: the load fails, `--crf` doesn't name
/// openh264 and `--help` says why. With Cisco's file (CI's step on it sets
/// `$STARFOLD_OPENH264_LIB`), the load succeeds and `--crf` names it.
#[test]
fn crf_names_openh264_only_after_a_successful_load() {
    let none = help(None);
    assert!(!none.contains(CRF_READER), "{none}");
    assert!(none.contains(NOT_LOADED), "{none}");
    let dir = std::env::temp_dir().join(format!("sf-cisco-help-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let lib = dir.join("libopenh264.so");
    std::fs::write(&lib, b"not a library").unwrap();
    let refused = help(Some(lib.as_os_str()));
    assert!(!refused.contains(CRF_READER), "{refused}");
    assert!(refused.contains(NOT_LOADED), "{refused}");
    std::fs::remove_dir_all(&dir).unwrap();
    if let Some(cisco) = std::env::var_os("STARFOLD_OPENH264_LIB") {
        let loaded = help(Some(&cisco));
        assert!(loaded.contains(CRF_READER), "{loaded}");
        assert!(!loaded.contains(NOT_LOADED), "{loaded}");
    }
}
