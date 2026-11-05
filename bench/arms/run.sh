#!/usr/bin/env bash
# Encoder-level numbers per arm at 1080p30, no browser: encode ms, CPU and bits from the arm_bench
# test, SSIM and PSNR from ffmpeg decoding the real bitstream against the source pictures.
#   run.sh once <arm> <workload> <kbps> [preset] [crf]    one `bench` line + one `score` line
#   run.sh search <arm> <workload> <target_ssim_db> [preset] [crf]    lowest rate reaching the target
# Env: FRAMES (300), THREADS (4), ROUNDS, DUMP_DIR. Needs ffmpeg and the arm's feature built:
# BIN=$(cargo test --release --features <features> --lib --no-run ... ), see bench/arms/README.
set -euo pipefail
bin=${BIN:?the test binary}
dump=${DUMP_DIR:-$(mktemp -d)}
frames=${FRAMES:-300}

format_of() {
	case $1 in
		libvpx-*) echo ivf ;;
		libaom | svt-av1) echo obu ;;
		kvazaar | x265) echo hevc ;;
		*) echo h264 ;;
	esac
}

once() {
	local arm=$1 workload=$2 kbps=$3 preset=${4:-} crf=${5:-10}
	ARM=$arm WORKLOAD=$workload KBPS=$kbps PRESET=$preset CRF=$crf DUMP=$dump FRAMES=$frames THREADS=${THREADS:-4} \
		"$bin" arm_bench --ignored --nocapture --test-threads=1 | grep -o 'bench .*'
	local bs="$dump/$arm-$workload.bin" src="$dump/$arm-$workload.yuv"
	ffmpeg -nostdin -v error -y -f "$(format_of "$arm")" -i "$bs" \
		-f rawvideo -pix_fmt yuv420p -s 1920x1080 -r 30 -i "$src" \
		-lavfi "[0:v]setpts=N/30/TB[a0];[1:v]setpts=N/30/TB[b0];[a0]split[a1][a2];[b0]split[b1][b2];[a1][b1]ssim=stats_file=$dump/ssim.log;[a2][b2]psnr=stats_file=$dump/psnr.log" \
		-f null - 2>&1 | head -5
	awk -v arm="$arm" -v w="$workload" -v k="$kbps" -v sf="$dump/psnr.log" '
		function db(s) { return s >= 1 ? 99 : -10 * log(1 - s) / log(10) }
		{ sub(".*All:", ""); split($0, a, " "); s[NR] = a[1]; t += a[1]; if (NR == 1 || a[1] < min) min = a[1] }
		END {
			while ((getline line < sf) > 0) {
				n++; if (match(line, /psnr_avg:[0-9.a-z]+/)) { v = substr(line, RSTART + 9, RLENGTH - 9); p += (v == "inf" ? 100 : v) }
			}
			tail = NR > 30 ? 30 : NR; for (i = NR - tail + 1; i <= NR; i++) l += s[i]
			printf "score arm=%s workload=%s target_kbps=%s frames=%d ssim_db=%.2f ssim_min_db=%.2f ssim_last30_db=%.2f psnr_db=%.2f\n", arm, w, k, NR, db(t / NR), db(min), db(l / tail), p / n
		}' "$dump/ssim.log"
	rm -f "$bs" "$src" "$dump/ssim.log" "$dump/psnr.log"
}

search() {
	local arm=$1 workload=$2 target=$3 preset=${4:-} crf=${5:-10} lo=250 hi=64000 best=none
	while ((hi - lo > lo / 8)); do
		local mid=$(((lo + hi) / 2)) out db
		out=$(once "$arm" "$workload" "$mid" "$preset" "$crf")
		echo "$out"
		db=$(sed -n 's/.* ssim_db=\([0-9.]*\).*/\1/p' <<< "$out")
		if awk -v a="$db" -v b="$target" 'BEGIN { exit !(a >= b) }'; then
			best=$mid hi=$mid
		else
			lo=$mid
		fi
	done
	echo "matched arm=$arm workload=$workload preset=${preset:-default} target_ssim_db=$target kbps=$best"
}

"$@"
