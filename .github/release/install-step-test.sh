#!/usr/bin/env bash
# Usage: install-step-test.sh
#
# Runs the install job's pip step, as release.yml has it, against a stand-in uv, with the
# LINUX_WHEELS switch in both positions on Linux: with Linux wheels starfold-server's platform wheel
# installs with the starfold wheel and its `starfold --version` is checked; without, starfold-server's
# pure wheel installs and the binary check is skipped, saying so; and with the switch on but no Linux
# wheel the step fails.
# shellcheck disable=SC2016
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
workflow=$here/../workflows/release.yml
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

# step <name>: the run script of the install job's step of that name, unindented.
step() {
	awk -v name="$1" '
		/^  install:$/ { job = 1; next }
		job && /^  [a-z][a-z0-9-]*:$/ { job = 0 }
		job && $0 ~ "^ *- name: " name "$" { found = 1; next }
		found && /^ *run: \|$/ { match($0, /^ */); ind = RLENGTH + 2; run = 1; next }
		run {
			if ($0 ~ /^ *$/) { print ""; next }
			match($0, /^ */)
			if (RLENGTH < ind) exit
			print substr($0, ind + 1)
		}
	' "$workflow"
}
step 'pip install, starfold --version' >"$root/step.sh"
[ -s "$root/step.sh" ] || {
	echo "FAIL no pip step found in release.yml"
	exit 1
}

# The stand-in uv: `pip install` of starfold-server==<v> from --find-links takes the wheel with
# $MOCK_TAG's platform tag, or fails as uv does when there is none; a .whl path installs that
# file. An installed platform wheel brings venv/bin/starfold.
mkdir -p "$root/bin"
cat >"$root/bin/uv" <<'UV'
#!/usr/bin/env bash
set -euo pipefail
echo "uv $*" >>"$MOCK_LOG"
case "$1 ${2:-}" in
"venv "*) mkdir -p "${@: -1}/bin" ;;
"pip list") echo "starfold (mock)" ;;
"pip install")
	shift 2
	links="" specs=()
	while [ $# -gt 0 ]; do
		case $1 in
		--python) venv=$2; shift ;;
		--find-links) links=$2; shift ;;
		--*) ;;
		*) specs+=("$1") ;;
		esac
		shift
	done
	for spec in "${specs[@]}"; do
		case $spec in
		*.whl)
			[ -f "$spec" ] || { echo "no such file: $spec" >&2; exit 1; }
			echo "installed $(basename "$spec")"
			;;
		starfold-server==*)
			v=${spec#starfold-server==}
			wheel=$links/starfold_server-$v-py3-none-$MOCK_TAG.whl
			[ -n "$links" ] && [ -f "$wheel" ] || {
				echo "error: No solution found when resolving dependencies" >&2
				exit 1
			}
			printf '#!/bin/sh\necho "starfold %s (mock)"\n' "$v" >"$venv/bin/starfold"
			chmod +x "$venv/bin/starfold"
			echo "installed $(basename "$wheel")"
			;;
		*) echo "uv mock: unexpected requirement $spec" >&2; exit 2 ;;
		esac
	done
	;;
*) echo "uv mock: $*" >&2; exit 2 ;;
esac
UV
chmod +x "$root/bin/uv"

# workdir <wheel tags...>: the install job's downloads: wheels/ with those platform wheels,
# python/ with the client's wheel and starfold-server's pure wheel.
workdir() {
	rm -rf "$root/w"
	mkdir -p "$root/w/wheels" "$root/w/python"
	for t in "$@"; do touch "$root/w/wheels/starfold_server-0.0.1-py3-none-$t.whl"; done
	touch "$root/w/python/starfold-0.0.1-py3-none-any.whl" "$root/w/python/starfold_server-0.0.1-py3-none-any.whl"
}
run() {
	(cd "$root/w" && env PATH="$root/bin:$PATH" MOCK_LOG="$root/uv.log" MOCK_TAG=manylinux_2_39_x86_64 \
		VERSION=0.0.1 RUNNER_OS=Linux LINUX_WHEELS="$1" bash -e "$root/step.sh") >"$root/out" 2>&1
}

workdir manylinux_2_39_x86_64 macosx_11_0_arm64 win_amd64
check "LINUX_WHEELS=1: the platform wheel installs with the client and starfold --version names the version" \
	'run 1 && grep -q "starfold 0.0.1 (mock)" "$root/out" && grep -q "installed starfold_server-0.0.1-py3-none-manylinux_2_39_x86_64.whl" "$root/out" && grep -q "installed starfold-0.0.1-py3-none-any.whl" "$root/out"'

workdir macosx_11_0_arm64 win_amd64
check "LINUX_WHEELS=0: the pure wheels install and the binary check is skipped, saying so" \
	'run 0 && grep -q -F "LINUX_WHEELS=0: no Linux platform wheel; the pure wheel installed, binary check skipped" "$root/out" && grep -q "installed starfold_server-0.0.1-py3-none-any.whl" "$root/out" && grep -q "installed starfold-0.0.1-py3-none-any.whl" "$root/out"'

workdir macosx_11_0_arm64 win_amd64
check "LINUX_WHEELS=1 without a Linux wheel fails" '! run 1'

[ "$failures" = 0 ] || {
	echo "$failures failed"
	exit 1
}
echo "all passed"
