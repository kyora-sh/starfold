#!/usr/bin/env bash
# smoke-linux.sh <archive> <version> <licence> <package...>: run as root in a clean Ubuntu
# container. Installs only the named runtime packages, shows the binary's ldd and fails on a
# library not found, checks --version, then starts it on Xvfb (installed afterwards, as the
# test's X server) until it is listening.
set -euo pipefail
archive=$1 version=$2 licence=$3
shift 3
here=$(cd "$(dirname "$0")" && pwd)
apt() { DEBIAN_FRONTEND=noninteractive apt-get -q -o Acquire::Retries=3 "$@"; }
apt update
apt install -y --no-install-recommends "$@"
work=$(mktemp -d)
tar -xzf "$archive" -C "$work"
bin=$(echo "$work"/*/starfold)
echo "ldd $bin, with only $* installed:"
ldd "$bin" >"$work/ldd.txt"
cat "$work/ldd.txt"
if grep -q 'not found' "$work/ldd.txt"; then
	echo "::error::a library starfold links is missing with only the runtime packages installed"
	exit 1
fi
"$here/check-version.sh" "$version" "$licence" "$bin"
apt install -y --no-install-recommends xvfb
Xvfb :99 -screen 0 1280x720x24 -nolisten tcp >/dev/null 2>&1 &
for _ in $(seq 40); do
	[ -S /tmp/.X11-unix/X99 ] && break
	sleep 0.25
done
DISPLAY=:99 RUST_LOG=info "$bin" --port=6901 --enable-basic-auth=false --audio-enabled=false >"$work/log" 2>&1 &
pid=$!
for _ in $(seq 60); do
	grep -q 'listening on' "$work/log" && break
	kill -0 "$pid" 2>/dev/null || break
	sleep 0.5
done
cut -c1-240 "$work/log"
ok=1
grep -q 'listening on' "$work/log" && kill -0 "$pid" 2>/dev/null || ok=0
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
if [ $ok != 1 ]; then
	echo "::error::starfold did not reach \"listening on\" on Xvfb"
	exit 1
fi
