#!/usr/bin/env bash
# check-release.sh <tag> <dist dir> <flavour...>: everything checked before a release is created.
# <dist dir> holds one directory per workflow artifact (never merged) plus x264-source/ from
# stage-x264-source.sh. Fails unless:
#   - Cargo.toml's version (read with tomllib) is the tag's, v<version>, and pyproject.toml's;
#   - <dist dir> holds exactly the files expected-files.sh lists, by directory and name, no more and
#     no fewer, no file name twice;
#   - every .sha256 names the file beside it and checks;
#   - the wheel's and the sdist's first Version header and package.json's top-level version are it;
#   - the x264 source files match third_party/x264's manifests;
#   - the tag has no GitHub release yet (GH, default gh, is asked; RELEASE_LOOKUP=0, for a dry run
#     that builds a version already released, skips this one).
set -euo pipefail
tag=${1:?usage: check-release.sh <tag> <dist dir> <flavour...>}
dist=$2
shift 2
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
fail() {
	echo "::error::$*"
	exit 1
}
version() { python3 -I "$here/versions.py" "$@"; }
v=$(version cargo "$root/crates/starfold-server/Cargo.toml")
py=$(version pyproject "$root/python/starfold/pyproject.toml")
[ "$tag" = "v$v" ] || fail "tag $tag isn't v$v, the version in crates/starfold-server/Cargo.toml"
[ "$py" = "$v" ] || fail "python/starfold/pyproject.toml declares $py, crates/starfold-server/Cargo.toml $v"

expected=$("$here/expected-files.sh" "$v" "$@")
dupes=$(sed 's|.*/||' <<<"$expected" | sort | uniq -d)
[ -z "$dupes" ] || fail "expected-files.sh lists a file name twice: $dupes"
found=$(cd "$dist" && find . -mindepth 1 \( -type f -o -type l \) | sed 's|^\./||' | sort)
[ "$(find "$dist" -mindepth 1 -type l | wc -l)" = 0 ] || fail "$dist holds a symbolic link"
diff -u <(sort <<<"$expected") <(cat <<<"$found") || fail "the files in $dist differ from the expected set (- expected, + found)"
[ "$(wc -l <<<"$expected")" = "$(wc -l <<<"$found")" ] || fail "$dist holds $(wc -l <<<"$found") files, $(wc -l <<<"$expected") expected"

while IFS= read -r s; do
	case $s in *.sha256) ;; *) continue ;; esac
	d=$(dirname "$s") f=$(basename "$s" .sha256)
	[ "$(awk '{ print $2 }' "$dist/$s")" = "$f" ] || fail "$s names $(awk '{ print $2 }' "$dist/$s"), not $f"
	[ "$(wc -l <"$dist/$s")" = 1 ] || fail "$s has more than one line"
	(cd "$dist/$d" && sha256sum -c --quiet "$f.sha256") || fail "$s doesn't check"
done <<<"$expected"

wheel=$(version wheel "$dist/starfold-python-$v/starfold-$v-py3-none-any.whl")
sdist=$(version sdist "$dist/starfold-python-$v/starfold-$v.tar.gz")
web=$(version package-json "$dist/starfold-web-embed-$v/starfold-web-embed-$v.tgz")
[ "$wheel" = "$v" ] || fail "the wheel's Version is $wheel, not $v"
[ "$sdist" = "$v" ] || fail "the sdist's Version is $sdist, not $v"
[ "$web" = "$v" ] || fail "the embeddable client's package.json version is $web, not $v"

for f in $("$here/expected-files.sh" x264-source); do
	case $f in vcpkg-*) continue ;; esac
	want=$(awk -v f="$f" '$2 == f { print $1 }' "$root/third_party/x264/SHA256SUMS")
	[ -n "$want" ] && [ "$want" = "$(sha256sum "$dist/x264-source/$f" | cut -d' ' -f1)" ] ||
		fail "x264-source/$f doesn't match third_party/x264/SHA256SUMS"
done

if [ "${RELEASE_LOOKUP:-1}" = 1 ] && "${GH:-gh}" release view "$tag" >/dev/null 2>&1; then
	fail "$tag already has a release"
fi
echo "release $tag: $(wc -l <<<"$expected") files checked"
