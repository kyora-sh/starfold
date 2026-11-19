#!/usr/bin/env bash
# publish.sh <tag> <commit> <notes file> <dist dir> <flavour...>: creates the GitHub release of an
# existing tag (--verify-tag) with exactly the files expected-files.sh lists from <dist dir>.
#
# <commit> is the one the Version job resolved the tag to and every job built. The release is
# created only while the tag on the remote still names it, and afterwards the tag is read again:
# a tag that moved in between fails the job loudly; nothing is deleted or rolled back, which is a
# maintainer's call. GH (default gh) and the remote `origin` are used as given.
set -euo pipefail
tag=${1:?usage: publish.sh <tag> <commit> <notes file> <dist dir> <flavour...>}
commit=$2 notes=$3 dist=$4
shift 4
here=$(cd "$(dirname "$0")" && pwd)
gh=${GH:-gh}
v=${tag#v}
files=()
while IFS= read -r f; do
	files+=("$dist/$f")
done < <("$here/expected-files.sh" "$v" "$@")

now=$("$here/tag-commit.sh" "$tag")
[ "$now" = "$commit" ] || {
	echo "::error::$tag names $now on the remote, not $commit (the commit built); no release was created"
	exit 1
}
args=(--verify-tag --title "starfold $v" --generate-notes --notes-file "$notes")
prev=$(git describe --tags --abbrev=0 --match 'v*' "$commit^" 2>/dev/null || true)
if [ -n "$prev" ]; then
	args+=(--notes-start-tag "$prev")
fi
"$gh" release create "$tag" "${args[@]}" "${files[@]}"
now=$("$here/tag-commit.sh" "$tag")
[ "$now" = "$commit" ] || {
	echo "::error::$tag moved to $now while its release was created from $commit; the release is published with $commit's files under a tag that names another commit, and needs a maintainer"
	exit 1
}
echo "released $tag at $commit with ${#files[@]} files"
