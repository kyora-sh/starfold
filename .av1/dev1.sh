#!/bin/bash
# dev1.sh: first build of starfold-aom-sys on a VM; updates Cargo.lock; measures; not a gate.
set -u
cd /work
o=/work/.av1-out; mkdir -p "$o"
s() { local n=$1; shift; local t=$(date +%s); bash -o pipefail -c "$*" >"$o/$n.log" 2>&1; echo "step $n exit=$? $(( $(date +%s) - t ))s"; }
s submodule 'git submodule sync && git submodule update --init --force --depth=1 && git -C crates/starfold-aom-sys/aom rev-parse HEAD'
tail -n 3 "$o/submodule.log"
s apt 'sudo timeout -k 10 180 env DEBIAN_FRONTEND=noninteractive apt-get -q update && sudo timeout -k 10 720 env DEBIAN_FRONTEND=noninteractive apt-get -q install -y libx264-dev=2:0.164.3108+git31e19f9-1 libclang-dev libyuv-dev libopus-dev libpulse-dev libxcb1-dev esbuild cmake nasm perl binutils'
s toolchain 'rustup toolchain install 1.93.0 --profile minimal -c clippy -c rustfmt'
export RUSTUP_TOOLCHAIN=1.93.0
s gitstate 'git log -1 --format=%H; git ls-files -s crates/starfold-aom-sys; ls crates/starfold-aom-sys/aom | head -3'
cat "$o/gitstate.log"
s lock 'cargo metadata --format-version 1 >/dev/null && cp Cargo.lock /work/.av1-out/Cargo.lock && git diff --stat Cargo.lock'
cat "$o/lock.log"
s fmt 'cargo fmt --check'
s sys-test 'cargo test -p starfold-aom-sys --locked'
grep -E "^test |test result|error" "$o/sys-test.log" | head -20
s sys-clippy 'cargo clippy -p starfold-aom-sys --all-targets --locked -- -D warnings'
s lib-facts 'a=$(find target/debug -name libstarfold_aom.a | head -1); ls -l "$a"; echo "objects $(ar t "$a" | wc -l)"; ar t "$a" | grep -iE "fast|vector|hwy|highway|webm|yuv|nonmax" ; echo "cxx symbols $(nm -C "$a" 2>/dev/null | grep -c " [TW] .*::")"; nm "$a" 2>/dev/null | grep -E " U (_Z|__cxa|operator)" | sort -u | head'
cat "$o/lib-facts.log"
s clippy-libaom 'cargo clippy --features libaom --all-targets --locked -- -D warnings'
s test-encoder 'cargo test --features libaom --locked --lib encoder'
grep -E "test result|FAILED|panicked" "$o/test-encoder.log" | head
echo dev1-done
