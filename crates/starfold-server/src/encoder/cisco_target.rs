/// Whether starfold loads Cisco's OpenH264 binary on a target with these `cfg` values
/// (`target_os`, `target_env`, `target_pointer_width`, `target_arch`, `target_endian`): Linux
/// with glibc, 64-bit, little-endian, x86-64 or arm64, the two builds starfold pins.
pub fn cisco_loadable(os: &str, env: &str, width: &str, arch: &str, endian: &str) -> bool {
    os == "linux"
        && env == "gnu"
        && width == "64"
        && endian == "little"
        && matches!(arch, "x86_64" | "aarch64")
}
