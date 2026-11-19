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
    /// Whether a given `--paintover-crf` is also the finest QP it refines a still picture to.
    pub refines_to_paintover_crf: bool,
    /// Reads `--crf` and `--paintover-crf` only with an uncapped bitrate (`--max-kbps 0`), as
    /// its constant QP; a capped one runs CBR and ignores them.
    pub uncapped_only: bool,
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
        refines_to_paintover_crf: true,
        uncapped_only: false,
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
        refines_to_paintover_crf: false,
        uncapped_only: false,
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
        refines_to_paintover_crf: false,
        uncapped_only: false,
        gpl: false,
    },
    Encoder {
        name: "libaom",
        built: cfg!(feature = "libaom"),
        hardware: false,
        h264: false,
        crf: Some("lowest quantizer"),
        preset: Some("a speed, 9 by default"),
        paintover_crf: false,
        refines_to_paintover_crf: false,
        uncapped_only: false,
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
        refines_to_paintover_crf: false,
        uncapped_only: false,
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
        refines_to_paintover_crf: false,
        uncapped_only: false,
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
        refines_to_paintover_crf: false,
        uncapped_only: false,
        gpl: true,
    },
    hardware("videotoolbox", cfg!(target_os = "macos"), Qp::Never),
    hardware("vaapi", cfg!(target_os = "linux"), Qp::Negotiated),
    hardware(
        "nvenc",
        cfg!(any(target_os = "linux", windows)),
        Qp::Uncapped,
    ),
    hardware("mediafoundation", cfg!(windows), Qp::Uncapped),
    hardware(
        "qsv",
        cfg!(all(windows, target_arch = "x86_64")),
        Qp::Uncapped,
    ),
];

/// When a hardware encoder codes at a constant QP: then `--crf` is that QP, and
/// `--paintover-crf` while it sharpens; under a bitrate (CBR, VBR) it ignores both.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Qp {
    /// It sets a bitrate only.
    Never,
    /// With an uncapped bitrate (`--max-kbps 0`), and only then: a cap is always CBR.
    Uncapped,
    /// When the driver's rate control comes out at constant QP: the first choice with
    /// `--max-kbps 0`, and also with a cap on a driver that has no CBR or VBR (VA-API).
    Negotiated,
}

/// A hardware H.264 encoder built on this platform when `built`. None takes a preset or links
/// anything; what `--crf` and `--paintover-crf` are to it follows `qp`.
const fn hardware(name: &'static str, built: bool, qp: Qp) -> Encoder {
    Encoder {
        name,
        built,
        hardware: true,
        h264: true,
        crf: match qp {
            Qp::Never => None,
            Qp::Uncapped => Some("constant QP with --max-kbps 0"),
            Qp::Negotiated => Some(
                "constant QP whenever the driver codes at one (with --max-kbps 0, or a cap on \
                 a driver without CBR or VBR)",
            ),
        },
        preset: None,
        paintover_crf: !matches!(qp, Qp::Never),
        refines_to_paintover_crf: false,
        uncapped_only: matches!(qp, Qp::Uncapped),
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
        "paintover-crf" => e
            .paintover_crf
            .then_some(match (e.hardware, e.uncapped_only) {
                (true, true) => "paint-over QP with --max-kbps 0",
                (true, false) => "paint-over QP whenever the driver codes at a constant QP",
                (false, _) => "paint-over quality",
            }),
        _ => None,
    }
}

/// Whether a given `--paintover-crf` is the finest QP the encoder named `encoder` refines to.
pub fn refines_to_paintover_crf(encoder: &str) -> bool {
    ENCODERS
        .iter()
        .any(|e| e.name == encoder && e.refines_to_paintover_crf)
}

/// This build's encoders a given `--paintover-crf` sets the finest refinement QP of.
pub fn refining_to_paintover_crf() -> Vec<&'static str> {
    built()
        .filter(|e| e.refines_to_paintover_crf)
        .map(|e| e.name)
        .collect()
}

/// Whether the encoder named `encoder` reads `flag` with the bitrate uncapped (`--max-kbps 0`)
/// or not.
pub fn reads(encoder: &str, flag: &str, uncapped: bool) -> bool {
    ENCODERS
        .iter()
        .any(|e| e.name == encoder && meaning(e, flag).is_some() && (uncapped || !e.uncapped_only))
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
    if reads(encoder, flag, true) {
        format!("--{flag} reaches {encoder} only with --max-kbps 0; with a capped bitrate it ignores it")
    } else if readers.is_empty() {
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
            assert!(e.h264 && !e.gpl && e.preset.is_none(), "{}", e.name);
        }
        // NVENC, Media Foundation and Quick Sync run CQP exactly when uncapped (a cap is
        // always CBR): --crf, and --paintover-crf while sharpening (set_rate takes its
        // quality), only then.
        for name in ["nvenc", "mediafoundation", "qsv"] {
            for flag in ["crf", "paintover-crf"] {
                assert!(reads(name, flag, true), "{name} {flag} uncapped");
                assert!(!reads(name, flag, false), "{name} {flag} capped");
            }
            let w = flag_ignored("crf", name);
            assert!(w.contains("only with --max-kbps 0"), "{w}");
        }
        // VA-API takes the driver's first mode of CQP, VBR, CBR uncapped and of CBR, VBR,
        // CQP capped, so a capped stream can still run CQP: no cap rules the flags out.
        let vaapi = ENCODERS.iter().find(|e| e.name == "vaapi").unwrap();
        for flag in ["crf", "paintover-crf"] {
            assert!(reads("vaapi", flag, true) && reads("vaapi", flag, false));
            let what = meaning(vaapi, flag).unwrap();
            assert!(what.contains("whenever the driver codes at"), "{what}");
        }
        for name in ["vaapi", "nvenc", "mediafoundation", "qsv", "videotoolbox"] {
            assert!(!reads(name, "preset", true) && !reads(name, "preset", false));
        }
        assert!(FLAGS.iter().all(|f| !reads("videotoolbox", f, true)));
        let hw = hardware_encoders();
        assert_eq!(hw.contains(&"videotoolbox"), cfg!(target_os = "macos"));
        assert_eq!(hw.contains(&"vaapi"), cfg!(target_os = "linux"));
        assert_eq!(
            hw.contains(&"nvenc"),
            cfg!(any(target_os = "linux", windows))
        );
        assert_eq!(hw.contains(&"mediafoundation"), cfg!(windows));
        assert_eq!(
            hw.contains(&"qsv"),
            cfg!(all(windows, target_arch = "x86_64"))
        );
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
