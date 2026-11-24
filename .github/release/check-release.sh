#!/usr/bin/env bash
# check-release.sh <tag> <dist dir> <staged x264 source dir> <flavour...>: everything checked before
# a release is created. With --downloads-only in place of the staged directory, it stops after the
# versions and the downloaded set: run that before staging x264's source. <dist dir> holds the downloaded workflow artifacts, one directory each
# (never merged); x264's source is staged elsewhere (stage-x264-source.sh) and joins only after
# the downloaded set has passed. Fails unless:
#   - Cargo.toml's version (tomllib) names the tag, v<version>, and pyproject.toml declares it;
#   - the downloaded set is exactly what expected-files.sh lists outside x264-source/, by directory
#     and name, no more and no fewer (an artifact named x264-source fails), no symbolic link;
#   - the staged source is exactly expected-files.sh's x264-source files, and once joined as
#     <dist dir>/x264-source the whole set is exactly the list;
#   - every .sha256 names the file beside it and checks;
#   - the wheels' and the sdist's first Version header and every npm tarball's package.json version
#     are the version (compared in versions.py), as is packages/starfold/package.json's;
#   - each npm server package's and platform wheel's binary is byte for byte the default archive's
#     (expected-files.sh native pairs them), each server package states its platform's licence
#     expression (python-platform-wheel.py licence), and the starfold tarball pins exactly the
#     four server packages at the version;
#   - the x264 source files match third_party/x264's SHA256SUMS;
#   - the tag has no GitHub release yet (GH, default gh, is asked; RELEASE_LOOKUP=0, for a dry run
#     that builds a version already released, skips this one). With RELEASE_RESUME=1 (a re-run of
#     the release job) an existing release passes, and publish.sh completes and verifies it.
set -euo pipefail
tag=${1:?usage: check-release.sh <tag> <dist dir> <staged x264 source dir> <flavour...>}
dist=$2 staged=$3
shift 3
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
fail() {
	echo "::error::$*"
	exit 1
}
versions() { python3 -I "$here/versions.py" "$@"; }
v=$(versions cargo "$root/crates/starfold-server/Cargo.toml")
[ "$tag" = "v$v" ] || fail "tag $tag isn't v$v, the version in crates/starfold-server/Cargo.toml"
versions check pyproject "$root/python/starfold/pyproject.toml" "$v" ||
	fail "python/starfold/pyproject.toml doesn't declare $v, crates/starfold-server/Cargo.toml's version"
versions check package-json-file "$root/packages/starfold/package.json" "$v" ||
	fail "packages/starfold/package.json doesn't declare $v, crates/starfold-server/Cargo.toml's version"

listing() { (cd "$1" && find . -mindepth 1 \( -type f -o -type l \) | sed 's|^\./||' | sort); }
links() { find "$1" -mindepth 1 -type l | wc -l | tr -d ' '; }
same() { # same <what> <expected> <found>
	diff -u <(cat <<<"$2") <(cat <<<"$3") || fail "$1 differs from the expected set (- expected, + found)"
	[ "$(wc -l <<<"$2")" = "$(wc -l <<<"$3")" ] || fail "$1: $(wc -l <<<"$3") files, $(wc -l <<<"$2") expected"
}
expected=$("$here/expected-files.sh" "$v" "$@" | sort)
dupes=$(sed 's|.*/||' <<<"$expected" | sort | uniq -d)
[ -z "$dupes" ] || fail "expected-files.sh lists a file name twice: $dupes"

[ ! -e "$dist/x264-source" ] || fail "a downloaded artifact is named x264-source"
[ "$(links "$dist")" = 0 ] || fail "$dist holds a symbolic link"
same "the downloaded artifacts in $dist" "$(grep -v '^x264-source/' <<<"$expected")" "$(listing "$dist")"
if [ "$staged" = --downloads-only ]; then
	echo "downloads of $tag: the expected set, $(listing "$dist" | wc -l | tr -d ' ') files"
	exit 0
