#!/usr/bin/env bash
# npm-dry-run.sh <tarball> <version>: npm publish --dry-run of the tarball, unless npm has its
# package at the version already. npm's dry run refuses a version the registry has, so without the
# check every branch that touches the packages fails between a release and the next version bump.
# npm-has.sh decides: "present" skips the dry run, "absent" runs it, and anything else fails, so a
# registry that doesn't answer never passes for a published version.
set -euo pipefail
tarball=${1:?usage: npm-dry-run.sh <tarball> <version>}
version=${2:?usage: npm-dry-run.sh <tarball> <version>}
here=$(cd "$(dirname "$0")" && pwd)
name=$(tar -xzOf "$tarball" package/package.json | python3 -I -c 'import json, sys; print(json.load(sys.stdin)["name"])')
answer=$("$here/npm-has.sh" "$name" "$version") || answer=failed
case $answer in
present) echo "$name@$version is on npm already: dry run skipped" ;;
absent) npm publish "./$tarball" --access public --dry-run ;;
*)
	echo "::error::npm-has.sh gave no answer for $name"
	exit 1
	;;
esac
