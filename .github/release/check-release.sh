#!/usr/bin/env bash
# check-release.sh <tag> <dist dir> <flavour...>: everything the release job checks before it creates
# anything. Fails unless:
#   - the tag is v<version> of crates/starfold-server/Cargo.toml, and python/starfold/pyproject.toml
#     declares the same version;
#   - <dist dir> holds exactly the files expected-files.sh lists for that version and those flavours,
#     and every .sha256 there checks;
#   - the wheel's and the sdist's metadata and the embeddable client's package.json carry the version;
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
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/crates/starfold-server/Cargo.toml")
py=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/python/starfold/pyproject.toml")
[ -n "$version" ] || fail "no version in crates/starfold-server/Cargo.toml"
[ "$tag" = "v$version" ] || fail "tag $tag doesn't match version $version in crates/starfold-server/Cargo.toml"
[ "$py" = "$version" ] || fail "python/starfold/pyproject.toml declares $py, Cargo.toml $version"

diff -u <("$here/expected-files.sh" "$version" "$@" | sort) <(cd "$dist" && find . -type f | sed 's|^\./||' | sort) ||
	fail "the files in $dist differ from the expected set (- expected, + found)"
(cd "$dist" && for s in *.sha256; do
	[ "$(awk '{print $2}' "$s")" = "${s%.sha256}" ] || fail "$s names $(awk '{print $2}' "$s"), not ${s%.sha256}"
	sha256sum -c "$s"
done)

wheel=$dist/starfold-$version-py3-none-any.whl
meta=$(unzip -p "$wheel" "starfold-$version.dist-info/METADATA")
grep -q -x "Version: $version" <<<"$meta" || fail "the wheel's metadata doesn't say Version: $version"
meta=$(tar -xOzf "$dist/starfold-$version.tar.gz" "starfold-$version/PKG-INFO")
grep -q -x "Version: $version" <<<"$meta" || fail "the sdist's metadata doesn't say Version: $version"
meta=$(tar -xOzf "$dist/starfold-web-embed-$version.tgz" package/package.json)
grep -q -F "\"version\": \"$version\"," <<<"$meta" || fail "the embeddable client's package.json doesn't carry version $version"

if [ "${RELEASE_LOOKUP:-1}" = 1 ] && "${GH:-gh}" release view "$tag" >/dev/null 2>&1; then
	fail "$tag already has a release"
fi
echo "release $tag: $(find "$dist" -type f | wc -l | tr -d ' ') files checked"
