#!/usr/bin/env bash
# pack.sh <commit> [out dir]: the embeddable client as an npm-style tarball,
# starfold-web-embed-<commit>.tgz. The worker, dav1d and worklet bundles are built with the
# same esbuild flags as the server's own (crates/starfold-server/build.rs); the entry
# (`mount`) is an ES module beside them. The served page's build is not touched.
# Needs esbuild on PATH (or ESBUILD) and bun.
set -euo pipefail
sha=${1:?usage: pack.sh <commit> [out dir]}
dest=$(cd "${2:-.}" && pwd)
cd "$(dirname "$0")/.."
esbuild=${ESBUILD:-esbuild}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
pkg=$work/package
mkdir -p "$pkg"
"$esbuild" worker=src/worker.ts dav1d=src/dav1d/index.ts audio=src/worklets/audio.ts \
	--bundle --format=iife --target=es2022 --minify --log-level=warning --outdir="$pkg"
"$esbuild" index=src/embed/index.ts --bundle --format=esm --target=es2022 --minify \
	--log-level=warning --outdir="$pkg"
bun x tsc -p tsconfig.embed.json --outDir "$work/types"
cp "$work/types/embed/index.d.ts" "$work/types/embed/preamble.d.ts" "$pkg/"
cp dav1d/dav1d.wasm "$pkg/"
cp ../../LICENSE "$pkg/LICENSE"
cp dav1d/COPYING-dav1d dav1d/COPYING-wasi-libc "$pkg/"
cat >"$pkg/package.json" <<EOF
{
	"name": "@starfold/web-embed",
	"version": "0.0.0-${sha}",
	"description": "starfold's browser client, to mount in another page",
	"type": "module",
	"main": "index.js",
	"types": "index.d.ts",
	"license": "MIT AND BSD-2-Clause",
	"sideEffects": false
}
EOF
tar -czf "$dest/starfold-web-embed-${sha}.tgz" -C "$work" package
echo "$dest/starfold-web-embed-${sha}.tgz"
