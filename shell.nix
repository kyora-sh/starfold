# Development shell: the native libraries, plus esbuild and TypeScript for the web
# client. `nix-shell --arg withRust true` also brings Rust (rustc, cargo, clippy and
# rustfmt from nixpkgs); otherwise your own toolchain is used. `--arg withX264 true`
# adds x264 for `cargo build --features x264`.
{
  pkgs ? import <nixpkgs> {},
  withRust ? false,
  withX264 ? false,
}:
pkgs.mkShell {
  nativeBuildInputs = with pkgs;
    [pkg-config clang nasm esbuild typescript]
    ++ lib.optionals withX264 [rustPlatform.bindgenHook]
    ++ lib.optionals withRust [rustc cargo clippy rustfmt];
  buildInputs = with pkgs;
    [libyuv libopus libpulseaudio xorg.libX11 xorg.libXext xorg.libxcb]
    ++ lib.optionals withX264 [x264];
}
