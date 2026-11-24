#!/usr/bin/env bash
# npm-has.sh <package> <version>: whether npm has that version of the package, from the registry's
# own list of versions. Prints "present" or "absent" and exits 0; any other answer (no reply, an
# error, a package npm doesn't know, a reply that isn't a version list) fails, so a publish is
# neither skipped nor attempted on a guess.
set -euo pipefail
name=${1:?usage: npm-has.sh <package> <version>}
version=${2:?usage: npm-has.sh <package> <version>}
cache=$(mktemp -d)
trap 'rm -rf "$cache"' EXIT
list=$(npm view "$name" versions --json --cache "$cache" --prefer-online) || {
	echo "npm-has.sh: the registry gave no version list for $name" >&2
	exit 1
}
python3 -I -c '
import json, sys
versions = json.loads(sys.argv[1])
if isinstance(versions, str):
    versions = [versions]
if not isinstance(versions, list) or not versions or not all(isinstance(v, str) for v in versions):
    sys.exit(f"npm-has.sh: not a version list: {sys.argv[1]!r}")
print("present" if sys.argv[2] in versions else "absent")
' "$list" "$version"
