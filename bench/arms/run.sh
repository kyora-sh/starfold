#!/usr/bin/env bash
# Encoder-level numbers per arm at 1080p30, no browser: encode ms, CPU and bits from the arm_bench
# test, SSIM and PSNR from ffmpeg decoding the real bitstream against the source pictures.
#   run.sh once <arm> <workload> <kbps> [preset] [crf]    one `bench` line + one `score` line
#   run.sh search <arm> <workload> <target_db> [preset] [crf]    lowest rate reaching the target
#   run.sh rounds <file> [n]    interleaved `once` lines
# Env: FRAMES (300), THREADS (4), METRIC, LO, HI, TOL (hi - lo stops under lo / TOL), DUMP_DIR, KEEP=<dir> for the per-frame logs. Needs ffmpeg and the arm's feature built:
# BIN=$(cargo test --release --features <features> --lib --no-run ... ), see bench/arms/README.
set -euo pipefail
bin=${BIN:?the test binary}
dump=${DUMP_DIR:-$(mktemp -d)}
frames=${FRAMES:-300}
here=$(dirname "$0")

format_of() {
	case $1 in
		libvpx-* | libaom | svt-av1) echo ivf ;;
		kvazaar | x265) echo hevc ;;
		*) echo h264 ;;
	esac
}

once() {
	local arm=$1 workload=$2 kbps=$3 preset=${4:-} crf=${5:-10} size=$((1920 * 1080 * 3 / 2))
	ARM=$arm WORKLOAD=$workload KBPS=$kbps PRESET=$preset CRF=$crf DUMP=$dump FRAMES=$frames THREADS=${THREADS:-4} \
		"$bin" arm_bench --ignored --nocapture --test-threads=1 | grep -o 'bench arm=.*'
	local bs="$dump/$arm-$workload.bin" src="$dump/$arm-$workload.yuv" units="$dump/$arm-$workload.units"
	local dec="$dump/decoded.yuv" held="$dump/held.yuv" decoded skipped
	ffmpeg -nostdin -v error -y -f "$(format_of "$arm")" -i "$bs" -fps_mode passthrough -f rawvideo -pix_fmt yuv420p "$dec"
	decoded=$(($(stat -c %s "$dec") / size))
	[[ $decoded == $(wc -l < "$units") ]] || { echo "$arm $workload: decoded $decoded of $(wc -l < "$units") units" >&2; return 1; }
	skipped=$(python3 -I "$here/score.py" "$dec" "$units" "$frames" "$size" "$held")
	ffmpeg -nostdin -v error -y -f rawvideo -pix_fmt yuv420p -s 1920x1080 -r 30 -i "$held" \
		-f rawvideo -pix_fmt yuv420p -s 1920x1080 -r 30 -i "$src" \
		-lavfi "[0:v]split[a1][a2];[1:v]split[b1][b2];[a1][b1]ssim=stats_file=$dump/ssim.log;[a2][b2]psnr=stats_file=$dump/psnr.log" \
		-f null - 2>&1 | head -5
	[[ -z ${KEEP:-} ]] || cp "$dump/ssim.log" "$units" "$KEEP/"
	awk -v arm="$arm" -v w="$workload" -v k="$kbps" -v p="${preset:-default}" -v sf="$dump/psnr.log" -v sk="$skipped" -v sha="$(sha256sum "$bs" | cut -c1-12)" '
		function db(s) { return s >= 1 ? 99 : -10 * log(1 - s) / log(10) }
		{ sub(".*All:", ""); split($0, a, " "); s[NR] = a[1]; t += a[1]; if (NR == 1 || a[1] < min) { min = a[1]; at = NR - 1 } }
		END {
			while ((getline line < sf) > 0) {
				n++; if (match(line, /psnr_avg:[0-9.a-z]+/)) { v = substr(line, RSTART + 9, RLENGTH - 9); q = (v == "inf" ? 100 : v); p_sum += q; if (n > NR - 30) pl += q }
			}
			tail = NR > 30 ? 30 : NR; for (i = NR - tail + 1; i <= NR; i++) l += s[i]
			printf "score arm=%s preset=%s workload=%s target_kbps=%s frames=%d %s ssim_db=%.2f ssim_min_db=%.2f min_at=%d ssim_last30_db=%.2f psnr_db=%.2f psnr_last30_db=%.2f sha=%s\n", arm, p, w, k, NR, sk, db(t / NR), db(min), at, db(l / tail), p_sum / n, pl / tail, sha
		}' "$dump/ssim.log"
	rm -f "$bs" "$src" "$units" "$dec" "$held" "$dump/ssim.log" "$dump/psnr.log"
}

# METRIC picks the score field to reach: ssim_db for motion, ssim_last30_db for settled text.
search() {
	local arm=$1 workload=$2 target=$3 preset=${4:-} crf=${5:-10} lo=${LO:-250} hi=${HI:-64000} best=none got=none
	local metric=${METRIC:-ssim_db} total=none
	while ((hi - lo > lo / ${TOL:-8})); do
		local mid=$(((lo + hi) / 2)) out db
		out=$(once "$arm" "$workload" "$mid" "$preset" "$crf")
		echo "$out"
		db=$(sed -n "s/.* $metric=\([0-9.]*\).*/\1/p" <<< "$out")
		if awk -v a="$db" -v b="$target" 'BEGIN { exit !(a >= b) }'; then
			best=$mid hi=$mid got=$db total=$(sed -n 's/.* total_kbps=\([0-9]*\).*/\1/p' <<< "$out")
		else
			lo=$mid
		fi
	done
	echo "matched arm=$arm workload=$workload preset=${preset:-default} metric=$metric target=$target kbps=$best total_kbps=$total got=$got"
}

# One `once` per line of <file> ("arm workload kbps preset crf", - for an empty preset), N rounds,
# each round starting one line later so no arm always runs first.
rounds() {
	local file=$1 n=${2:-5} r i count
	local -a lines
	mapfile -t lines < "$file"
	count=${#lines[@]}
	for ((r = 0; r < n; r++)); do
		for ((i = 0; i < count; i++)); do
			read -r arm workload kbps preset crf <<< "${lines[(i + r) % count]}"
			[[ $preset != - ]] || preset=
			echo "round $r"
			once "$arm" "$workload" "$kbps" "$preset" "$crf"
		done
	done
}

"$@"
