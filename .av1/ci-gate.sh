#!/bin/bash
# ci-gate.sh OUTDIR JOB...: CI's jobs on this VM through ci-steps.py (step text read from
# ci.yml). Like a CI checkout, the tree holds the tracked files only: the clone's untracked
# files and the VM's HOME (.home inside the checkout) are moved out first.
set -u
cd /work
out=$1; shift
sudo timeout -k 10 300 env DEBIAN_FRONTEND=noninteractive apt-get -q -o Acquire::Retries=3 install -y python3-yaml 1>/tmp/yaml.log 2>&1 \
  || { sudo timeout -k 10 180 env DEBIAN_FRONTEND=noninteractive apt-get -q update 1>/dev/null 2>&1; sudo timeout -k 10 300 env DEBIAN_FRONTEND=noninteractive apt-get -q install -y python3-yaml 1>>/tmp/yaml.log 2>&1; }
mkdir -p /tmp/gate /tmp/untracked "/tmp/gate-out"
cp .av1/ci-steps.py /tmp/gate/
git ls-files --others --exclude-standard --directory | while read -r f; do
  [ "$f" = .home/ ] && continue
  mkdir -p "/tmp/untracked/$(dirname "$f")" && mv "$f" "/tmp/untracked/$f"
done
if [ -d .home ] && [ "$HOME" = "$PWD/.home" ]; then
  mv .home /tmp/vmhome
  export HOME=/tmp/vmhome
  export PATH="/tmp/vmhome/.cargo/bin:/tmp/vmhome/.bun/bin:$PATH"
  export CARGO_HOME=/tmp/vmhome/.cargo RUSTUP_HOME=/tmp/vmhome/.rustup
fi
export BUN_INSTALL=$HOME/.bun BUN_INSTALL_CACHE_DIR=$HOME/.bun/install/cache
git status --short --ignored | grep -v '^!! \(target\|node_modules\)' 1>/tmp/gate/tree.txt
python3 -I /tmp/gate/ci-steps.py /tmp/gate-out "$@"
cp /tmp/gate/tree.txt /tmp/gate-out/tree.txt
mkdir -p "/work/$out" && cp -r /tmp/gate-out/. "/work/$out/"
cat "/work/$out/status"
