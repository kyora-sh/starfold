#!/usr/bin/env bash
# Usage: publish-test.sh
#
# Runs publish.sh against a scratch remote and a stand-in gh that keeps releases in a directory,
# and checks the fresh release, a re-run after a failed upload, a re-run of a complete release,
# and the cases that must fail: a released file that differs from the build, an asset that isn't
# expected, and a tag that moved.
# The checks are strings eval runs, so shellcheck sees neither their expansions nor their uses.
# shellcheck disable=SC2016,SC2034
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com
export GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com

failures=0
check() {
	if eval "$2"; then echo "ok   $1"; else
		echo "FAIL $1"
		failures=$((failures + 1))
	fi
}

# The stand-in gh: $GH_STATE/<tag>/ is a release, its files the assets. FAIL_UPLOAD_AFTER=n makes
# `release create` stop with an error after n assets, as a failed upload leaves a release.
mkdir -p "$root/bin"
cat >"$root/bin/gh" <<'GH'
#!/usr/bin/env bash
set -euo pipefail
[ "$1" = release ] || { echo "fake gh: $*" >&2; exit 2; }
cmd=$2 tag=$3
shift 3
rel=$GH_STATE/$tag
case $cmd in
view)
	[ -d "$rel" ] || { echo "release not found" >&2; exit 1; }
	case "$*" in *assets*) ls -1 "$rel" ;; esac
	;;
create)
	mkdir "$rel"
	n=0
	for a in "$@"; do
		if [ -f "$a" ] && [ "${a#"$GH_DIST"/}" != "$a" ]; then
			if [ -n "${FAIL_UPLOAD_AFTER:-}" ] && [ "$n" -ge "$FAIL_UPLOAD_AFTER" ]; then
				echo "upload failed" >&2
				exit 1
			fi
			cp "$a" "$rel/"
			n=$((n + 1))
		fi
	done
	;;
upload)
	for a in "$@"; do
		[ ! -e "$rel/$(basename "$a")" ] || { echo "asset exists: $a" >&2; exit 1; }
		cp "$a" "$rel/"
	done
	echo "uploaded $#" >>"$GH_STATE/uploads.log"
	;;
download)
	[ "$1" = --pattern ] && [ "$3" = --dir ] || exit 2
	cp "$rel/$2" "$4/"
	;;
*) exit 2 ;;
esac
GH
chmod +x "$root/bin/gh"
export GH=$root/bin/gh

# A remote with the tag, a clone to run in, and the build's files.
git init -q --bare "$root/origin.git"
git init -q "$root/work"
cd "$root/work"
git commit -q --allow-empty -m one
git tag v0.0.1
git remote add origin "$root/origin.git"
git push -q origin HEAD:refs/heads/main v0.0.1
commit=$(git rev-parse HEAD)
dist=$root/dist
export GH_DIST=$dist
while IFS= read -r f; do
	mkdir -p "$dist/$(dirname "$f")"
	echo "$f" >"$dist/$f"
done < <("$here/expected-files.sh" 0.0.1 starfold)
count=$("$here/expected-files.sh" 0.0.1 starfold | wc -l | tr -d ' ')
echo notes >"$root/notes.md"

run() { "$here/publish.sh" v0.0.1 "$commit" "$root/notes.md" "$dist" starfold >"$root/out" 2>&1; }
fresh() {
	export GH_STATE=$root/state-$1
	rm -rf "$GH_STATE"
	mkdir -p "$GH_STATE"
}
released() { find "$GH_STATE/v0.0.1" -type f | wc -l | tr -d ' '; }

fresh a
check "a fresh release holds every expected file" 'run && [ "$(released)" = "$count" ]'
check "a complete release re-runs without uploading" 'run && [ ! -e "$GH_STATE/uploads.log" ] && grep -q "0 missing" "$root/out"'

fresh b
check "an upload that fails leaves a partial release and fails" '! FAIL_UPLOAD_AFTER=5 run && [ "$(released)" = 5 ]'
check "the re-run uploads only what is missing" 'run && [ "$(released)" = "$count" ] && grep -q "uploaded $((count - 5))" "$GH_STATE/uploads.log"'

fresh c
FAIL_UPLOAD_AFTER=5 run || true
first=$(find "$GH_STATE/v0.0.1" -type f | sort | head -1)
echo tampered >"$first"
check "a released file that differs from the build fails the re-run" '! run && grep -q "isn.t the one built" "$root/out"'
check "and nothing more is uploaded" '[ ! -e "$GH_STATE/uploads.log" ] && [ "$(released)" = 5 ]'

fresh d
FAIL_UPLOAD_AFTER=5 run || true
echo stray >"$GH_STATE/v0.0.1/stray.bin"
check "an asset that isn't expected fails the re-run" '! run && grep -q "stray.bin" "$root/out"'

fresh e
git commit -q --allow-empty -m two
git tag -f v0.0.1 >/dev/null
git push -q -f origin v0.0.1
check "a tag that names another commit creates nothing" '! run && [ ! -e "$GH_STATE/v0.0.1" ] && grep -q "no release was created or changed" "$root/out"'

[ "$failures" = 0 ] || {
	echo "$failures failed"
	exit 1
}
echo "all passed"
