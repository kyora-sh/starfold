#!/usr/bin/env bash
# set.sh <lan|wan> <rounds> <run> <results file>: run <run> (0, 1, ...) of a set on this VM: CALIB before and after, then
# run.sh with the arm rotation carried on from the set's earlier runs. Env as for run.sh (ARMS, WORKLOADS, AA, ...).
# Each run of a set goes on a VM of its own; bench/vs/aa.py reads their logs.
set -u
cd "$(dirname "$(readlink -f "$0")")/../.." || exit 1
calib() { python3 -c 'import time
t = time.perf_counter(); s = 0
for i in range(40000000): s += i
print("CALIB %.3f" % (time.perf_counter() - t))'; }
calib
RUN=$3 bash bench/vs/run.sh "$1" "$2" 30 "$4"
code=$?
calib
exit $code
