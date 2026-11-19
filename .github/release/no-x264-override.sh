#!/usr/bin/env bash
# no-x264-override.sh: fails when STARFOLD_X264_LIB is set. Only the macOS job links x264 through
# it (checked by x264-linked.sh there); the Linux and Windows jobs check the x264 their package
# manager installed, so an archive given this way would go unchecked.
set -euo pipefail
if [ -n "${STARFOLD_X264_LIB+set}" ]; then
	echo "::error::STARFOLD_X264_LIB is set ('${STARFOLD_X264_LIB}'): this job links the x264 it checks, from its package manager"
	exit 1
fi
echo "STARFOLD_X264_LIB is unset"
