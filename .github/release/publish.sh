#!/usr/bin/env bash
# publish.sh <tag> <commit> <notes file> <dist dir> <flavour...>: creates the GitHub release of an
# existing tag (--verify-tag) with exactly the files expected-files.sh lists from <dist dir>.
#
# <commit> is the one the Version job resolved the tag to and every job built. The release is
# created only while the tag on the remote still names it, and afterwards the tag is read again:
# a tag that moved in between fails the job loudly; nothing is deleted or rolled back, which is a
# maintainer's call. GH (default gh) and the remote `origin` are used as given.
#
# A re-run (the tag's release exists already, say after an upload failed) creates nothing: each
# expected file the release has is downloaded and compared with <dist dir>'s by sha256, a mismatch
# fails, the missing ones are uploaded, and a release asset that isn't expected fails too. Either
# way the release ends with exactly the expected asset names, which is read back and checked, and
# a draft (what an interrupted `gh release create` leaves) is then published. More than one
# release or draft for the tag fails before anything changes.
set -euo pipefail
tag=${1:?usage: publish.sh <tag> <commit> <notes file> <dist dir> <flavour...>}
commit=$2 notes=$3 dist=$4
shift 4
here=$(cd "$(dirname "$0")" && pwd)
gh=${GH:-gh}
v=${tag#v}
fail() {
	echo "::error::$*" >&2
	exit 1
}
files=()
while IFS= read -r f; do
	files+=("$dist/$f")
done < <("$here/expected-files.sh" "$v" "$@")
for f in "${files[@]}"; do
	[ -f "$f" ] || fail "no $f"
done
names=$(for f in "${files[@]}"; do basename "$f"; done | sort)

tag_check() {
	local now
	now=$("$here/tag-commit.sh" "$tag")
	[ "$now" = "$commit" ] || fail "$1 ($tag names $now on the remote, not $commit, the commit built)"
}
# The releases of the tag, drafts included ("<id> <draft>" each), from the REST list: `gh release
# view <tag>` finds at most one, and a draft only through its pending tag name.
releases() {
	"$gh" api --paginate "repos/{owner}/{repo}/releases" --jq ".[] | select(.tag_name == \"$tag\") | \"\(.id) \(.draft)\""
}
# one: the single release of the tag, as "<id> <draft>"; none prints nothing; more than one fails.
one() {
	local found
	found=$(releases) || fail "the releases of the repository couldn't be listed"
	[ "$(grep -c . <<<"$found")" -le 1 ] ||
		fail "$tag has $(grep -c . <<<"$found") releases or drafts ($(tr '\n' ' ' <<<"$found")); a maintainer has to keep one"
	echo "$found"
}
assets() { "$gh" api "repos/{owner}/{repo}/releases/$id" --jq '.assets[].name' | sort; }
sum() { sha256sum "$1" | cut -d' ' -f1; }

tag_check "no release was created or changed"
found=$(one)
if [ -n "$found" ]; then
	read -r id draft <<<"$found"
	echo "the release of $tag exists (id $id, draft $draft)"
	have=$(assets)
	extra=$(comm -13 <(cat <<<"$names") <(cat <<<"$have") | sed '/^$/d')
	[ -z "$extra" ] || fail "the release of $tag has assets that aren't expected: $(tr '\n' ' ' <<<"$extra")"
	work=$(mktemp -d)
	trap 'rm -rf "$work"' EXIT
	missing=()
	for f in "${files[@]}"; do
		name=$(basename "$f")
		if grep -q -x -F "$name" <<<"$have"; then
			"$gh" release download "$tag" --pattern "$name" --dir "$work"
			[ "$(sum "$work/$name")" = "$(sum "$f")" ] ||
				fail "the release's $name isn't the one built ($(sum "$work/$name"), built $(sum "$f")); it needs a maintainer"
			rm -f "$work/$name"
		else
			missing+=("$f")
		fi
	done
	echo "the release of $tag exists: $(($(wc -l <<<"$names") - ${#missing[@]})) assets match the build, ${#missing[@]} missing"
	if [ ${#missing[@]} -gt 0 ]; then
		"$gh" release upload "$tag" "${missing[@]}"
	fi
else
	# --target: should the tag vanish before GitHub publishes, the tag it recreates names the commit built.
	args=(--verify-tag --target "$commit" --title "starfold $v" --generate-notes --notes-file "$notes")
	prev=$(git describe --tags --abbrev=0 --match 'v*' "$commit^" 2>/dev/null || true)
	if [ -n "$prev" ]; then
		args+=(--notes-start-tag "$prev")
	fi
	"$gh" release create "$tag" "${args[@]}" "${files[@]}"
	found=$(one)
	[ -n "$found" ] || fail "gh release create made no release of $tag"
	read -r id draft <<<"$found"
fi
[ "$(assets)" = "$names" ] || {
	diff <(cat <<<"$names") <(assets) || true
	fail "the release of $tag doesn't hold exactly the expected assets (- expected, + released)"
}
# gh release create makes a draft, uploads and then publishes it: one it couldn't finish is a
# draft, published here once it holds exactly the build's files.
if [ "$draft" = true ]; then
	"$gh" release edit "$tag" --draft=false
	[ "$("$gh" api "repos/{owner}/{repo}/releases/$id" --jq '.draft')" = false ] || fail "the release of $tag is still a draft"
	echo "published the draft release of $tag"
fi
tag_check "$tag moved while its release was made from $commit; the release holds $commit's files under a tag that names another commit, and needs a maintainer"
echo "released $tag at $commit with ${#files[@]} files"
