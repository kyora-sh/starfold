//! What this binary was built with, from its cargo features. Every user-visible statement
//! about its software encoders and its licence (`--help`, `--version`, the startup log, the
//! unknown-encoder error) comes from here, so none of them names an encoder it doesn't have.

use crate::encoder::SOFTWARE_H264;

/// The software H.264 encoders this binary includes, by `--encoder` name.
pub fn software_encoders() -> Vec<&'static str> {
    let mut v = Vec::new();
    if cfg!(feature = "x264") {
        v.push("x264");
    }
    if cfg!(feature = "openh264") {
        v.push("openh264");
    }
    v
}

/// The software H.264 encoder `--encoder auto` falls back to where there is no hardware one.
pub fn software_h264() -> &'static str {
    SOFTWARE_H264.name()
}

pub fn has_x264() -> bool {
    cfg!(feature = "x264")
}

pub fn has_openh264() -> bool {
    cfg!(feature = "openh264")
}

/// The licence of this binary: linking x264 (or the bench's x265) makes it GPL-2.0-or-later as a
/// combined work.
pub fn licence() -> &'static str {
    if cfg!(any(feature = "x264", feature = "x265")) {
        "GPL-2.0-or-later"
    } else {
        "MIT"
    }
}

/// The startup warning for an x264-only flag given while `encoder` encodes H.264.
pub fn x264_flag_ignored(flag: &str, encoder: &str) -> String {
    if has_x264() {
        format!("--{flag} is an x264 setting; {encoder} ignores it")
    } else {
        format!("--{flag} has no effect: {encoder} ignores it")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_follow_the_features() {
        let v = software_encoders();
        assert_eq!(v.contains(&"x264"), cfg!(feature = "x264"));
        assert_eq!(v.contains(&"openh264"), cfg!(feature = "openh264"));
        assert!(v.contains(&software_h264()));
        let gpl = cfg!(any(feature = "x264", feature = "x265"));
        assert_eq!(licence() == "GPL-2.0-or-later", gpl);
        assert_eq!(licence() == "MIT", !gpl);
    }

    #[test]
    fn the_ignored_flag_warning_names_x264_only_in_a_build_with_it() {
        let w = x264_flag_ignored("preset", "videotoolbox");
        assert!(w.starts_with("--preset "), "{w}");
        assert_eq!(w.contains("x264"), cfg!(feature = "x264"), "{w}");
    }
}
