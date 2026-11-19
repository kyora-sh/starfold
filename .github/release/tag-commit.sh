#!/usr/bin/env bash
# tag-commit.sh <tag> [remote]: the commit the tag names on the remote (default origin): an
# annotated tag's peeled commit, a lightweight tag's own. Fails when the remote has no such tag.
set -euo pipefail
tag=${1:?usage: tag-commit.sh <tag> [remote]}
refs=$(git ls-remote "${2:-origin}" "refs/tags/$tag" "refs/tags/$tag^{}")
peeled=$(awk -v r="refs/tags/$tag^{}" '$2 == r { print $1 }' <<<"$refs")
plain=$(awk -v r="refs/tags/$tag" '$2 == r { print $1 }' <<<"$refs")
commit=${peeled:-$plain}
[ -n "$commit" ] || {
	echo "::error::the remote has no tag $tag" >&2
	exit 1
}
echo "$commit"
