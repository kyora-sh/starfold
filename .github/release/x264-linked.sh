#!/usr/bin/env bash
# x264-linked.sh <cargo JSON messages> <libx264.a>: fails unless the starfold-server build those
# messages describe linked x264 from exactly <libx264.a>, as build.rs records it in
# OUT_DIR/x264-linked when STARFOLD_X264_LIB is set (no record, or another archive, fails).
set -euo pipefail
messages=${1:?usage: x264-linked.sh <cargo JSON messages> <libx264.a>}
want=$(cd "$(dirname "$2")" && pwd -P)/$(basename "$2")
out_dir=$(python3 -I -c '
import json, sys
dirs = set()
for line in open(sys.argv[1], encoding="utf-8"):
    if not line.startswith("{"):
        continue
    m = json.loads(line)
    # A path package id: path+file:///.../crates/starfold-server#starfold@0.3.2
    if m.get("reason") == "build-script-executed" and m["package_id"].split("#")[0].endswith("/starfold-server"):
        dirs.add(m["out_dir"])
if len(dirs) != 1:
    sys.exit(f"expected one starfold-server build script run, found {sorted(dirs)}")
print(dirs.pop())
' "$messages")
[ -f "$out_dir/x264-linked" ] || {
	echo "::error::the build recorded no x264 archive ($out_dir/x264-linked): STARFOLD_X264_LIB wasn't used"
	exit 1
}
got=$(cat "$out_dir/x264-linked")
echo "build linked x264 from $got"
[ "$got" = "$want" ] || {
	echo "::error::the build linked x264 from $got, not $want"
	exit 1
}
