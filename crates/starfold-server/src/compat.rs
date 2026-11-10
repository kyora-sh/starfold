//! The voxl names starfold still answers to. Each old environment variable, request
//! header and microphone FIFO is used when its starfold name is absent, and logs one
//! warning per name for the process's life. Responses carry both header names.

use std::collections::HashSet;
use std::sync::Mutex;

use axum::http::{HeaderMap, HeaderName, HeaderValue};

/// Environment variables: (starfold name, deprecated voxl name).
pub const ENV: &[(&str, &str)] = &[
    ("STARFOLD_PASSWORD", "VOXL_PASSWORD"),
    ("STARFOLD_TEST_HOOKS", "VOXL_TEST_HOOKS"),
    ("STARFOLD_DEBUG_DAMAGE", "VOXL_DEBUG_DAMAGE"),
    ("STARFOLD_DEBUG_CORNERS", "VOXL_DEBUG_CORNERS"),
    ("STARFOLD_DEBUG_MARKER", "VOXL_DEBUG_MARKER"),
    ("STARFOLD_DEBUG_STRIPES", "VOXL_DEBUG_STRIPES"),
    ("STARFOLD_NO_DIRECT_FB", "VOXL_NO_DIRECT_FB"),
    ("STARFOLD_NO_MBINFO", "VOXL_NO_MBINFO"),
    ("STARFOLD_X264_INFO", "VOXL_X264_INFO"),
];

/// Request headers: (starfold name, deprecated voxl name), lowercase.
pub const HEADERS: &[(&str, &str)] = &[("x-starfold-upload", "x-voxl-upload")];

/// Response headers, sent under both names: (starfold name, deprecated voxl name).
pub const RESPONSE_HEADERS: &[(&str, &str)] = &[
    ("x-starfold-seq", "x-voxl-seq"),
    ("x-starfold-segments", "x-voxl-segments"),
    ("x-starfold-record-end", "x-voxl-record-end"),
];

/// The default microphone FIFO and the voxl one it falls back to.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const MIC_FIFO: (&str, &str) = ("/tmp/starfold-mic.fifo", "/tmp/voxl-mic.fifo");

static WARNED: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);

fn old_name(table: &[(&'static str, &'static str)], new: &str) -> &'static str {
    table
        .iter()
        .find(|(n, _)| *n == new)
        .map(|(_, o)| *o)
        .unwrap_or_else(|| panic!("{new} has no entry in compat's table"))
}

/// The value `new` resolves to with `get` as the source, and the warning that calls for.
fn resolve<T>(
    new: &str,
    old: &str,
    get: impl Fn(&str) -> Option<T>,
) -> (Option<T>, Option<String>) {
    match (get(new), get(old)) {
        (Some(v), Some(_)) => (
            Some(v),
            Some(format!("{old} is ignored: {new} is set (drop {old})")),
        ),
        (Some(v), None) => (Some(v), None),
        (None, Some(v)) => (Some(v), Some(format!("{old} is deprecated: use {new}"))),
        (None, None) => (None, None),
    }
}

/// True the first time `key` is seen in this process.
fn first(key: &'static str) -> bool {
    WARNED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashSet::new)
        .insert(key)
}

/// Logs `warning` the first time `key` comes up.
pub fn warn_once(key: &'static str, warning: &str) {
    if first(key) {
        log::warn!("{warning}");
    }
}

fn report<T>(old: &'static str, (v, warning): (Option<T>, Option<String>)) -> Option<T> {
    if let Some(w) = warning {
        warn_once(old, &w);
    }
    v
}

/// The environment variable `new` (named in ENV), or its voxl name with a warning.
pub fn env(new: &str) -> Option<String> {
    let old = old_name(ENV, new);
    report(old, resolve(new, old, |n| std::env::var(n).ok()))
}

/// Whether the environment variable `new` (named in ENV), or its voxl name, is set.
pub fn is_set(new: &str) -> bool {
    let old = old_name(ENV, new);
    report(old, resolve(new, old, |n| std::env::var_os(n).map(|_| ()))).is_some()
}

/// Whether the request carries header `new` (named in HEADERS), or its voxl name.
pub fn has_header(headers: &HeaderMap, new: &str) -> bool {
    let old = old_name(HEADERS, new);
    report(old, resolve(new, old, |n| headers.get(n).map(|_| ()))).is_some()
}

/// Sets response header `new` (named in RESPONSE_HEADERS) and its voxl name.
pub fn insert_header(headers: &mut HeaderMap, new: &'static str, value: HeaderValue) {
    let old = old_name(RESPONSE_HEADERS, new);
    headers.insert(HeaderName::from_static(old), value.clone());
    headers.insert(HeaderName::from_static(new), value);
}

