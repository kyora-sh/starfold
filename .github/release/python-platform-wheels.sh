#!/usr/bin/env bash
# python-platform-wheels.sh <version> <artifacts dir> <pure wheel> <out dir>
# The Python SDK's platform wheels: the pure wheel (starfold-<version>-py3-none-any.whl) with the server
# binary of one of the four default release archives each (python-platform-wheel.py says what goes in).
# An archive is <artifacts dir>/starfold-<version>-<target>/starfold-<version>-<target>.<tar.gz|zip>
# (as the release's artifacts download) or directly in <artifacts dir>, with its .sha256 beside it,
# which is checked first. LINUX_WHEELS=0 leaves out the Linux wheels (default 1).
# Each wheel is read back: its binary's sha256 against the archive's, RECORD against its files; then
# its size, and for a Linux wheel `auditwheel show` (printed, not judged).
set -euo pipefail
version=${1:?usage: python-platform-wheels.sh <version> <artifacts dir> <pure wheel> <out dir>}
artifacts=${2:?artifacts dir} pure=${3:?pure wheel} out=${4:?out dir}
AUDITWHEEL=auditwheel==6.8.2
here=$(cd "$(dirname "$0")" && pwd)
fail() {
	echo "::error::python-platform-wheels.sh: $*"
	exit 1
}
case ${LINUX_WHEELS:-1} in 0 | 1) ;; *) fail "LINUX_WHEELS is 0 or 1, not ${LINUX_WHEELS}" ;; esac
[ "$(basename "$pure")" = "starfold-$version-py3-none-any.whl" ] || fail "$pure isn't starfold-$version-py3-none-any.whl"
[ -f "$pure" ] || fail "no $pure"
mkdir -p "$out"
sha256() { if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }

targets=(aarch64-macos x86_64-windows)
[ "${LINUX_WHEELS:-1}" = 1 ] && targets=(x86_64-linux aarch64-linux "${targets[@]}")
for target in "${targets[@]}"; do
	case $target in *-windows) ext=zip ;; *) ext=tar.gz ;; esac
	name=starfold-$version-$target.$ext
	archive=$artifacts/starfold-$version-$target/$name
	[ -f "$archive" ] || archive=$artifacts/$name
	if [ ! -f "$archive" ] || [ ! -f "$archive.sha256" ]; then fail "no $name with its .sha256 in $artifacts"; fi
	(cd "$(dirname "$archive")" && sha256 -c "$name.sha256") || fail "$name doesn't match its .sha256"
	wheel=$(python3 -I "$here/python-platform-wheel.py" build "$version" "$target" "$archive" "$pure" "$out")
	python3 -I "$here/python-platform-wheel.py" verify "$version" "$target" "$archive" "$wheel"
	echo "$(basename "$wheel"): $(wc -c <"$wheel" | tr -d ' ') bytes"
	if [ "$target" != "${target%-linux}" ]; then
		if [ "$(uname -s)" = Linux ] && command -v uvx >/dev/null; then
			uvx --quiet "$AUDITWHEEL" show "$wheel" || echo "auditwheel show exited with $? for $(basename "$wheel")"
		else
			echo "auditwheel show: skipped for $(basename "$wheel") (needs Linux and uvx)"
		fi
	fi
done
(cd "$out" && for w in starfold-"$version"-py3-none-*.whl; do sha256 "$w" >"$w.sha256"; done)
ls -l "$out"
