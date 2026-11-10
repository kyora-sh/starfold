# Development shell: the native libraries, plus esbuild and TypeScript for the web
# client. `nix-shell --arg withRust true` also brings Rust (rustc, cargo, clippy and
# rustfmt from nixpkgs); otherwise your own toolchain is used. nasm is for
# `--features openh264`.
{
  pkgs ? import <nixpkgs> {},
  withRust ? false,
}:
pkgs.mkShell {
  nativeBuildInputs = with pkgs;
    [pkg-config clang nasm esbuild typescript rustPlatform.bindgenHook]
    ++ lib.optionals withRust [rustc cargo clippy rustfmt];
  buildInputs = with pkgs;
    [x264 libyuv libopus libpulseaudio xorg.libX11 xorg.libXext xorg.libxcb];
}
