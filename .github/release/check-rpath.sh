#!/usr/bin/env bash
# check-rpath.sh <readelf> <binary> <multiarch>: fails unless every RPATH/RUNPATH entry of the
# binary is /lib, /usr/lib or their <multiarch> directory. An empty entry (a leading, trailing or
# doubled `:`) means the current directory to the loader and fails too.
set -euo pipefail
readelf=${1:?usage: check-rpath.sh <readelf> <binary> <multiarch>}
bin=$2 multiarch=$3
dynamic=$("$readelf" -d "$bin")
grep -E 'NEEDED|RPATH|RUNPATH' <<<"$dynamic" || true
bad=0
while IFS= read -r path; do
	while IFS= read -r entry; do
		case $entry in
		/lib | /lib/ | /usr/lib | /usr/lib/ | "/lib/$multiarch" | "/lib/$multiarch/" | "/usr/lib/$multiarch" | "/usr/lib/$multiarch/") ;;
		*)
			echo "::error::$bin: rpath entry '${entry}' (in '$path') isn't a system library directory"
			bad=1
			;;
		esac
	done < <(awk -v p="$path" 'BEGIN { n = split(p, a, ":"); for (i = 1; i <= n; i++) print a[i] }')
done < <(sed -n 's/.*(R\(UN\)\{0,1\}PATH).*\[\(.*\)\]$/\2/p' <<<"$dynamic")
exit $bad
