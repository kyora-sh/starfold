#!/usr/bin/env bash
# Server-side per-codec numbers on a Mac at 1080p30: encode time, bits and this process's CPU, interleaved
# with rotating order. VideoToolbox's work happens outside the process, so its CPU is a lower bound.
# Usage: mac-encoders.sh <rounds>. Also says whether this host can capture its screen for a browser run.
set -euo pipefail
rounds=${1:-5}
# The OpenH264 arm measures what ships: Cisco's binary (the test binary would otherwise build it from source).
STARFOLD_OPENH264_LIB=${STARFOLD_OPENH264_LIB:-$(scripts/fetch-openh264.sh)}
export STARFOLD_OPENH264_LIB
bin=$(cargo test --release --locked --lib --no-run --color never 2>&1 | tee /dev/stderr | grep -oE 'target/release/deps/starfold-[0-9a-f]+' | head -1)
arms=("videotoolbox h264" "videotoolbox hevc" "openh264 h264")
for ((i = 0; i < rounds; i++)); do
	for ((k = 0; k < ${#arms[@]}; k++)); do
		read -r backend codec <<< "${arms[$(((k + i) % ${#arms[@]}))]}"
		BACKEND=$backend CODEC=$codec "$bin" cpu_per_stream --ignored --nocapture --test-threads=1 | grep -o 'bench .*' | sed "s/^/round=$i /"
	done
done
sysctl -n machdep.cpu.brand_string hw.ncpu
cargo build --release --locked --color never 2>&1 | tail -1
log=$(mktemp)
RUST_LOG=info target/release/starfold --addr=127.0.0.1 --port=6961 --enable-basic-auth=false --audio-enabled=false --encoder=auto > "$log" 2>&1 &
pid=$!
sleep 5
curl -s --max-time 3 127.0.0.1:6961/stats | head -c 300 || true
echo
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
grep -iE 'permission|screen recording|error' "$log" | cut -c1-200 | head -5 || true
rm -f "$log"
