#!/usr/bin/env bash
# VT floor experiment. Usage: vt-floor.sh "<sets>" <rounds> "<best env>"
set -uo pipefail
sets=$1; rounds=$2; best=${3:-}
bin=$(cargo test --release --locked --lib --no-run --color never 2>&1 | tee /dev/stderr | grep -oE 'target/release/deps/starfold-[0-9a-f]+' | head -1)
[ -n "$bin" ] || { echo "no test binary"; exit 1; }
sysctl -n machdep.cpu.brand_string hw.ncpu hw.memsize; sw_vers -productVersion
run_set() { # name rounds arm...   arm = label|test|env words
  local name=$1 n=$2; shift 2
  local arms=("$@") len=$#
  for ((i = 0; i < n; i++)); do
    for ((k = 0; k < len; k++)); do
      IFS='|' read -r label test envs <<< "${arms[$(((k + i) % len))]}"
      # shellcheck disable=SC2086
      if ! env VT_REPORT=1 $envs "$bin" "$test" --ignored --nocapture --test-threads=1 2>&1 | grep -oE '(bench|stages|vtcfg|pipeline) .*' | sed "s/^/set=$name cell=$label round=$i /"; then
        echo "FAILED set=$name cell=$label round=$i"
      fi
    done
  done
}
for s in $sets; do
  arms=()
  case $s in
    a)
      for c in h264 hevc; do
        arms+=("$c-baseline|cpu_per_stream|BACKEND=videotoolbox CODEC=$c")
        arms+=("$c-mfdc0|cpu_per_stream|BACKEND=videotoolbox CODEC=$c VT_MFDC=0")
        arms+=("$c-nofps|cpu_per_stream|BACKEND=videotoolbox CODEC=$c VT_NO_FPS=1")
        arms+=("$c-both|cpu_per_stream|BACKEND=videotoolbox CODEC=$c VT_MFDC=0 VT_NO_FPS=1")
      done
      arms+=("openh264|cpu_per_stream|BACKEND=openh264 CODEC=h264")
      run_set a "$rounds" "${arms[@]}" ;;
    b)
      for c in h264 hevc; do
        for r in 640x360 1280x720 1920x1080; do
          for f in 30 60 120; do
            arms+=("$c-${r}-f$f|cpu_per_stream|BACKEND=videotoolbox CODEC=$c W=${r%x*} H=${r#*x} FPS=$f $best")
          done
        done
      done
      run_set b "$rounds" "${arms[@]}" ;;
    p)
      for c in h264 hevc; do
        arms+=("$c-free|cpu_per_stream|BACKEND=videotoolbox CODEC=$c FRAMES=150 $best")
        arms+=("$c-pace33|cpu_per_stream|BACKEND=videotoolbox CODEC=$c FRAMES=150 PACE_MS=33 $best")
        arms+=("$c-pace37|cpu_per_stream|BACKEND=videotoolbox CODEC=$c FRAMES=150 PACE_MS=37 $best")
        arms+=("$c-pipe|vt_pipeline|CODEC=$c $best")
      done
      run_set p "$rounds" "${arms[@]}" ;;
    c)
      for c in h264 hevc; do
        arms+=("$c-base|cpu_per_stream|BACKEND=videotoolbox CODEC=$c FRAMES=90")
        arms+=("$c-llrc-off|cpu_per_stream|BACKEND=videotoolbox CODEC=$c FRAMES=90 VT_LL=0")
        arms+=("$c-rt-off|cpu_per_stream|BACKEND=videotoolbox CODEC=$c FRAMES=90 VT_RT=0")
        arms+=("$c-llrc-rt-off|cpu_per_stream|BACKEND=videotoolbox CODEC=$c FRAMES=90 VT_LL=0 VT_RT=0")
      done
      run_set c "$rounds" "${arms[@]}" ;;
  esac
done
echo SETS_DONE
