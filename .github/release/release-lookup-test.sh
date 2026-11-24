#!/usr/bin/env bash
# Usage: release-lookup-test.sh
#
# Runs release-lookup.sh against a stand-in gh: no release passes, an API error fails, an
# existing release fails unless RELEASE_RESUME=1, where it passes with a notice.
# shellcheck disable=SC2016
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT

failures=0
check() {
	if eval "$2"; then echo "ok   $1"; else
		echo "FAIL $1"
		sed 's/^/     /' "$root/out"
		failures=$((failures + 1))
	fi
}

# The stand-in gh answers `release view` with $MOCK_OUT on stderr (or stdout) and exits $MOCK_RC.
cat >"$root/gh" <<'GH'
#!/usr/bin/env bash
[ "$1 $2" = "release view" ] || exit 2
if [ "$MOCK_RC" = 0 ]; then echo "$MOCK_OUT"; else echo "$MOCK_OUT" >&2; fi
exit "$MOCK_RC"
GH
chmod +x "$root/gh"
run() { env GH="$root/gh" MOCK_RC="$1" MOCK_OUT="$2" RELEASE_RESUME="${3:-0}" "$here/release-lookup.sh" v0.0.1 >"$root/out" 2>&1; }

check "no release: passes" 'run 1 "release not found"'
check "no release, said in other words around it: passes" 'run 1 "gh: release not found (HTTP 404)"'
check "an API error: fails" '! run 1 "HTTP 502: Bad Gateway (https://api.github.com/graphql)" && grep -q "couldn.t look up" "$root/out"'
check "a release without RELEASE_RESUME: fails" '! run 0 "{\"tagName\":\"v0.0.1\"}" && grep -q "already has a release" "$root/out"'
check "a release with RELEASE_RESUME=1: passes with a notice" 'run 0 "{\"tagName\":\"v0.0.1\"}" 1 && grep -q "::notice::" "$root/out"'

[ "$failures" = 0 ] || {
	echo "$failures failed"
	exit 1
}
echo "all passed"
