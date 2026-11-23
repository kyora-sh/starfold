#!/usr/bin/env bash
# stage-crate-web.sh: puts the web client a repository build embeds into crates/starfold-server/web,
# which the published crate carries in place of packages/web (build.rs reads it when there is no
# packages/web). The files are the ones build.rs gathers in OUT_DIR/web, taken from a `cargo check`
# of this checkout (esbuild on PATH, or ESBUILD), so a build from the crate embeds the same bytes
# as one from the repository; the licences of dav1d, wasi-libc and Inter join them.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
crate=$root/crates/starfold-server
messages=$(mktemp)
trap 'rm -f "$messages"' EXIT
cargo check --locked --manifest-path "$crate/Cargo.toml" --message-format=json >"$messages"
out_dir=$(python3 -I -c '
import json, sys
dirs = set()
for line in open(sys.argv[1], encoding="utf-8"):
    if not line.startswith("{"):
        continue
    m = json.loads(line)
    # A path package id: path+file:///.../crates/starfold-server#starfold@0.3.2
    if m.get("reason") == "build-script-executed" and m["package_id"].split("#")[0].endswith("/crates/starfold-server"):
        dirs.add(m["out_dir"])
if len(dirs) != 1:
    sys.exit(f"expected one crates/starfold-server build script run, found {sorted(dirs)}")
print(dirs.pop())
' "$messages")
rm -rf "$crate/web"
mkdir "$crate/web"
cp "$out_dir"/web/* "$crate/web/"
cp "$root/packages/web/inter-LICENSE.txt" "$root/packages/web/dav1d/COPYING-dav1d" \
	"$root/packages/web/dav1d/COPYING-wasi-libc" "$crate/web/"
for f in index.html favicon.ico inter.woff2; do
	cmp "$root/packages/web/$f" "$crate/web/$f"
done
cmp "$root/packages/web/dav1d/dav1d.wasm" "$crate/web/dav1d.wasm"
if command -v sha256sum >/dev/null; then
	(cd "$crate/web" && sha256sum -- *)
else
	(cd "$crate/web" && shasum -a 256 -- *)
fi
