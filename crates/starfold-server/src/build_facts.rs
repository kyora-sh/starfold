//! What this binary was built with, from its cargo features. Every user-visible statement
//! about its software encoders, the flags they read and its licence (`--help`, `--version`,
//! the startup warnings, the unknown-encoder error) comes from here, so none of them names an
//! encoder the binary doesn't have.

use crate::encoder::SOFTWARE_H264;

/// One software encoder feature of Cargo.toml, or one hardware encoder: whether this build has
/// it, and what it makes of the flags the encoders share.
pub struct Encoder {
    /// The cargo feature, which is also the backend's name; a hardware encoder's backend name.
    pub name: &'static str,
    /// A software encoder's feature is on; a hardware encoder is built on this platform.
    pub built: bool,
    /// A hardware encoder: no cargo feature, found by its probe at startup.
    pub hardware: bool,
    /// An H.264 encoder `--encoder` can pin and `auto` can fall back to; else a bench arm.
    pub h264: bool,
    /// What `--crf` is to it.
    pub crf: Option<&'static str>,
    /// What `--preset` is to it.
    pub preset: Option<&'static str>,
    /// Whether it takes the quality again on the fly, so `--paintover-crf` reaches it.
    pub paintover_crf: bool,
    /// GPL: a binary that links it is GPL-2.0-or-later as a combined work.
    pub gpl: bool,
}

pub const ENCODERS: &[Encoder] = &[
    Encoder {
        name: "x264",
        built: cfg!(feature = "x264"),
        hardware: false,
        h264: true,
        crf: Some("base quality"),
        preset: Some("a preset name, ultrafast by default"),
        paintover_crf: true,
        gpl: true,
    },
    Encoder {
        name: "openh264",
        built: cfg!(feature = "openh264"),
        hardware: false,
        h264: true,
        crf: Some("lowest QP in motion"),
        preset: None,
        paintover_crf: false,
        gpl: false,
    },
    Encoder {
        name: "libvpx",
        built: cfg!(feature = "libvpx"),
        hardware: false,
        h264: false,
        crf: Some("lowest quantizer"),
        preset: Some("a speed, 8 by default"),
        paintover_crf: true,
        gpl: false,
    },
    Encoder {
        name: "libaom",
        built: cfg!(feature = "libaom"),
        hardware: false,
        h264: false,
        crf: Some("lowest quantizer"),
        preset: Some("a speed, 10 by default"),
        paintover_crf: true,
        gpl: false,
    },
    Encoder {
        name: "svt-av1",
        built: cfg!(feature = "svt-av1"),
        hardware: false,
        h264: false,
        crf: Some("lowest QP"),
        preset: Some("a preset number, 12 by default"),
        paintover_crf: false,
        gpl: false,
    },
    Encoder {
        name: "kvazaar",
        built: cfg!(feature = "kvazaar"),
        hardware: false,
        h264: false,
        crf: None,
        preset: Some("ultrafast (default), superfast or veryfast"),
        paintover_crf: false,
        gpl: false,
    },
    Encoder {
        name: "x265",
        built: cfg!(feature = "x265"),
        hardware: false,
        h264: false,
        crf: Some("lowest QP"),
        preset: Some("ultrafast (default), superfast or veryfast"),
        paintover_crf: false,
        gpl: true,
    },
    hardware("videotoolbox", cfg!(target_os = "macos")),
    hardware("vaapi", cfg!(target_os = "linux")),
    hardware("nvenc", cfg!(any(target_os = "linux", windows))),
    hardware("mediafoundation", cfg!(windows)),
    hardware("qsv", cfg!(windows)),
];

/// A hardware H.264 encoder built on this platform when `built`; it reads none of the shared
/// flags and links nothing.
const fn hardware(name: &'static str, built: bool) -> Encoder {
    Encoder {
        name,
        built,
        hardware: true,
        h264: true,
        crf: None,
        preset: None,
        paintover_crf: false,
        gpl: false,
    }
}

/// The flags whose meaning depends on the encoder.
pub const FLAGS: [&str; 3] = ["preset", "crf", "paintover-crf"];

fn built() -> impl Iterator<Item = &'static Encoder> {
    ENCODERS.iter().filter(|e| e.built)
}

fn software() -> impl Iterator<Item = &'static Encoder> {
    built().filter(|e| !e.hardware)
}

/// Every software encoder this binary includes, H.264 and bench arms, by feature name.
pub fn software_encoders() -> Vec<&'static str> {
    software().map(|e| e.name).collect()
}

/// The software H.264 encoders `--encoder` can pin.
pub fn h264_encoders() -> Vec<&'static str> {
    software().filter(|e| e.h264).map(|e| e.name).collect()
}

/// The bench arms `--encoder` can name.
pub fn bench_arms() -> Vec<&'static str> {
    #[cfg(feature = "sw-arm")]
    {
        crate::encoder::ARMS.iter().map(|a| a.0).collect()
    }
    #[cfg(not(feature = "sw-arm"))]
    {
        Vec::new()
    }
}

