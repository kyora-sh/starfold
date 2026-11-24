#!/usr/bin/env bash
# Usage: crates-index-test.sh
#
# Runs crates.sh's published() against a stand-in curl: a version the index lists is present
# (0), one it doesn't is absent (1), and so is a crate the index doesn't know (404); any other
# answer (curl failing, another status, an empty, HTML or truncated body) stops the release
# (exit 2), so no crate is published on a guess.
# shellcheck disable=SC2016
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT

failures=0
check() {
	if eval "$2"; then echo "ok   $1"; else
		echo "FAIL $1 (exit $rc)"
		failures=$((failures + 1))
	fi
}

# The function as crates.sh has it.
awk '/^published\(\) \{$/ { f = 1 } f { print } f && /^\}$/ { exit }' "$here/crates.sh" >"$root/published.sh"
[ -s "$root/published.sh" ] || {
	echo "FAIL no published() in crates.sh"
	exit 1
}

# The stand-in curl: writes $MOCK_BODY to its -o file, prints $MOCK_CODE (-w) and exits $MOCK_RC.
mkdir -p "$root/bin"
cat >"$root/bin/curl" <<'CURL'
#!/usr/bin/env bash
while [ $# -gt 0 ]; do
	case $1 in -o) out=$2; shift ;; esac
	shift
done
printf '%s' "$MOCK_BODY" >"$out"
printf '%s' "$MOCK_CODE"
exit "${MOCK_RC:-0}"
CURL
chmod +x "$root/bin/curl"

# ask <code> <body> [curl exit]: published starfold 0.1.0+libaom.3.15.3's exit status.
ask() {
	rc=$(
		export PATH="$root/bin:$PATH" MOCK_CODE=$1 MOCK_BODY=$2 MOCK_RC=${3:-0}
		# shellcheck disable=SC1091
		source "$root/published.sh"
		(published starfold 0.1.0+libaom.3.15.3 >/dev/null 2>&1)
		echo $?
	)
}
line() { printf '{"name":"starfold","vers":"%s","deps":[],"cksum":"00","features":{},"yanked":false}' "$1"; }
two="$(line 0.0.9)
$(line 0.1.0)"

ask 200 "$two"
check "a listed version is present" '[ "$rc" = 0 ]'
ask 200 "$(line 0.0.9)"
check "an unlisted version is absent" '[ "$rc" = 1 ]'
ask 404 ""
check "an unknown crate is absent" '[ "$rc" = 1 ]'
ask 200 ""
check "an empty body stops the release" '[ "$rc" -ge 2 ]'
ask 200 "<html><body>Service Unavailable</body></html>"
check "an HTML body stops the release" '[ "$rc" -ge 2 ]'
ask 200 "$(line 0.0.9)
{\"name\":\"starfold\",\"vers\":\"0.1"
check "a truncated body stops the release" '[ "$rc" -ge 2 ]'
ask 200 "$(line 0.0.9)" 56
check "curl failing after a 200 stops the release" '[ "$rc" -ge 2 ]'
ask 503 "unavailable"
check "another status stops the release" '[ "$rc" -ge 2 ]'

[ "$failures" = 0 ] || {
	echo "$failures failed"
	exit 1
}
echo "all passed"
