#!/usr/bin/env bash
# Server-side per-codec numbers on a Mac at 1080p30: encode time, bits and this process's CPU, interleaved
# with rotating order. VideoToolbox's work happens outside the process, so its CPU is a lower bound.
# Usage: mac-encoders.sh <rounds>. Also says whether this host could capture its screen for a browser run.
set -u
rounds=${1:-5}
cargo test --release --locked --lib --no-run 2>&1 | tail -1
bin=$(ls -t target/release/deps/starfold_server-* | grep -v '\.d$' | head -1)
arms=("videotoolbox h264" "videotoolbox hevc" "openh264 h264")
for ((i = 0; i < rounds; i++)); do
	for ((k = 0; k < ${#arms[@]}; k++)); do
		read -r backend codec <<< "${arms[$(((k + i) % ${#arms[@]}))]}"
		BACKEND=$backend CODEC=$codec "$bin" cpu_per_stream --ignored --nocapture --test-threads=1 | grep -o 'bench .*' | sed "s/^/round=$i /"
	done
done
echo "screen capture access: $(swift -e 'import CoreGraphics; print(CGPreflightScreenCaptureAccess())' 2>&1 | tail -1)"
screencapture -x /tmp/vs-probe.png 2>&1 | tail -1; ls -l /tmp/vs-probe.png 2>&1
system_profiler SPDisplaysDataType 2>/dev/null | grep -E 'Chipset|Resolution|Display Type' | head -5
