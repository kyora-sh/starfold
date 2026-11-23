#!/usr/bin/env bash
# crates.sh dry-run|publish: packages the workspace's crates (starfold-aom-sys, then the server
# crate) and, with `publish`, uploads them to crates.io with the token in CARGO_REGISTRY_TOKEN.
# A crate whose version crates.io already has is left out (starfold-aom-sys keeps its version
# across most releases); when none is left there is nothing to do. Before packaging: libaom's
# submodule is at the commit the repository records, unchanged (the packaged crate can't check
# it). `dry-run` stages the web client (stage-crate-web.sh) and has cargo verify every package by
# building it from its .crate alone, with ESBUILD=false so a build that reached for esbuild fails;
# each .crate stays under crates.io's 10 MB limit. `publish` builds nothing: it packages the web
# client a dry run of the same commit staged (in the workflow, its artifact) without verifying,
# and uploads.
set -euo pipefail
mode=${1:?usage: crates.sh dry-run|publish}
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
cd "$root"
case $mode in
	dry-run) dry=(--dry-run) verify=() ;;
	publish) dry=() verify=(--no-verify) ;;
	*) echo "usage: crates.sh dry-run|publish" >&2; exit 2 ;;
esac

for c in crates/*/; do
	cmp LICENSE "$c/LICENSE" || { echo "::error::${c}LICENSE isn't the repository's LICENSE"; exit 1; }
done
aom=crates/starfold-aom-sys/aom
status=$(git submodule status -- "$aom")
case $status in
	" "*) ;;
	*) echo "::error::$aom isn't checked out at the commit the repository records: $status"; exit 1 ;;
esac
[ -z "$(git -C "$aom" status --porcelain --untracked-files=no)" ] || { echo "::error::$aom has local changes"; exit 1; }

# cargo counts the staged web/ (ignored by git, listed in `include`) as uncommitted, so packaging
# passes --allow-dirty; this check stands in for cargo's: no tracked file changed, and nothing
# untracked under crates/ that a package could take in.
dirty=$(git status --porcelain --untracked-files=no; git status --porcelain -- crates)
[ -z "$dirty" ] || { echo "::error::uncommitted changes:"; echo "$dirty"; exit 1; }
if [ "$mode" = dry-run ]; then
	"$here/stage-crate-web.sh"
elif [ ! -f crates/starfold-server/web/client.js ]; then
	echo "::error::crates/starfold-server/web holds no staged web client: run crates.sh dry-run first"
	exit 1
fi

meta=$(cargo metadata --locked --no-deps --format-version 1)
pkgdir=$(python3 -I -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])' <<<"$meta")/package
# name<TAB>version of every package the workspace may publish.
packages=$(python3 -I -c '
import json, sys
for p in json.load(sys.stdin)["packages"]:
    if p["publish"] is None or "crates-io" in p["publish"]:
        print(p["name"], p["version"], sep="\t")
' <<<"$meta")
# Whether crates.io's index lists name at version (build metadata aside, as crates.io compares).
published() {
	local name=$1 version=${2%%+*} index
	index=$(mktemp)
	code=$(curl -sS -o "$index" -w '%{http_code}' -A 'starfold-release (https://github.com/kyora-sh/starfold)' \
		"https://index.crates.io/${name:0:2}/${name:2:2}/$name")
	case $code in
		404) rm -f "$index"; return 1 ;;
		200) ;;
		*) echo "::error::crates.io's index answered $code for $name"; exit 1 ;;
	esac
	python3 -I -c '
import json, sys
want = sys.argv[2]
vers = [json.loads(l)["vers"].split("+")[0] for l in open(sys.argv[1], encoding="utf-8") if l.strip()]
sys.exit(0 if want in vers else 1)
' "$index" "$version"
	local rc=$?
	rm -f "$index"
	return $rc
}
selected=()
while IFS=$'\t' read -r name version; do
	if published "$name" "$version"; then
		echo "$name $version: on crates.io already, left out"
	else
		echo "$name $version: to package"
		selected+=(-p "$name")
	fi
done <<<"$packages"
if [ ${#selected[@]} = 0 ]; then
	echo "every crate's version is on crates.io already: nothing to $mode"
	exit 0
fi

rm -rf "$pkgdir"
ESBUILD=false cargo package ${verify[@]+"${verify[@]}"} --allow-dirty --locked "${selected[@]}"
for f in "$pkgdir"/*.crate; do
	size=$(wc -c <"$f" | tr -d ' ')
	echo "$(basename "$f"): $size bytes"
	[ "$size" -lt 10485760 ] || { echo "::error::$f is over crates.io's 10 MB limit"; exit 1; }
	files=$(tar -tzf "$f" | sed 's|^[^/]*/||')
	grep -qx '\.cargo_vcs_info\.json' <<<"$files" || { echo "::error::$f has no .cargo_vcs_info.json"; exit 1; }
	case $(basename "$f") in
	starfold-[0-9]*)
		grep '^web/' <<<"$files"
		# The texts of the three licences its `license` names: MIT, BSD-2-Clause (dav1d), OFL-1.1 (Inter).
		for want in web/client.js LICENSE web/COPYING-dav1d web/COPYING-wasi-libc web/inter-LICENSE.txt; do
			grep -qxF "$want" <<<"$files" || { echo "::error::$f lacks $want"; exit 1; }
		done
		;;
	esac
done
# A dry run's `cargo package` above built each one from its .crate; an upload is that same tree.
ESBUILD=false cargo publish ${dry[@]+"${dry[@]}"} --no-verify --allow-dirty --locked "${selected[@]}"
