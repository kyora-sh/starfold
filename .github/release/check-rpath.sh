#!/usr/bin/env bash
# check-rpath.sh <readelf> <binary> <multiarch>: fails unless every RPATH/RUNPATH entry of the
# binary is /lib, /usr/lib or their <multiarch> directory. Refused too: an RPATH/RUNPATH that is
# present but empty, an empty entry (a leading, trailing or doubled `:`, which the loader reads as
# the current directory), an entry that isn't absolute, and any backslash.
#
# readelf runs with LC_ALL=C, and an (RPATH) or (RUNPATH) line whose value this script can't read
# fails rather than being skipped.
set -euo pipefail
readelf=${1:?usage: check-rpath.sh <readelf> <binary> <multiarch>}
bin=$2 multiarch=$3
dynamic=$(LC_ALL=C "$readelf" -d "$bin")
grep -E 'NEEDED|RPATH|RUNPATH' <<<"$dynamic" || true
bad=0
refuse() {
	echo "::error::$bin: $*"
	bad=1
}
while IFS= read -r line; do
	if ! grep -q -E '\((RPATH|RUNPATH)\) +Library r(un)?path: \[.*\]$' <<<"$line"; then
		refuse "an rpath line this check can't read: '$line'"
		continue
	fi
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
done < <(grep -E '\((RPATH|RUNPATH)\)' <<<"$dynamic")
exit $bad
