//! libaom's AV1 encoder (v3.15.3, BSD-2-Clause): built from the `aom` submodule in its
//! real-time-only configuration without the decoder, and linked statically. Bindings to
//! `aom/aom_encoder.h` and `aom/aomcx.h`.

#![allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code,
    clippy::all
)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// The libaom commit this crate builds (v3.15.3).
pub const COMMIT: &str = "b141d9c45db236335351c8a4b2a14b3ade27c937";

/// libaom's version and the cmake line its build recorded.
pub fn build_info() -> (String, String) {
    let s = |p: *const std::os::raw::c_char| {
        unsafe { std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .into_owned()
    };
    unsafe { (s(aom_codec_version_str()), s(aom_codec_build_config())) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The library linked is the one build.rs made from the submodule: its version, and the
    /// configuration it recorded (real-time only, no decoder).
    #[test]
    fn linked_libaom_is_the_submodule_build() {
        let (version, config) = build_info();
        assert!(version.contains("3.15.3"), "libaom version {version}");
        for flag in ["-DCONFIG_REALTIME_ONLY=1", "-DCONFIG_AV1_DECODER=0"] {
            assert!(config.contains(flag), "{flag} missing from {config}");
        }
    }

    fn init(usage: u32) -> aom_codec_err_t {
        unsafe {
            let iface = aom_codec_av1_cx();
            let mut cfg: aom_codec_enc_cfg_t = std::mem::zeroed();
            let r = aom_codec_enc_config_default(iface, &mut cfg, usage);
            if r != aom_codec_err_t_AOM_CODEC_OK {
                return r;
            }
            cfg.g_w = 64;
            cfg.g_h = 64;
            let mut ctx: aom_codec_ctx_t = std::mem::zeroed();
            let r = aom_codec_enc_init_ver(&mut ctx, iface, &cfg, 0, STARFOLD_AOM_ABI);
            if r == aom_codec_err_t_AOM_CODEC_OK {
                aom_codec_destroy(&mut ctx);
            }
            r
        }
    }

    /// A real-time-only libaom opens in real-time mode and refuses the others.
    #[test]
    fn opens_realtime_only() {
        assert_eq!(init(AOM_USAGE_REALTIME), aom_codec_err_t_AOM_CODEC_OK);
        assert_ne!(init(AOM_USAGE_GOOD_QUALITY), aom_codec_err_t_AOM_CODEC_OK);
    }
}
