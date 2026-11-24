#!/bin/bash
# dev3.sh: the config/ refusal on a VM: a stray config/aom_config.h must stop the build, and
# removing it must let the build through.
set -u
cd /work
o=/work/.av1-out; mkdir -p "$o"
s() { local n=$1; shift; local t=$(date +%s); bash -o pipefail -c "$*" >"$o/$n.log" 2>&1; echo "step $n exit=$? $(( $(date +%s) - t ))s"; }
s submodule 'git submodule update --init --force --depth=1'
s apt 'sudo timeout -k 10 180 env DEBIAN_FRONTEND=noninteractive apt-get -q update && sudo timeout -k 10 720 env DEBIAN_FRONTEND=noninteractive apt-get -q install -y libclang-dev cmake nasm perl'
s toolchain 'rustup toolchain install 1.93.0 --profile minimal'
export RUSTUP_TOOLCHAIN=1.93.0
A=crates/starfold-aom-sys/aom
s clean-build 'cargo build -p starfold-aom-sys --locked'
s neg-config "mkdir -p $A/config && echo '#define CONFIG_AV1_DECODER 1' > $A/config/aom_config.h && cargo build -p starfold-aom-sys --locked"
grep -E "error:|config/" "$o/neg-config.log" | head -4
s restore-config "rm -rf $A/config && cargo build -p starfold-aom-sys --locked"
echo dev3-done
