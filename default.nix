{
  lib,
  rustPlatform,
  pkg-config,
  nasm,
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
    nasm
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
    libyuv
    libopus
    libpulseaudio
    xorg.libxcb
  ];

  meta = {
    description = "Low-latency X11 desktop streaming server (H.264 over WebSocket) for the Computer preview";
    license = lib.licenses.mit;
    mainProgram = "starfold";
    platforms = ["x86_64-linux"];
  };
}
