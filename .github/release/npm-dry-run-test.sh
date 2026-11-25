#!/usr/bin/env bash
# Usage: npm-dry-run-test.sh
#
# Runs npm-dry-run.sh against a stand-in npm: a version npm lists already skips the dry run, one it
# doesn't runs `npm publish --dry-run` of the tarball, and a registry error fails without a dry run.
# Also checks that release.yml's npm dry run goes through npm-dry-run.sh.
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

# The stand-in npm answers `view <name> versions --json ...` with $MOCK_OUT and exits $MOCK_RC, and
# writes the arguments of `publish` to $root/published.
mkdir "$root/bin" "$root/work" "$root/work/package"
cat >"$root/bin/npm" <<'NPM'
#!/usr/bin/env bash
case "$1 $3 $4" in
"view versions --json")
	[ "$2" = @starfold/server ] || { echo "unexpected package: $2" >&2; exit 2; }
	printf '%s\n' "$MOCK_OUT"
	exit "$MOCK_RC"
	;;
esac
[ "$1" = publish ] || { echo "unexpected: npm $*" >&2; exit 2; }
echo "$*" >"$MOCK_PUBLISHED"
NPM
chmod +x "$root/bin/npm"
printf '{"name": "@starfold/server", "version": "0.4.0"}\n' >"$root/work/package/package.json"
tar -czf "$root/work/server.tgz" -C "$root/work" package

run() {
	rm -f "$root/published"
	(cd "$root/work" && env PATH="$root/bin:$PATH" MOCK_RC="$1" MOCK_OUT="$2" MOCK_PUBLISHED="$root/published" \
		"$here/npm-dry-run.sh" server.tgz 0.4.0) >"$root/out" 2>&1
}
says() { grep -qxF -- "$1" "$root/out"; }
published() { [ "$(cat "$root/published" 2>/dev/null)" = "$1" ]; }

check "npm lists the version: dry run skipped" \
	'run 0 "[\"0.0.0-stage\", \"0.4.0\"]" && says "@starfold/server@0.4.0 is on npm already: dry run skipped" && [ ! -e "$root/published" ]'
check "npm doesn't list the version: npm publish --dry-run of the tarball" \
	'run 0 "[\"0.0.0-stage\", \"0.3.2\"]" && published "publish ./server.tgz --access public --dry-run"'
check "a registry error: fails, no dry run" \
	'! run 1 "npm error code E404" && says "::error::npm-has.sh gave no answer for @starfold/server" && [ ! -e "$root/published" ]'
check "an answer that isn't a version list: fails, no dry run" \
	'! run 0 "{\"error\": \"x\"}" && [ ! -e "$root/published" ]'

dry_runs() { grep -v '^[[:space:]]*#' "$here/../workflows/release.yml" | grep -E 'npm publish .*--dry-run|npm-dry-run\.sh "' >"$root/out"; }
check "release.yml's npm dry run goes through npm-dry-run.sh only" \
	'dry_runs && [ "$(wc -l <"$root/out")" -eq 1 ] && grep -q "npm-dry-run\.sh" "$root/out"'

[ "$failures" = 0 ] || {
	echo "$failures failed"
	exit 1
}
echo "all passed"
