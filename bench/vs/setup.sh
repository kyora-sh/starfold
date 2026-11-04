#!/usr/bin/env bash
# VM setup: packages, release server, source, stock noVNC, Playwright, Chrome. Sourced by run.sh; expects CHROME_ZIP.
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -qq >/dev/null 2>&1
sudo apt-get install -y -qq nasm esbuild libyuv-dev libopus-dev libpulse-dev libxcb1-dev libx11-dev libxext-dev x11-xserver-utils unzip xvfb x11vnc websockify iproute2 ethtool >/dev/null 2>&1
cargo +1.93.0 build --release --locked 2>&1 | tail -1
cc -O2 bench/vs/source.c -lX11 -lXext -lm -o /tmp/vs-source || exit 1
V=$(node -e "import('./bench/vs/config.mjs').then((c) => console.log(c.NOVNC_VERSION))")
rm -rf /tmp/novnc; mkdir -p /tmp/novnc
(cd /tmp && npm pack -s "@novnc/novnc@$V" >/dev/null 2>&1 && tar -xzf "novnc-novnc-$V.tgz" -C /tmp/novnc --strip-components=1) || exit 1
cp bench/vs/novnc-viewer.html /tmp/novnc/
mkdir -p /tmp/pw && (cd /tmp/pw && npm i -s playwright-core >/dev/null 2>&1 && npx -y playwright-core install-deps chromium >/dev/null 2>&1)
unzip -qo "$CHROME_ZIP" -d /tmp/chrome
export CHROMIUM=$(find /tmp/chrome -name chrome-headless-shell -type f | head -1) PLAYWRIGHT_NODE_MODULES=/tmp/pw/node_modules/
