#!/usr/bin/env bash
# publish.sh <tag> <commit> <notes file> <file...>: creates the release as a draft with the files,
# then publishes it only if the tag on the remote still names <commit> (the commit the files were
# built from); otherwise deletes the draft and fails. The tag is checked again after publishing, and
# a release whose tag moved in between goes back to a draft and the job fails, so a published
# release never stays on a tag that names another commit. GH (default gh) and the remote `origin`
# are used as given.
set -euo pipefail
tag=${1:?usage: publish.sh <tag> <commit> <notes file> <file...>}
commit=$2 notes=$3
shift 3
gh=${GH:-gh}
remote_commit() {
	local refs
	refs=$(git ls-remote origin "refs/tags/$tag" "refs/tags/$tag^{}")
	# An annotated tag's peeled line (^{}) is the commit; a lightweight tag has only its own line.
	if grep -q '\^{}$' <<<"$refs"; then
		grep '\^{}$' <<<"$refs" | cut -f1
	else
		cut -f1 <<<"$refs"
	fi
}
args=(--verify-tag --draft --title "starfold ${tag#v}" --generate-notes --notes-file "$notes")
prev=$(git describe --tags --abbrev=0 --match 'v*' "$tag^" 2>/dev/null || true)
if [ -n "$prev" ]; then
	args+=(--notes-start-tag "$prev")
fi
"$gh" release create "$tag" "${args[@]}" "$@"
now=$(remote_commit)
if [ "$now" != "$commit" ]; then
	"$gh" release delete "$tag" --yes
	echo "::error::$tag names ${now:-nothing} on the remote, not $commit (the commit built); the draft is deleted"
	exit 1
fi
"$gh" release edit "$tag" --draft=false
now=$(remote_commit)
if [ "$now" != "$commit" ]; then
	"$gh" release edit "$tag" --draft=true
	echo "::error::$tag moved to ${now:-nothing} while the release was published; it is a draft again"
	exit 1
fi
echo "published $tag at $commit"
