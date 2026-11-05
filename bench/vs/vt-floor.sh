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
  local i k
  for ((i = 0; i < n; i++)); do
    for ((k = 0; k < len; k++)); do
      IFS='|' read -r label test envs <<< "${arms[$(((k + i) % len))]}"
      local load=0 pids=() y
      case " $envs " in *" LOAD="*) load=${envs##*LOAD=}; load=${load%% *};; esac
      for ((y = 0; y < load; y++)); do yes > /dev/null & pids+=($!); done
      # shellcheck disable=SC2086
      if ! env VT_REPORT=1 $envs "$bin" "$test" --ignored --nocapture --test-threads=1 2>&1 | grep -oE '(bench|stages|vtcfg|vtkfi|pipeline) .*' | sed "s/^/set=$name cell=$label round=$i /"; then
        echo "FAILED set=$name cell=$label round=$i"
      fi
      if [ ${#pids[@]} -gt 0 ]; then kill "${pids[@]}"; wait "${pids[@]}" 2> /dev/null; fi
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
      for cfg in "base:" "off:VT_LL=0 VT_RT=0"; do
        for c in h264 hevc; do
          for r in 640x360 1280x720 1920x1080; do
            for f in 30 60 120; do
              arms+=("${cfg%%:*}-$c-${r}-f$f|cpu_per_stream|BACKEND=videotoolbox CODEC=$c W=${r%x*} H=${r#*x} FPS=$f ${cfg#*:}")
            done
          done
        done
      done
      run_set b "$rounds" "${arms[@]}" ;;
    p)
      for cfg in "base:" "off:VT_LL=0 VT_RT=0"; do
        for c in h264 hevc; do
          n=${cfg%%:*}-$c; e=${cfg#*:}
          arms+=("$n-free|cpu_per_stream|BACKEND=videotoolbox CODEC=$c FRAMES=150 $e")
          arms+=("$n-pace33|cpu_per_stream|BACKEND=videotoolbox CODEC=$c FRAMES=150 PACE_MS=33 $e")
          arms+=("$n-pace37|cpu_per_stream|BACKEND=videotoolbox CODEC=$c FRAMES=150 PACE_MS=37 $e")
          arms+=("$n-pipe|vt_pipeline|CODEC=$c $e")
        done
      done
      run_set p "$rounds" "${arms[@]}" ;;
    q)
      command -v ffmpeg > /dev/null || brew install ffmpeg > /dev/null 2>&1
      dump=$(mktemp -d)
      for ((i = 0; i < rounds; i++)); do
        qarms=()
        for c in h264 hevc; do
          for k in 30000 8000 2000; do
            for d in 30 60 120; do qarms+=("$c-decl$d-k$k $c $d $k"); done
          done
        done
        for ((a = 0; a < ${#qarms[@]}; a++)); do
          read -r label c d k <<< "${qarms[$(((a + i) % ${#qarms[@]}))]}"
          run_set q 1 "$label|cpu_per_stream|BACKEND=videotoolbox CODEC=$c FRAMES=150 PACE_MS=33 FPS=$d KBPS=$k DUMP=$dump"
          for w in typing scrolling video; do
            ffmpeg -hide_banner -nostats -loglevel error -y -f "$c" -framerate 30 -i "$dump/$w.bits" -f rawvideo -pix_fmt yuv420p -s 1920x1080 -framerate 30 -i "$dump/$w.yuv" -lavfi "[0:v]trim=start_frame=30,setpts=N,format=yuv420p,split=2[a0][a1];[1:v]trim=start_frame=30,setpts=N,format=yuv420p,split=2[b0][b1];[a0][b0]psnr=stats_file=$dump/p.log[o0];[a1][b1]ssim=stats_file=$dump/s.log[o1]" -map '[o0]' -f null - -map '[o1]' -f null - 2>&1 | tail -3
            awk -v tag="quality set=q cell=$label round=$i workload=$w" '{for(i=1;i<=NF;i++) if($i ~ /^psnr_y:/){v=substr($i,8); if(v=="inf")v=99; v+=0; s+=v; if(n==0||v<m)m=v; n++}} END{printf "%s frames=%d psnr_y_avg=%.3f psnr_y_min=%.3f", tag, n, (n?s/n:0), m}' "$dump/p.log"
            awk '{for(i=1;i<=NF;i++) if($i ~ /^All:/){v=substr($i,5)+0; s+=v; if(n==0||v<m)m=v; n++}} END{printf " ssim_avg=%.5f ssim_min=%.5f\n", (n?s/n:0), m}' "$dump/s.log"
            rm -f "$dump/$w.bits" "$dump/$w.yuv" "$dump/p.log" "$dump/s.log"
          done
        done
      done ;;
    z)
      OFF="VT_LL=0 VT_RT=0"
      for c in h264 hevc; do
        C="BACKEND=videotoolbox CODEC=$c FRAMES=150 PACE_MS=33"
        arms+=("$c-base|cpu_per_stream|$C")
        arms+=("$c-base-load1|cpu_per_stream|$C LOAD=1")
        arms+=("$c-base-load2|cpu_per_stream|$C LOAD=2")
        arms+=("$c-base-decl60|cpu_per_stream|$C FPS=60")
        arms+=("$c-base-decl120|cpu_per_stream|$C FPS=120")
        arms+=("$c-off|cpu_per_stream|$C $OFF")
        arms+=("$c-off-load2|cpu_per_stream|$C $OFF LOAD=2")
        arms+=("$c-off-load6|cpu_per_stream|$C $OFF LOAD=6")
        arms+=("$c-off-nokfi|cpu_per_stream|$C $OFF VT_KFI=0")
        arms+=("$c-off-nokfi-k8000|cpu_per_stream|$C $OFF VT_KFI=0 KBPS=8000")
        arms+=("$c-off-nokfi-k2000|cpu_per_stream|$C $OFF VT_KFI=0 KBPS=2000")
        arms+=("$c-base-k2000|cpu_per_stream|$C KBPS=2000")
      done
      run_set z "$rounds" "${arms[@]}" ;;
    r)
      for cfg in "base:" "off:VT_LL=0 VT_RT=0"; do
        for c in h264 hevc; do
          for k in 2000 8000 30000; do
            arms+=("${cfg%%:*}-$c-k$k|cpu_per_stream|BACKEND=videotoolbox CODEC=$c FRAMES=240 KBPS=$k ${cfg#*:}")
          done
        done
      done
      run_set r "$rounds" "${arms[@]}" ;;
    l)
      for load in 0 2 6; do
        for ((i = 0; i < rounds; i++)); do
          pids=()
          for ((y = 0; y < load; y++)); do yes > /dev/null & pids+=($!); done
          for c in h264 hevc; do
            run_set l 1 "$c-load$load|cpu_per_stream|BACKEND=videotoolbox CODEC=$c FRAMES=150"
          done
          [ ${#pids[@]} -eq 0 ] || kill "${pids[@]}"
          wait 2> /dev/null
        done
      done ;;
  esac
done
echo SETS_DONE
