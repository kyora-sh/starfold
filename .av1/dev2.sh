#!/bin/bash
# dev2.sh: the review fixes on a VM: tests, clippy, the CI link step, and each way the source
# check must stop a build (moved HEAD after a warm build, a changed file, no git metadata).
set -u
cd /work
o=/work/.av1-out; mkdir -p "$o"
s() { local n=$1; shift; local t=$(date +%s); bash -o pipefail -c "$*" >"$o/$n.log" 2>&1; echo "step $n exit=$? $(( $(date +%s) - t ))s"; }
s submodule 'git submodule update --init --force --depth=1 && git -C crates/starfold-aom-sys/aom rev-parse HEAD'
s apt 'sudo timeout -k 10 180 env DEBIAN_FRONTEND=noninteractive apt-get -q update && sudo timeout -k 10 720 env DEBIAN_FRONTEND=noninteractive apt-get -q install -y libx264-dev=2:0.164.3108+git31e19f9-1 libclang-dev libyuv-dev libopus-dev libpulse-dev libxcb1-dev esbuild cmake nasm perl binutils'
s toolchain 'rustup toolchain install 1.93.0 --profile minimal -c clippy -c rustfmt'
export RUSTUP_TOOLCHAIN=1.93.0
s fmt 'cargo fmt --check'
s lock 'cargo metadata --locked --format-version 1 >/dev/null'
s sys-test 'cargo test -p starfold-aom-sys --locked'
grep -E "^test |test result" "$o/sys-test.log" | head
s sys-clippy 'cargo clippy -p starfold-aom-sys --all-targets --locked -- -D warnings'
s clippy-libaom 'cargo clippy --features libaom --all-targets --locked -- -D warnings'
RUNNER_TEMP=/tmp/rt; mkdir -p $RUNNER_TEMP; export RUNNER_TEMP
s link-step '
cargo build --features libaom --locked
ldd target/debug/starfold > "$RUNNER_TEMP/ldd.txt"
if grep -i aom "$RUNNER_TEMP/ldd.txt"; then echo "::error::a shared libaom is linked"; exit 1; fi
nm target/debug/starfold > "$RUNNER_TEMP/nm.txt"
grep " T aom_codec_av1_cx$" "$RUNNER_TEMP/nm.txt"
for a in target/debug/build/starfold-aom-sys-*/out/lib/libstarfold_aom_*.a; do
  ar t "$a" > "$RUNNER_TEMP/objects.txt"
  grep -x "av1_cx_iface.c.o" "$RUNNER_TEMP/objects.txt"
  if grep -E "^(av1_dx_iface|decodeframe|decodemv|decoder)\.c\.o$" "$RUNNER_TEMP/objects.txt"; then echo "::error::$a has libaom decoder"; exit 1; fi
done
ls target/debug/build/starfold-aom-sys-*/out/lib/'
cat "$o/link-step.log" | tail -6
s encoder-tests 'cargo test --features libaom --locked --lib encoder'
grep -E "test result" "$o/encoder-tests.log"
# Negative paths, each expected to FAIL with its message.
A=crates/starfold-aom-sys/aom
s neg-moved-head "git -C $A -c user.name=t -c user.email=t@t commit -q --allow-empty -m moved && cargo build --features libaom --locked"
grep -E "error:|is at" "$o/neg-moved-head.log" | head -3
s restore-head "git -C $A reset -q --hard b141d9c45db236335351c8a4b2a14b3ade27c937 && cargo build --features libaom --locked"
s neg-dirty "echo '/* x */' >> $A/av1/av1_cx_iface.c && cargo build --features libaom --locked"
grep -E "error:|local changes| M " "$o/neg-dirty.log" | head -3
s restore-dirty "git -C $A checkout -q -- av1/av1_cx_iface.c && git -C $A status --porcelain"
s neg-nogit "mv $A/.git /tmp/aom-dotgit && cargo build --features libaom --locked"
grep -E "error:|no git metadata" "$o/neg-nogit.log" | head -3
s restore-nogit "mv /tmp/aom-dotgit $A/.git && cargo build --features libaom --locked"
s neg-cmake-old "printf '#!/bin/sh\necho cmake version 3.15.7\n' > /tmp/oldcmake && chmod +x /tmp/oldcmake && touch crates/starfold-aom-sys/build.rs && CMAKE=/tmp/oldcmake cargo build --features libaom --locked"
grep -E "error:|3.16" "$o/neg-cmake-old.log" | head -3
echo dev2-done
