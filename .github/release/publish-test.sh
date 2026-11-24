#!/usr/bin/env bash
# Usage: publish-test.sh
#
# Runs publish.sh against a scratch remote and a stand-in gh that keeps releases in a directory,
# and checks the fresh release, a re-run after a failed upload (which leaves a draft), a re-run of
# a complete release or a complete draft, and the cases that must fail: a released file that
# differs from the build, an asset that isn't expected, two releases or drafts for the tag, and a
# tag that moved.
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

# The stand-in gh. A release is $GH_STATE/<id>/ with its tag, whether it is a draft, and its
# assets. As the real one, `release create` makes a draft, uploads and then publishes it;
# FAIL_UPLOAD_AFTER=n stops it with an error after n assets, leaving the draft. The commands that
# take a tag act on the first release of that tag, as gh's lookup does.
mkdir -p "$root/bin"
cat >"$root/bin/gh" <<'GH'
#!/usr/bin/env bash
set -euo pipefail
by_tag() {
	local d
	for d in "$GH_STATE"/*/; do
		[ -f "$d/tag" ] && [ "$(cat "$d/tag")" = "$1" ] && { echo "${d%/}"; return; }
	done
	echo "release not found" >&2
	exit 1
}
case "$1 $2" in
"api --paginate")
	tag=$(sed 's/.*tag_name == "\([^"]*\)".*/\1/' <<<"$5")
	for d in "$GH_STATE"/*/; do
		[ -f "$d/tag" ] && [ "$(cat "$d/tag")" = "$tag" ] && echo "$(basename "$d") $(cat "$d/draft")"
	done
	true
	;;
"api repos/{owner}/{repo}/releases/"*)
	d=$GH_STATE/${2##*/}
	case $4 in
	'.assets[].name') ls -1 "$d/assets" ;;
	'.draft') cat "$d/draft" ;;
	*) exit 2 ;;
	esac
	;;
"release create")
	n=$(($(cat "$GH_STATE/next" 2>/dev/null || echo 0) + 1))
	echo "$n" >"$GH_STATE/next"
	d=$GH_STATE/$n
	mkdir -p "$d/assets"
	echo "$3" >"$d/tag"
	echo true >"$d/draft"
	shift 3
	k=0
	for a in "$@"; do
		if [ -f "$a" ] && [ "${a#"$GH_DIST"/}" != "$a" ]; then
			if [ -n "${FAIL_UPLOAD_AFTER:-}" ] && [ "$k" -ge "$FAIL_UPLOAD_AFTER" ]; then
				echo "upload failed" >&2
				exit 1
			fi
			cp "$a" "$d/assets/"
			k=$((k + 1))
		fi
	done
	echo false >"$d/draft"
	;;
"release upload")
	d=$(by_tag "$3")
	shift 3
	for a in "$@"; do
		[ ! -e "$d/assets/$(basename "$a")" ] || { echo "asset exists: $a" >&2; exit 1; }
		cp "$a" "$d/assets/"
	done
	echo "uploaded $#" >>"$GH_STATE/uploads.log"
	;;
"release download")
	d=$(by_tag "$3")
	[ "$4" = --pattern ] && [ "$6" = --dir ] || exit 2
	cp "$d/assets/$5" "$7/"
	;;
"release edit")
	d=$(by_tag "$3")
	[ "$4" = --draft=false ] || exit 2
	echo false >"$d/draft"
	echo "published" >>"$GH_STATE/edits.log"
	;;
*)
	echo "fake gh: $*" >&2
	exit 2
	;;
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
rel() { find "$GH_STATE" -mindepth 1 -maxdepth 1 -type d | sort | head -1; }
released() { find "$(rel)/assets" -type f | wc -l | tr -d ' '; }
draft() { cat "$(rel)/draft"; }
releases() { find "$GH_STATE" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' '; }

fresh a
check "a fresh release holds every expected file, published" 'run && [ "$(released)" = "$count" ] && [ "$(draft)" = false ]'
check "a complete release re-runs without uploading or publishing" 'run && [ ! -e "$GH_STATE/uploads.log" ] && [ ! -e "$GH_STATE/edits.log" ] && grep -q "0 missing" "$root/out" && [ "$(releases)" = 1 ]'

fresh b
check "an upload that fails leaves a draft with some assets and fails" '! FAIL_UPLOAD_AFTER=5 run && [ "$(released)" = 5 ] && [ "$(draft)" = true ]'
check "the re-run uploads only what is missing and publishes the draft" 'run && [ "$(released)" = "$count" ] && grep -q "uploaded $((count - 5))" "$GH_STATE/uploads.log" && [ "$(draft)" = false ] && [ "$(releases)" = 1 ]'

fresh c
FAIL_UPLOAD_AFTER=5 run || true
cp "$dist"/*/* "$(rel)/assets/"
check "a complete draft is published without uploading" 'run && [ ! -e "$GH_STATE/uploads.log" ] && [ "$(draft)" = false ] && grep -q published "$GH_STATE/edits.log"'

fresh d
FAIL_UPLOAD_AFTER=5 run || true
first=$(find "$(rel)/assets" -type f | sort | head -1)
echo tampered >"$first"
check "a released file that differs from the build fails the re-run" '! run && grep -q "isn.t the one built" "$root/out"'
check "and nothing is uploaded or published" '[ ! -e "$GH_STATE/uploads.log" ] && [ "$(released)" = 5 ] && [ "$(draft)" = true ]'

fresh e
FAIL_UPLOAD_AFTER=5 run || true
echo stray >"$(rel)/assets/stray.bin"
check "an asset that isn't expected fails the re-run" '! run && grep -q "stray.bin" "$root/out" && [ "$(draft)" = true ]'

fresh f
FAIL_UPLOAD_AFTER=3 run || true
FAIL_UPLOAD_AFTER=5 GH_STATE=$GH_STATE "$GH" release create v0.0.1 "$dist"/*/* 2>/dev/null || true
check "two drafts for the tag fail, and nothing changes" '! run && grep -q "2 releases or drafts" "$root/out" && [ ! -e "$GH_STATE/uploads.log" ] && [ ! -e "$GH_STATE/edits.log" ]'

fresh g
run
FAIL_UPLOAD_AFTER=1 GH_STATE=$GH_STATE "$GH" release create v0.0.1 "$dist"/*/* 2>/dev/null || true
check "a release and a draft for the tag fail" '! run && grep -q "2 releases or drafts" "$root/out"'

fresh h
git commit -q --allow-empty -m two
git tag -f v0.0.1 >/dev/null
git push -q -f origin v0.0.1
check "a tag that names another commit creates nothing" '! run && [ "$(releases)" = 0 ] && grep -q "no release was created or changed" "$root/out"'

[ "$failures" = 0 ] || {
	echo "$failures failed"
	exit 1
}
echo "all passed"
