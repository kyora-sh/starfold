#!/usr/bin/env bash
# check-rpath.sh <readelf> <binary> <multiarch>: fails unless every RPATH/RUNPATH entry of the
# binary is /lib, /usr/lib or their <multiarch> directory. Refused too: an RPATH/RUNPATH that is
# present but empty, an empty entry (a leading, trailing or doubled `:`, which the loader reads as
# the current directory), an entry that isn't absolute, and any backslash.
set -euo pipefail
readelf=${1:?usage: check-rpath.sh <readelf> <binary> <multiarch>}
bin=$2 multiarch=$3
dynamic=$("$readelf" -d "$bin")
grep -E 'NEEDED|RPATH|RUNPATH' <<<"$dynamic" || true
bad=0
refuse() {
	echo "::error::$bin: $*"
	bad=1
}
while IFS= read -r line; do
	value=${line#*\[}
	value=${value%\]}
	case $value in
	"") refuse "an empty rpath ($line)" && continue ;;
	*\\*) refuse "an rpath with a backslash: '$value'" && continue ;;
	:* | *: | *::*) refuse "an empty rpath entry in '$value'" ;;
	esac
	IFS=: read -r -a entries <<<"$value"
	for entry in "${entries[@]}"; do
		case $entry in
		"") ;;
		/lib | /lib/ | /usr/lib | /usr/lib/ | "/lib/$multiarch" | "/lib/$multiarch/" | "/usr/lib/$multiarch" | "/usr/lib/$multiarch/") ;;
		*) refuse "rpath entry '$entry' (in '$value') isn't a system library directory" ;;
		esac
	done
done < <(grep -E '\((RPATH|RUNPATH)\)' <<<"$dynamic" | grep -o -E 'Library r(un)?path: \[.*\]$')
exit $bad