fi
[ "$(links "$staged")" = 0 ] || fail "$staged holds a symbolic link"
same "the staged x264 source in $staged" "$(grep '^x264-source/' <<<"$expected" | sed 's|^x264-source/||')" "$(listing "$staged")"
mkdir "$dist/x264-source"
cp -p "$staged"/* "$dist/x264-source/"
same "$dist with x264's source joined" "$expected" "$(listing "$dist")"

while IFS= read -r s; do
	case $s in *.sha256) ;; *) continue ;; esac
	d=$(dirname "$s") f=$(basename "$s" .sha256)
	read -r _ named rest <"$dist/$s"
	[ "$named" = "$f" ] && [ -z "$rest" ] || fail "$s names '$named $rest', not $f"
	[ "$(wc -l <"$dist/$s" | tr -d ' ')" = 1 ] || fail "$s has more than one line"
	(cd "$dist/$d" && sha256sum -c --quiet "$f.sha256") || fail "$s doesn't check"
done <<<"$expected"

versions check wheel "$dist/starfold-python-$v/starfold-$v-py3-none-any.whl" "$v" || fail "the wheel's Version isn't $v"
versions check sdist "$dist/starfold-python-$v/starfold-$v.tar.gz" "$v" || fail "the sdist's Version isn't $v"
versions check package-json "$dist/starfold-web-embed-$v/starfold-web-embed-$v.tgz" "$v" ||
	fail "the embeddable client's package.json version isn't $v"

# member <archive> <path>: one file of a .tar.gz, .tgz, .whl or .zip, to stdout.
member() {
	python3 -I -c '
import sys, tarfile, zipfile
path, name = sys.argv[1], sys.argv[2]
if path.endswith((".zip", ".whl")):
    with zipfile.ZipFile(path) as z:
        data = z.read(name)
else:
    with tarfile.open(path) as t:
        f = t.extractfile(name)
        if f is None:
            sys.exit(f"{path}: {name} is not a file")
        data = f.read()
sys.stdout.buffer.write(data)
' "$1" "$2"
}
sum() { sha256sum | cut -d' ' -f1; }
servers=()
while read -r target suffix archive tgz wheel <&3; do
	case $target in *-windows) binary=starfold.exe ;; *) binary=starfold ;; esac
	want=$(member "$dist/$archive" "$(basename "$(dirname "$archive")")/$binary" | sum)
	versions check package-json "$dist/$tgz" "$v" || fail "$tgz: package.json version isn't $v"
	got=$(member "$dist/$tgz" "package/bin/$binary" | sum)
	[ "$got" = "$want" ] || fail "$tgz: bin/$binary is $got, $archive has $want"
	licence=$(python3 -I "$here/python-platform-wheel.py" licence "$target")
	member "$dist/$tgz" package/package.json | python3 -I -c 'import json, sys; sys.exit(json.load(sys.stdin).get("license") != sys.argv[1])' "$licence" ||
		fail "$tgz: package.json's license isn't $licence"
	if [ "$wheel" != - ]; then
		versions check wheel "$dist/$wheel" "$v" || fail "$wheel: Version isn't $v"
		got=$(member "$dist/$wheel" "starfold/_bin/$binary" | sum)
		[ "$got" = "$want" ] || fail "$wheel: starfold/_bin/$binary is $got, $archive has $want"
	fi
	echo "$target: $binary $want in $archive, $tgz, $wheel"
	servers+=("@starfold/server-$suffix")
done 3< <("$here/expected-files.sh" native "$v")
sdk=starfold-npm-$v/starfold-$v.tgz
versions check package-json "$dist/$sdk" "$v" || fail "$sdk: package.json version isn't $v"
member "$dist/$sdk" package/package.json | python3 -I -c '
import json, sys
m, version, servers = json.load(sys.stdin), sys.argv[1], sys.argv[2:]
want = {s: version for s in servers}
name, got = m.get("name"), m.get("optionalDependencies")
if name != "starfold" or got != want:
    sys.exit(f"{name}: optionalDependencies {got}, expected {want}")
' "$v" "${servers[@]}" || fail "$sdk doesn't pin the four server packages at $v"

while read -r sum name; do
	case $name in vcpkg-*) continue ;; esac
	[ -f "$dist/x264-source/$name" ] || continue
	[ "$sum" = "$(sha256sum "$dist/x264-source/$name" | cut -d' ' -f1)" ] ||
		fail "x264-source/$name doesn't match third_party/x264/SHA256SUMS"
done <"$root/third_party/x264/SHA256SUMS"

if [ "${RELEASE_LOOKUP:-1}" = 1 ] && "${GH:-gh}" release view "$tag" >/dev/null 2>&1; then
	[ "${RELEASE_RESUME:-0}" = 1 ] || fail "$tag already has a release"
	echo "::notice::$tag has a release already; publish.sh uploads what it lacks and verifies the rest"
fi
echo "release $tag: $(wc -l <<<"$expected" | tr -d ' ') files checked"
