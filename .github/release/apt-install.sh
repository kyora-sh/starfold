#!/usr/bin/env bash
# apt-install.sh <package...>: run as root in a clean Ubuntu container. Fetches the package index
# and the named packages, then installs them. When the runner has an apt proxy (a package cache
# serving the same archive's files), the workflow mounts its settings at /run/runner-apt-proxy.conf
# and it gets one bounded attempt first; then the archive is fetched directly. No attempt can hang:
# each phase is bounded, a missing index fails the attempt, and on failure apt's output is printed
# with the phase and the hosts apt was still waiting on.
set -uo pipefail
proxy=/run/runner-apt-proxy.conf
log=$(mktemp)

hosts() {
	grep -h -o -E '^URIs:.*|^deb .*' /etc/apt/sources.list.d/*.sources /etc/apt/sources.list 2>/dev/null |
		grep -o -E 'https?://[^/ ]+' | sed -E 's#https?://##' | sort -u
}

# The log is public and a proxy URL can carry a user and password: printed apt output has them
# removed, up to the last @ of a URL's authority or of any other word.
redacted() {
	sed -E -e 's#([A-Za-z][A-Za-z0-9+.-]*://)[^/[:space:]]*@#\1#g' \
		-e 's#(^|[[:space:]])[^/[:space:]]*@#\1#g' "$@"
}

# run <path> <phase> <limit> <apt-get args...>
run() {
	local path=$1 phase=$2 limit=$3 rc start=$SECONDS waiting h
	shift 3
	timeout -k 10 "$limit" env DEBIAN_FRONTEND=noninteractive apt-get -q "$@" >"$log" 2>&1
	rc=$?
	echo "::group::apt-get $phase via $path ($((SECONDS - start))s, exit $rc)"
	redacted "$log"
	echo "::endgroup::"
	[ $rc = 0 ] && return 0
	if [ $rc = 124 ] || [ $rc = 137 ]; then
		echo "apt-get $phase via $path: no result after ${limit}s (stopped)."
	else
		echo "apt-get $phase via $path: exit $rc after $((SECONDS - start))s."
	fi
	if [ "$phase" = update ]; then
		waiting=
		for h in $(hosts); do
			grep -q -E "^(Hit|Get):[0-9]+ https?://$h/" "$log" || waiting="$waiting $h"
		done
		echo "  no index received from:${waiting:- (none; every host sent one)}"
	elif [ "$phase" = download ]; then
		echo "  last package started: $(grep -E '^Get:[0-9]+ ' "$log" | tail -n 1 | redacted)"
	fi
	echo "  last line: $(grep -v '^ *$' "$log" | tail -n 1 | redacted)"
	return $rc
}

# fetch <path> <index limit> <download limit> <apt-get options...>: index and packages, no install.
fetch() {
	local path=$1 index=$2 download=$3
	shift 3
	run "$path" update "$index" "$@" update --error-on=any &&
		run "$path" download "$download" "$@" install -y --no-install-recommends --download-only "${packages[@]}"
}

packages=("$@")
served=
if grep -q -i -E '"[a-z][a-z0-9+.-]*://' "$proxy" 2>/dev/null; then
	fetch "the runner's package cache" 60 120 -c "$proxy" -o Acquire::Retries=0 \
		-o Acquire::http::Timeout=15 -o Acquire::https::Timeout=15 && served="the runner's package cache"
fi
if [ -z "$served" ]; then
	fetch "the archive" 75 600 -o Acquire::Retries=1 \
		-o Acquire::http::Timeout=20 -o Acquire::https::Timeout=20 && served="the archive"
fi
if [ -z "$served" ]; then
	echo "::error::apt could not fetch ${packages[*]} (index or packages): see the reason above"
	exit 1
fi
echo "apt: ${packages[*]} fetched from $served"
run "local files" install 300 install -y --no-install-recommends --no-download "${packages[@]}" || {
	echo "::error::apt could not install ${packages[*]} from the downloaded files: see the output above"
	exit 1
}