/// The software names `--encoder` takes: the H.264 encoders, then the bench arms.
pub fn encoder_names() -> Vec<&'static str> {
    [h264_encoders(), bench_arms()].concat()
}

/// The hardware encoders `--encoder` can name on this platform; each still needs its probe to
/// find a device.
pub fn hardware_encoders() -> Vec<&'static str> {
    built().filter(|e| e.hardware).map(|e| e.name).collect()
}

/// The software H.264 encoder `--encoder auto` falls back to where there is no hardware one.
pub fn software_h264() -> &'static str {
    SOFTWARE_H264.name()
}

fn meaning(e: &Encoder, flag: &str) -> Option<&'static str> {
    match flag {
        "preset" => e.preset,
        "crf" => e.crf,
        "paintover-crf" => e.paintover_crf.then_some("paint-over quality"),
        _ => None,
    }
}

/// Whether the encoder named `encoder` reads `flag`; a hardware encoder reads none of them.
pub fn reads(encoder: &str, flag: &str) -> bool {
    ENCODERS
        .iter()
        .any(|e| e.name == encoder && meaning(e, flag).is_some())
}

/// This build's encoders that read `flag`, each with what the flag is to it.
pub fn readers(flag: &str) -> Vec<(&'static str, &'static str)> {
    built()
        .filter_map(|e| meaning(e, flag).map(|m| (e.name, m)))
        .collect()
}

/// The licence of this binary: linking a GPL encoder makes it GPL-2.0-or-later as a combined
/// work; otherwise it stays under starfold's MIT licence.
pub fn licence() -> &'static str {
    if built().any(|e| e.gpl) {
        "GPL-2.0-or-later"
    } else {
        "MIT"
    }
}

/// The startup warning for `--flag` given while `encoder`, which ignores it, encodes H.264.
pub fn flag_ignored(flag: &str, encoder: &str) -> String {
    let readers: Vec<&str> = readers(flag).into_iter().map(|r| r.0).collect();
    if readers.is_empty() {
        format!("--{flag} has no effect in this build: {encoder} ignores it")
    } else {
        format!(
            "--{flag} is for {}; {encoder} ignores it",
            readers.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoder features Cargo.toml declares: every feature but `default` and `sw-arm`.
    fn cargo_features() -> Vec<String> {
        let toml = include_str!("../Cargo.toml");
        let section = toml.split("[features]").nth(1).expect("[features]");
        let section = section.split("\n[").next().unwrap_or(section);
        section
            .lines()
            .filter_map(|l| l.split_once('=').map(|(k, _)| k.trim().to_string()))
            .filter(|k| !k.starts_with('#') && k != "default" && k != "sw-arm")
            .collect()
    }

    #[test]
    fn every_encoder_feature_in_cargo_toml_is_described() {
        let mut features = cargo_features();
        features.sort();
        let mut described: Vec<String> = ENCODERS
            .iter()
            .filter(|e| !e.hardware)
            .map(|e| e.name.to_string())
            .collect();
        described.sort();
        assert_eq!(features, described);
    }

    #[test]
    fn facts_follow_the_features() {
        assert!(h264_encoders().contains(&software_h264()));
        assert_eq!(
            licence() == "GPL-2.0-or-later",
            cfg!(any(feature = "x264", feature = "x265"))
        );
        for arm in bench_arms() {
            assert!(
                software_encoders().iter().any(|e| arm.starts_with(e)),
                "{arm}"
            );
        }
    }

    #[test]
    fn hardware_rows_are_the_platforms_hardware_backends() {
        use crate::encoder::Backend;
        for e in ENCODERS.iter().filter(|e| e.hardware) {
            let parsed = Backend::parse(e.name);
            assert_eq!(parsed.is_ok(), e.built, "{}", e.name);
            assert!(parsed.is_ok_and(|b| b.name() == e.name) || !e.built);
            assert!(e.h264 && !e.gpl && FLAGS.iter().all(|f| !reads(e.name, f)));
        }
        let hw = hardware_encoders();
        assert_eq!(hw.contains(&"videotoolbox"), cfg!(target_os = "macos"));
        assert_eq!(hw.contains(&"vaapi"), cfg!(target_os = "linux"));
        assert_eq!(
            hw.contains(&"nvenc"),
            cfg!(any(target_os = "linux", windows))
        );
        assert_eq!(hw.contains(&"mediafoundation"), cfg!(windows));
        assert_eq!(hw.contains(&"qsv"), cfg!(windows));
        let e = Backend::parse("bogus").unwrap_err();
        for name in hw {
            assert!(e.contains(name), "{e}");
        }
    }

    #[test]
    fn the_ignored_flag_warning_names_only_encoders_built_in() {
        for flag in FLAGS {
            let w = flag_ignored(flag, "videotoolbox");
            assert!(w.starts_with(&format!("--{flag} ")), "{w}");
            for e in ENCODERS.iter().filter(|e| !e.hardware) {
                assert!(!w.contains(e.name) || e.built, "{w}");
            }
        }
    }
}
