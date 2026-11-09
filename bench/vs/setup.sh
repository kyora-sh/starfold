#!/usr/bin/env bash
# VM setup: packages, release server, source, stock noVNC, Playwright, Chrome. Sourced by run.sh; expects
# CHROME_ZIP and NOVNC_TGZ (noVNC's source tag tarball: the npm package's lib/ has a top-level await in CommonJS).
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -qq >/dev/null 2>&1
sudo apt-get install -y -qq nasm esbuild libyuv-dev libopus-dev libpulse-dev libxcb1-dev libx11-dev libxext-dev x11-xserver-utils unzip xvfb x11vnc websockify iproute2 x11-apps xfonts-base python3-numpy python3-pil >/dev/null 2>&1
build() { "$@" > /tmp/vs-build.log 2>&1; local c=$?; tail -3 /tmp/vs-build.log; return $c; }
build cargo +1.93.0 build --release --locked || { echo 'server did not build'; exit 1; }
cc -O2 bench/vs/source.c -lX11 -lXext -lm -o /tmp/vs-source || { echo 'source did not build'; exit 1; }
rm -rf /tmp/novnc; mkdir -p /tmp/novnc
tar -xzf "$NOVNC_TGZ" -C /tmp/novnc --strip-components=1 || { echo "bad NOVNC_TGZ $NOVNC_TGZ"; exit 1; }
grep -q "\"version\": \"$(node -e "import('./bench/vs/config.mjs').then((c) => console.log(c.NOVNC_VERSION))")\"" /tmp/novnc/package.json || { echo 'wrong noVNC version'; exit 1; }
cp bench/vs/novnc-viewer.html bench/vs/kyora/kyora-viewer.html bench/vs/kyora/vms-viewer.js /tmp/novnc/
mkdir -p /tmp/pw && (cd /tmp/pw && npm i -s playwright-core >/dev/null 2>&1 && npx -y playwright-core install-deps chromium >/dev/null 2>&1)
unzip -qo "$CHROME_ZIP" -d /tmp/chrome
CHROMIUM=$(find /tmp/chrome -name chrome-headless-shell -type f | head -1)
export CHROMIUM PLAYWRIGHT_NODE_MODULES=/tmp/pw/node_modules/
node bench/vs/page.mjs /tmp/vs-page.png && python3 -c "
from PIL import Image
im = Image.open('/tmp/vs-page.png').convert('RGB')
page = Image.new('RGB', (1920, 7080), 'white')
page.paste(im.crop((0, 0, 1920, 7080)), (0, 0))
r, g, b = page.split()
open('/tmp/vs-page.raw', 'wb').write(Image.merge('RGBA', (b, g, r, Image.new('L', page.size, 255))).tobytes())
" || { echo 'no page'; exit 1; }
if [ -f "${BEFORE_TGZ:-}" ]; then
	mkdir -p /tmp/before && tar -xzf "$BEFORE_TGZ" -C /tmp/before
	(cd /tmp/before && CARGO_TARGET_DIR=/tmp/t-before build cargo +1.93.0 build --release --locked) || { echo 'before did not build'; exit 1; }
	export STARFOLD_BEFORE_BIN=/tmp/t-before/release/starfold
fi
if [ "${PROBE_BUILDS:-0}" = 1 ]; then
	sudo apt-get install -y -qq libx264-dev libclang-dev >/dev/null 2>&1
	CARGO_TARGET_DIR=/tmp/t-x264 build cargo +1.93.0 build --release --locked --features x264 || { echo 'x264 did not build'; exit 1; }
	export STARFOLD_X264_BIN=/tmp/t-x264/release/starfold
	if [ -f "${VOXL_TGZ:-}" ]; then
		mkdir -p /tmp/voxl && tar -xzf "$VOXL_TGZ" -C /tmp/voxl
		(cd /tmp/voxl && build cargo +1.93.0 build --release --locked) || { echo 'upstream did not build'; exit 1; }
		export VOXL_BIN=/tmp/voxl/target/release/voxl
	fi
fi
