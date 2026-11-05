#!/usr/bin/env bash
# run.sh <lan|wan> <rounds> <window s> <results file>: one set on this VM. Env: CHROME_ZIP, NOVNC_TGZ, CONTROL_ROUNDS, QUALITY_ROUNDS, ARMS, WORKLOADS.
set -u
cd "$(dirname "$(readlink -f "$0")")/../.."
link=${1:-lan} rounds=${2:-5} win=${3:-10} out=${4:-/tmp/vs-results.jsonl}
source bench/vs/setup.sh
openssl rand -hex 16 > /tmp/vs-pw
"$CHROMIUM" --version; nproc; free -g | sed -n 2p; x11vnc -version 2>/dev/null | head -1; websockify --help 2>&1 | head -1; dpkg -s websockify 2>/dev/null | grep -i '^version'; dpkg -s x11vnc 2>/dev/null | grep -i '^version'; grep '"version"' /tmp/novnc/package.json
node bench/vs/agent.mjs target/release/starfold /tmp/vs-source /tmp/novnc /tmp/vs-pw 1700 "$link" bench/shaper.py > /tmp/vs-agent.log 2>&1 &
for _ in $(seq 50); do curl -sf 127.0.0.1:6940/ >/dev/null && break; sleep 0.2; done
LINK=$link ROUNDS=$rounds WIN=$win OUT=$out PW=/tmp/vs-pw node bench/vs/driver.mjs
code=$?
curl -s 127.0.0.1:6940/quit; echo; sleep 1
echo "LEFT: $(pgrep -c -f 'target/release/starfold|Xvfb|x11vnc|websockify|vs-source|/tmp/chrome/' || true) driver exit $code"
exit $code
