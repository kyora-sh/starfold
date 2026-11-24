#!/usr/bin/env bash
# release-lookup.sh <tag>: whether the tag may get its release, asked of GH (default gh). Passes
# when gh says the release isn't there ("release not found" anywhere in its answer); fails when
# it is there, unless RELEASE_RESUME=1 (a re-run of the release job, which publish.sh completes);
# fails on any other answer, an API error say, so a lookup that didn't work is never taken for
# "no release yet".
set -euo pipefail
tag=${1:?usage: release-lookup.sh <tag>}
if lookup=$("${GH:-gh}" release view "$tag" --json tagName 2>&1); then
	if [ "${RELEASE_RESUME:-0}" != 1 ]; then
		echo "::error::$tag already has a release"
		exit 1
	fi
	echo "::notice::$tag has a release already; publish.sh uploads what it lacks and verifies the rest"
else
	case $lookup in
		*"release not found"*) echo "$tag has no release yet" ;;
		*)
			echo "::error::couldn't look up the release of $tag: $lookup"
			exit 1
			;;
	esac
fi
