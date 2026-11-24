#!/usr/bin/env bash
# Usage: npm-has-test.sh
#
# Runs npm-has.sh against a stand-in npm: a package with only a prerelease (npm view prints a bare
# string for one version, a list for more) hasn't the release, one that lists it has; a registry
# error and an answer that isn't a version list fail. Also checks that release.yml publishes to npm
# without --tag, so `latest` moves to the release.
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

# The stand-in npm answers `view <name> versions --json ...` with $MOCK_OUT and exits $MOCK_RC.
mkdir "$root/bin"
cat >"$root/bin/npm" <<'NPM'
#!/usr/bin/env bash
[ "$1 $3 $4" = "view versions --json" ] || { echo "unexpected: npm $*" >&2; exit 2; }
printf '%s\n' "$MOCK_OUT"
exit "$MOCK_RC"
NPM
chmod +x "$root/bin/npm"
run() { env PATH="$root/bin:$PATH" MOCK_RC="$1" MOCK_OUT="$2" "$here/npm-has.sh" @starfold/server "${3:-0.4.0}" >"$root/out" 2>&1; }
says() { [ "$(cat "$root/out")" = "$1" ]; }

check "only 0.0.0-stage, as a bare string: absent" 'run 0 "\"0.0.0-stage\"" && says absent'
check "only 0.0.0-stage, as a one-element list: absent" 'run 0 "[\"0.0.0-stage\"]" && says absent'
check "0.0.0-stage and the release: present" 'run 0 "[\"0.0.0-stage\", \"0.4.0\"]" && says present'
check "the release alone, as a bare string: present" 'run 0 "\"0.4.0\"" && says present'
check "a prerelease of the release isn't the release: absent" 'run 0 "[\"0.4.0-rc.1\"]" && says absent'
check "a registry error: fails" '! run 1 "npm error code E404"'
check "an empty answer: fails" '! run 0 ""'
check "an empty list: fails" '! run 0 "[]"'
check "an object: fails" '! run 0 "{\"error\": \"x\"}"'
check "not a version: fails" '! run 0 "[\"latest\"]"'

publishes() { grep -E '^\s*(absent\) )?npm publish ' "$here/../workflows/release.yml" >"$root/out"; }
check "release.yml publishes to npm" 'publishes'
check "release.yml's npm publish takes no --tag" 'publishes && ! grep -q -e --tag "$root/out"'

[ "$failures" = 0 ] || {
	echo "$failures failed"
	exit 1
}
echo "all passed"
