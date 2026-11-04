// Codec negotiation end to end: a starfold with VOXL_TEST_HOOKS on its own Xvfb, the
// real page in Chrome negotiating protocol 2, a switch forced through the test hook,
// a resize through the same epoch transition, and a protocol 1 client decoding beside
// it. Fails on any decoder error.
// Usage: node negotiation.mjs   (XVFB, VOXL_BIN, CHROMIUM, PLAYWRIGHT_NODE_MODULES)
import { createRequire } from 'module';
import { spawn } from 'child_process';
import fs from 'fs';
import path from 'path';
const require = createRequire(process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/');
let pw;
try {
	pw = require('playwright');
} catch {
	pw = require('playwright-core');
}
const { chromium } = pw;

const HERE = new URL('.', import.meta.url).pathname;
const bin = process.env.VOXL_BIN || path.join(HERE, '../../target/release/starfold');
const DISPLAY = process.env.NEGOTIATION_DISPLAY || ':9';
const port = process.env.VOXL_PORT || '6909';
const base = `http://127.0.0.1:${port}`;
const log = '/tmp/voxl-negotiation.log';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failed = 0;
const check = (ok, what) => {
	console.log(`${ok ? 'PASS' : 'FAIL'} ${what}`);
	if (!ok) failed++;
};
const until = async (f, ms = 15000) => {
	for (const t = Date.now(); Date.now() - t < ms; await sleep(200)) {
		const v = await f();
		if (v) return v;
	}
	return null;
};

// Each in its own process group, killed as a group at the end.
const group = (cmd, args, opts) => spawn(cmd, args, { detached: true, ...opts });
const procs = [];
procs.push(group(process.env.XVFB || 'Xvfb', [DISPLAY, '-screen', '0', '2560x1600x24', '-dpi', '144', '-shmem', '-ac', '-nolisten', 'tcp'], { stdio: 'ignore' }));
await sleep(1000);
// Something always moving, so frames keep coming.
procs.push(group('sh', ['-c', 'while :; do xsetroot -solid "#$(od -An -N3 -tx1 /dev/urandom | tr -d " ")"; sleep 0.04; done'], { env: { ...process.env, DISPLAY }, stdio: 'ignore' }));
const out = fs.openSync(log, 'w');
procs.push(group(bin, [`--port=${port}`, '--audio-enabled=false', '--enable-basic-auth=false', '--enable-resize=true', '--framerate=30'], { env: { ...process.env, DISPLAY, VOXL_TEST_HOOKS: '1', RUST_LOG: 'info' }, stdio: ['ignore', out, out] }));

const stats = async () => (await fetch(`${base}/stats`)).json();
const viewer = async (protocol) => (await stats()).clients.find((c) => c.video && c.stream.protocol === protocol);
let b;
try {
	for (let i = 0; i < 100; i++) {
		try {
			if ((await fetch(`${base}/healthz`)).ok) break;
		} catch {}
		await sleep(100);
	}
	b = await chromium.launch({ executablePath: process.env.CHROMIUM || 'chromium', args: ['--no-sandbox'] });
	const p = await b.newPage({ viewport: { width: 1280, height: 800 } });
	const errors = [];
	p.on('console', (m) => {
		const t = m.text();
		if (/decoder error|configure failed|decode threw|no video/.test(t)) errors.push(t);
		if (/\[voxl\]/.test(t)) console.log('[page]', t);
	});
	p.on('pageerror', (e) => errors.push(String(e)));
	await p.goto(`${base}/?stats`, { waitUntil: 'commit' });
	await p.waitForFunction(() => window.starfoldFirstFrame !== undefined, null, { timeout: 20000 });
	// Frames the page presented (decoded frames written to its <video> stripes, or the
	// worker's own count when it paints the canvas), and the worker's latest report.
	const presented = () =>
		p.evaluate(() => {
			const v = [...document.querySelectorAll('video.stripe')];
			return v.length ? v.reduce((n, x) => n + x.getVideoPlaybackQuality().totalVideoFrames, 0) : null;
		});
	const report = () => p.evaluate(() => window.voxlStats || {});
	const decoding = async (what, width) => {
		await sleep(1500);
		const n0 = await presented();
		await sleep(2000);
		const n1 = await presented();
		const r = await report();
		const vw = await p.evaluate(() => document.querySelector('video.stripe')?.videoWidth ?? null);
		const frames = n0 === null ? Math.round(r.fps * 2) : n1 - n0;
		check(frames >= 20 && r.dropped === 0 && (!width || vw === null || vw === width), `${what}: ${frames} frames decoded in 2 s, ${r.fps?.toFixed(1)} fps, ${r.dropped} dropped, video ${vw ?? 'canvas'} wide`);
		return r;
	};
	const ready = (after) => until(async () => {
		const v = await viewer(2);
		return v && v.stream.phase === 'ready' && v.stream.epoch > after ? v : null;
	});

	// 1) Negotiated: protocol 2, H.264 (Linux has no hardware encoder), at the size the
	// window asked for (the startup size, then the resize, each its own epoch).
	const v1 = await until(async () => {
		const v = await viewer(2);
		const s = await stats();
		const coded = s.cohorts.find((c) => c.id === v?.stream.cohort)?.stripes[0]?.config;
		return v && v.stream.phase === 'ready' && coded && coded.codedWidth === 1920 ? { v, coded } : null;
	}, 20000);
	check(!!v1 && v1.v.stream.codec === 'h264', `negotiated ${v1 ? `${v1.v.stream.codec} ${v1.coded.codec} ${v1.coded.codedWidth}x${v1.coded.codedHeight} epoch ${v1.v.stream.epoch}` : 'nothing'}`);
	const r1 = await decoding('after negotiation', 1920);
	check(r1.codec === v1?.coded.codec, `worker decoders configured with ${r1.codec}`);
	const enc = (await stats()).encoder;
	const h264 = enc?.chosen?.find((c) => c.codec === 'h264');
	check(enc?.mode === 'auto' && h264?.backend === 'openh264', `--encoder ${enc?.mode}: H.264 from ${h264?.backend}`);
	check(r1.encoder === 'openh264 (sw)', `stats overlay encoder ${r1.encoder}`);
	const e1 = v1 ? v1.v.stream.epoch : 0;

	// 2) A forced switch: a new epoch, a keyframe, frames keep decoding.
	const k0 = (await stats()).keyframes;
	const r = await (await fetch(`${base}/debug/renegotiate`, { method: 'POST' })).json();
	check(r.renegotiated === 1, `test hook renegotiated ${r.renegotiated} viewer(s)`);
	const v2 = await ready(e1);
	check(!!v2, `switch: epoch ${e1} -> ${v2?.stream.epoch}`);
	const k1 = await until(async () => {
		const k = (await stats()).keyframes;
		return k > k0 ? k : null;
	}, 5000);
	check(!!k1, `keyframe on the switch (${k0} -> ${k1})`);
	await decoding('after the switch', 1920);

	// 3) A resize goes through the same transition: new offers, a new epoch at the new size.
	const e2 = v2 ? v2.stream.epoch : e1;
	await p.setViewportSize({ width: 960, height: 600 });
	const v3 = await until(async () => {
		const v = await viewer(2);
		const s = await stats();
		const coded = s.cohorts.find((c) => c.id === v?.stream.cohort)?.stripes[0]?.config;
		return v && v.stream.phase === 'ready' && v.stream.epoch > e2 && coded?.codedWidth === 1440 ? { v, coded } : null;
	}, 20000);
	check(!!v3, `resize: epoch ${e2} -> ${v3?.v.stream.epoch} at ${v3 ? `${v3.coded.codedWidth}x${v3.coded.codedHeight}` : '?'}`);
	await decoding('after the resize', 1440);

	// 4) A protocol 1 client (as the old worker: `v,1` first, codec from the SPS, `a,<id>`
	// acks) beside it, decoding with WebCodecs.
	const old = await b.newPage();
	await old.goto(`${base}/healthz`);
	const legacy = await old.evaluate(async (url) => {
		const r = { frames: 0, keys: 0, errors: [], v2: 0, codec: null, hellos: 0, codecMsgs: 0 };
		const ws = new WebSocket(url);
		ws.binaryType = 'arraybuffer';
		let dec = null;
		let ts = 0;
		const hex2 = (x) => x.toString(16).padStart(2, '0');
		const sps = (d) => {
			for (let i = 0; i + 6 < d.length; i++)
				if (d[i] === 0 && d[i + 1] === 0 && d[i + 2] === 1 && (d[i + 3] & 0x1f) === 7) return 'avc1.' + hex2(d[i + 4]) + hex2(d[i + 5]) + hex2(d[i + 6]);
			return null;
		};
		ws.onopen = () => ws.send('v,1');
		ws.onmessage = (e) => {
			if (typeof e.data === 'string') {
				const m = JSON.parse(e.data);
				if (m.t === 'hello') r.hellos++;
				if (m.t === 'codec') r.codecMsgs++;
				return;
			}
			const b = new Uint8Array(e.data);
			if (b[0] === 0x05) r.v2++;
			if (b[0] !== 0x01) return;
			const key = (b[1] & 1) === 1;
			const id = ((b[2] << 24) | (b[3] << 16) | (b[4] << 8) | b[5]) >>> 0;
			const data = b.subarray(16);
			if (key) {
				r.keys++;
				const c = sps(data);
				if (!dec || c !== r.codec) {
					r.codec = c;
					dec?.close();
					dec = new VideoDecoder({ output: (f) => { r.frames++; f.close(); }, error: (x) => r.errors.push(x.message) });
					dec.configure({ codec: c, optimizeForLatency: true, hardwareAcceleration: 'no-preference' });
				}
			}
			if (!dec) return;
			dec.decode(new EncodedVideoChunk({ type: key ? 'key' : 'delta', timestamp: ++ts, data }));
			ws.send(`a,${id}`);
		};
		await new Promise((res) => setTimeout(res, 4000));
		ws.close();
		return r;
	}, `ws://127.0.0.1:${port}/ws`);
	check(legacy.frames >= 30 && legacy.keys >= 1 && legacy.errors.length === 0 && legacy.v2 === 0 && legacy.codecMsgs === 0, `protocol 1 client: ${JSON.stringify(legacy)}`);
	await decoding('the protocol 2 viewer while the protocol 1 client watched', 1440);

	check(errors.length === 0, `no decoder errors in the page (${errors.length}${errors.length ? ': ' + errors.join(' | ') : ''})`);
} catch (e) {
	check(false, `threw ${e && e.stack}`);
} finally {
	await b?.close();
	for (const c of procs.reverse()) {
		try {
			process.kill(-c.pid, 'SIGTERM');
		} catch {}
	}
	await sleep(500);
	for (const c of procs) {
		try {
			process.kill(-c.pid, 0);
			process.kill(-c.pid, 'SIGKILL');
			console.log(`killed leftover group ${c.pid}`);
		} catch {}
	}
}
console.log(failed ? `${failed} FAILED (server log: ${log})` : 'all passed');
process.exit(failed ? 1 : 0);