/// The FIFOs to write the microphone to, in order: the one --microphone-fifo names, or
/// the starfold default and then the voxl one.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn mic_fifos(given: Option<&str>) -> Vec<String> {
    match given {
        Some(p) => vec![p.to_string()],
        None => vec![MIC_FIFO.0.to_string(), MIC_FIFO.1.to_string()],
    }
}

/// The warning for writing the microphone to `path`, if it is the voxl FIFO.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn fifo_warning(path: &str) -> Option<String> {
    (path == MIC_FIFO.1).then(|| {
        format!(
            "{} is deprecated: point PulseAudio's pipe source at {}",
            MIC_FIFO.1, MIC_FIFO.0
        )
    })
}

/// Warns once when the microphone ends up on the voxl FIFO.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn opened_fifo(path: &str) {
    if let Some(w) = fifo_warning(path) {
        warn_once(MIC_FIFO.1, &w);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn src(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn check(table: &[(&'static str, &'static str)]) {
        for &(new, old) in table {
            let only_old = src(&[(old, "o")]);
            let (v, w) = resolve(new, old, |n| only_old.get(n).cloned());
            assert_eq!(v.as_deref(), Some("o"), "{old} alone works");
            assert_eq!(
                w.as_deref(),
                Some(format!("{old} is deprecated: use {new}").as_str()),
                "{old} alone warns"
            );
            let only_new = src(&[(new, "n")]);
            assert_eq!(
                resolve(new, old, |n| only_new.get(n).cloned()),
                (Some("n".into()), None),
                "{new} alone works without a warning"
            );
            let both = src(&[(new, "n"), (old, "o")]);
            let (v, w) = resolve(new, old, |n| both.get(n).cloned());
            assert_eq!(v.as_deref(), Some("n"), "{new} wins over {old}");
            assert_eq!(
                w.as_deref(),
                Some(format!("{old} is ignored: {new} is set (drop {old})").as_str()),
                "both set warns that {old} is ignored"
            );
            assert_eq!(resolve::<String>(new, old, |_| None), (None, None));
            assert!(new.to_ascii_lowercase().contains("starfold"));
            assert!(old.to_ascii_lowercase().contains("voxl"));
            assert_eq!(old_name(table, new), old);
        }
    }

    #[test]
    fn every_env_alias_works_warns_and_loses_to_its_new_name() {
        check(ENV);
    }

    #[test]
    fn every_request_header_alias_works_warns_and_loses_to_its_new_name() {
        check(HEADERS);
        for &(new, old) in HEADERS {
            let mut h = HeaderMap::new();
            h.insert(HeaderName::from_static(old), HeaderValue::from_static("1"));
            assert!(has_header(&h, new), "{old} alone is accepted");
            let mut h = HeaderMap::new();
            h.insert(HeaderName::from_static(new), HeaderValue::from_static("1"));
            assert!(has_header(&h, new), "{new} is accepted");
            assert!(!has_header(&HeaderMap::new(), new));
        }
    }

    #[test]
    fn every_response_header_goes_out_under_both_names() {
        check(RESPONSE_HEADERS);
        for &(new, old) in RESPONSE_HEADERS {
            let mut h = HeaderMap::new();
            insert_header(&mut h, new, HeaderValue::from(7));
            assert_eq!(h[new], "7");
            assert_eq!(h[old], "7");
        }
    }

    #[test]
    fn the_microphone_tries_the_starfold_fifo_then_the_voxl_one_unless_one_is_named() {
        assert_eq!(
            mic_fifos(None),
            ["/tmp/starfold-mic.fifo", "/tmp/voxl-mic.fifo"]
        );
        assert_eq!(mic_fifos(Some("/run/mic")), ["/run/mic"]);
        assert_eq!(
            mic_fifos(Some("/tmp/voxl-mic.fifo")),
            ["/tmp/voxl-mic.fifo"]
        );
        assert!(fifo_warning("/tmp/voxl-mic.fifo")
            .is_some_and(|w| w.contains("deprecated") && w.contains("/tmp/starfold-mic.fifo")));
        assert_eq!(fifo_warning("/tmp/starfold-mic.fifo"), None);
        assert_eq!(fifo_warning("/run/mic"), None);
    }

    #[test]
    fn a_warning_is_logged_once_per_name() {
        assert!(first("compat-test-name"));
        assert!(!first("compat-test-name"));
    }

    #[test]
    #[should_panic(expected = "no entry")]
    fn a_name_missing_from_the_table_is_a_bug() {
        env("STARFOLD_NOT_A_NAME");
    }
}
