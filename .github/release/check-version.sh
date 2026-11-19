#!/usr/bin/env bash
# check-version.sh <version> <licence> <command...>: the command (a starfold binary, or a runner
# for one) prints `starfold <version> (encoders: ...; licence: <licence>)` for --version.
set -euo pipefail
version=${1:?usage: check-version.sh <version> <licence> <command...>}
licence=$2
shift 2
line=$("$@" --version | tr -d '\r')
echo "$line"
case $line in
"starfold $version (encoders: "*"; licence: $licence)") ;;
*)
	echo "::error::expected starfold $version with licence $licence, got: $line"
	exit 1
	;;
esac
