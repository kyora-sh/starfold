{
  lib,
  rustPlatform,
  pkg-config,
  x264,
  libyuv,
  libopus,
  libpulseaudio,
  xorg,
  esbuild,
  typescript,
}:
rustPlatform.buildRustPackage {
  pname = "starfold";
  version = "0.3.2";

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./crates
      ./packages/web
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    rustPlatform.bindgenHook
    esbuild
    typescript
  ];

  # build.rs bundles packages/web/src with esbuild; type-check it first.
  preBuild = ''
    tsc -p packages/web
    tsc -p packages/web/tsconfig.worker.json
    tsc -p packages/web/tsconfig.worklet.json
  '';

  buildInputs = [
    x264
    libyuv
    libopus
    libpulseaudio
    xorg.libxcb
  ];

  meta = {
    description = "Low-latency X11 desktop streaming server (H.264 over WebSocket) for the Computer preview";
    # MIT source; the binary links x264, so it is GPL-2.0-or-later as a combined work.
    license = with lib.licenses; [mit gpl2Plus];
    mainProgram = "starfold";
    platforms = ["x86_64-linux"];
  };
}
