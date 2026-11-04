// Decoding in the worker, end to end, in headless Chrome:
// 1) a starfold on its own Xvfb, negotiated to H.264, decoded with WebCodecs;
// 2) dav1d-wasm, with the page's test hook (?nohwdecode) pretending the browser has no
//    hardware WebCodecs decoder and no WebCodecs AV1: the page and its scripts come from
//    the same starfold, its socket from a stand-in server here that streams the committed
//    AV1 fixture (fixtures/av1-640x360.ivf, two stripes of it) over protocol 2.
// Both check that nothing decoded raised an error and that the page's main thread had no
// task over 50 ms; the first also that dav1d.js and dav1d.wasm were never fetched.
// Fixture: ffmpeg -f lavfi -i "color=c=0x3050a0:s=640x360:r=30:d=2,format=rgb24,
//   drawbox=x=40+t*200:y=120:w=96:h=96:c=white:t=fill,scale=out_color_matrix=bt709:out_range=tv,
//   format=yuv420p" -c:v libaom-av1 -usage realtime -cpu-used 8 -lag-in-frames 0 -g 1000
//   -b:v 300k -colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv av1-640x360.ivf
// Usage: node decode.mjs   (XVFB, VOXL_BIN, CHROMIUM, PLAYWRIGHT_NODE_MODULES)
import { createRequire } from 'module';
import { spawn } from 'child_process';
import crypto from 'crypto';
import fs from 'fs';
import http from 'http';
import path from 'path';
import zlib from 'zlib';
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
const DISPLAY = process.env.DECODE_DISPLAY || ':10';
const port = Number(process.env.VOXL_PORT || 6910);
const stubPort = port + 1;
const base = `http://127.0.0.1:${port}`;
const log = '/tmp/voxl-decode.log';
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

/** The temporal units of an IVF file. */
function ivfFrames(file) {
	const b = fs.readFileSync(file);
	const out = [];
	for (let i = b.readUInt16LE(6); i + 12 <= b.length; ) {
		const n = b.readUInt32LE(i);
		out.push(b.subarray(i + 12, i + 12 + n));
		i += 12 + n;
	}
	return out;
}

/** RGB of an 8-bit RGB/RGBA PNG at (x, y). */
function pngPixel(png, x, y) {
	let i = 8;
	let w = 0, ch = 0;
	const idat = [];
	while (i < png.length) {
		const n = png.readUInt32BE(i);
		const type = png.toString('latin1', i + 4, i + 8);
		const d = png.subarray(i + 8, i + 8 + n);
		if (type === 'IHDR') {
			w = d.readUInt32BE(0);
			ch = { 2: 3, 6: 4 }[d[9]];
		} else if (type === 'IDAT') idat.push(d);
		i += 12 + n;
	}
	const raw = zlib.inflateSync(Buffer.concat(idat));
	const stride = w * ch;
	let prev = Buffer.alloc(stride);
	let row;
	for (let r = 0; r <= y; r++) {
		const f = raw[r * (stride + 1)];
		const src = raw.subarray(r * (stride + 1) + 1, (r + 1) * (stride + 1));
		row = Buffer.alloc(stride);
		for (let k = 0; k < stride; k++) {
			const a = k >= ch ? row[k - ch] : 0;
			const b = prev[k];
			const c = k >= ch ? prev[k - ch] : 0;
			const p = a + b - c;
			const pr = Math.abs(p - a) <= Math.abs(p - b) && Math.abs(p - a) <= Math.abs(p - c) ? a : Math.abs(p - b) <= Math.abs(p - c) ? b : c;
			row[k] = (src[k] + [0, a, b, (a + b) >> 1, pr][f]) & 0xff;
		}
		prev = row;
	}
	return [row[x * ch], row[x * ch + 1], row[x * ch + 2]];
}

// A protocol 2 server just for the socket: frames are WebSocket frames (RFC 6455).
function wsSend(sock, data) {
	const op = typeof data === 'string' ? 1 : 2;
	const body = typeof data === 'string' ? Buffer.from(data) : data;
	const n = body.length;
	const head = n < 126 ? Buffer.from([0x80 | op, n]) : n < 65536 ? Buffer.from([0x80 | op, 126, n >> 8, n & 255]) : (() => {
		const h = Buffer.alloc(10);
		h[0] = 0x80 | op;
		h[1] = 127;
		h.writeBigUInt64BE(BigInt(n), 2);
		return h;
	})();
	sock.write(Buffer.concat([head, body]));
}
function wsReader(onMessage) {
	let buf = Buffer.alloc(0);
	return (chunk) => {
		buf = Buffer.concat([buf, chunk]);
		for (;;) {
			if (buf.length < 2) return;
			let n = buf[1] & 0x7f;
			let o = 2;
			if (n === 126) {
				if (buf.length < 4) return;
				n = buf.readUInt16BE(2);
				o = 4;
			} else if (n === 127) {
				if (buf.length < 10) return;
				n = Number(buf.readBigUInt64BE(2));
				o = 10;
			}
			const masked = buf[1] & 0x80;
			const end = o + (masked ? 4 : 0) + n;
			if (buf.length < end) return;
			const mask = masked ? buf.subarray(o, o + 4) : null;
			const p = Buffer.from(buf.subarray(end - n, end));
			if (mask) for (let k = 0; k < n; k++) p[k] ^= mask[k & 3];
			onMessage(buf[0] & 0x0f, p);
			buf = buf.subarray(end);
		}
	};
}

const AV1 = 'av01.0.01M.08';
const tus = ivfFrames(path.join(HERE, 'fixtures/av1-640x360.ivf'));
const stub = { caps: null, ready: 0, acks: 0, lastAck: -1, keyRequests: 0, errors: [], sent: 0, corrupt: false, epochs: 0 };
// An OBU with its forbidden bit set: dav1d refuses it.
const BAD_TU = Buffer.from([0x8a, 0x01, 0x00]);
// Padding OBUs after the frame, which dav1d may keep buffered into the next call.
const PAD = Buffer.from([0x7a, 0x08, 0, 0, 0, 0, 0, 0, 0, 0]);
function serveStub() {
	const server = http.createServer(async (req, res) => {
		try {
			const r = await fetch(base + req.url);
			res.writeHead(r.status, { 'content-type': r.headers.get('content-type') || 'application/octet-stream', 'cache-control': 'no-cache' });
			res.end(Buffer.from(await r.arrayBuffer()));
		} catch (e) {
			res.writeHead(502);
			res.end(String(e));
		}
	});
	server.on('upgrade', (req, sock) => {
		const accept = crypto.createHash('sha1').update(req.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
		sock.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
		const stripes = [0, 1].map((i) => ({ i, y: i * 360, h: 360, config: { codec: AV1, codedWidth: 640, codedHeight: 360 } }));
		wsSend(sock, JSON.stringify({ t: 'hello', w: 640, h: 720, stripes: [[0, 360], [360, 360]], os: 'linux', audio: false, protocol: 2, offer: 1, videoOffers: [{ codec: 'av1', format: 'av1-low-overhead', stripes }] }));
		let epoch = 0;
		let timer = null;
		let id = 0;
		let tu = 0;
		const frame = () => {
			if (id - stub.lastAck > 4) return;
			id++;
			const key = tu % tus.length === 0;
			for (const s of [0, 1]) {
				const h = Buffer.alloc(20);
				h[0] = 0x05;
				h[1] = key ? 1 : 0;
				h.writeUInt32BE(id, 2);
				h[10] = s;
				h[11] = 2;
				h[12] = 2;
				h.writeUInt16BE(s * 360, 14);
				h.writeUInt32BE(epoch, 16);
				wsSend(sock, Buffer.concat([h, stub.corrupt && key ? BAD_TU : key ? tus[tu % tus.length] : Buffer.concat([tus[tu % tus.length], PAD])]));
			}
			stub.sent++;
			tu++;
		};
		sock.on('data', wsReader((op, p) => {
			if (op === 8) return sock.end();
			if (op !== 1) return;
			const t = p.toString();
			if (!t.startsWith('{')) return;
			const m = JSON.parse(t);
			if (m.t === 'hello' && m.v === 2) {
				stub.caps = m.decode;
				epoch = 1;
				wsSend(sock, JSON.stringify({ t: 'codec', epoch, codec: 'av1', format: 'av1-low-overhead', w: 640, h: 720, stripes }));
			} else if (m.t === 'codec-ready' && m.epoch === epoch) {
				stub.ready++;
				stub.lastAck = id;
				timer ??= setInterval(frame, 1000 / 30);
			} else if (m.t === 'ack' && m.epoch === epoch) {
				stub.acks++;
				stub.lastAck = Math.max(stub.lastAck, m.frame);
			} else if (m.t === 'keyframe') {
				stub.keyRequests++;
				tu = 0;
			} else if (m.t === 'codec-error') {
				stub.errors.push(m.reason);
				stub.corrupt = false;
				stub.epochs++;
				epoch = m.epoch + 1;
				tu = 0;
				wsSend(sock, JSON.stringify({ t: 'codec', epoch, codec: 'av1', format: 'av1-low-overhead', w: 640, h: 720, stripes }));
			}
		}));
		sock.on('close', () => clearInterval(timer));
		sock.on('error', () => clearInterval(timer));
	});
	return new Promise((r) => server.listen(stubPort, '127.0.0.1', () => r(server)));
}

// Each in its own process group, killed as a group at the end.
const group = (cmd, args, opts) => spawn(cmd, args, { detached: true, ...opts });
const procs = [];
procs.push(group(process.env.XVFB || 'Xvfb', [DISPLAY, '-screen', '0', '1920x1080x24', '-shmem', '-ac', '-nolisten', 'tcp'], { stdio: 'ignore' }));
await sleep(1000);
procs.push(group('sh', ['-c', 'while :; do xsetroot -solid "#$(od -An -N3 -tx1 /dev/urandom | tr -d " ")"; sleep 0.04; done'], { env: { ...process.env, DISPLAY }, stdio: 'ignore' }));
const out = fs.openSync(log, 'w');
procs.push(group(bin, [`--port=${port}`, '--audio-enabled=false', '--enable-basic-auth=false', '--framerate=30'], { env: { ...process.env, DISPLAY, RUST_LOG: 'info' }, stdio: ['ignore', out, out] }));

let b, server;
try {
	for (let i = 0; i < 100; i++) {
		try {
			if ((await fetch(`${base}/healthz`)).ok) break;
		} catch {}
		await sleep(100);
	}
	const sizes = {};
	for (const f of ['client.js', 'worker.js', 'dav1d.js', 'dav1d.wasm']) {
		const r = await fetch(`${base}/${f}`);
		sizes[f] = [(await r.arrayBuffer()).byteLength, r.headers.get('content-type')];
	}
	console.log('assets', JSON.stringify(sizes));
	check(sizes['dav1d.wasm'][1] === 'application/wasm', `dav1d.wasm served as ${sizes['dav1d.wasm'][1]}`);

	b = await chromium.launch({ executablePath: process.env.CHROMIUM || 'chromium', args: ['--no-sandbox', '--use-angle=swiftshader', '--enable-unsafe-swiftshader'] });
	const open = async (url) => {
		const ctx = await b.newContext({ viewport: { width: 1280, height: 800 } });
		const requests = [];
		ctx.on('request', (r) => requests.push(new URL(r.url()).pathname));
		const p = await ctx.newPage();
		const errors = [];
		p.on('console', (m) => {
			const t = m.text();
			if (/decoder error|configure failed|decode threw|no video|fell behind|unavailable/.test(t)) errors.push(t);
			if (/\[voxl\]/.test(t)) console.log('[page]', t);
		});
		p.on('pageerror', (e) => errors.push(String(e)));
		await p.addInitScript(() => {
			window.longTasks = [];
			new PerformanceObserver((l) => {
				for (const e of l.getEntries()) window.longTasks.push(Math.round(e.duration));
			}).observe({ type: 'longtask' });
		});
		// Readiness is the page's first painted frame; the load event must still come.
		await p.goto(url, { waitUntil: 'commit' });
		await p.waitForFunction(() => window.starfoldFirstFrame !== undefined, null, { timeout: 20000 });
		const loaded = await p.waitForFunction(() => document.readyState === 'complete', null, { timeout: 5000 }).then(() => true, () => false);
		check(loaded, `${url}: first frame painted, then the load event`);
		return { ctx, p, errors, requests };
	};
	const report = (p) => p.evaluate(() => window.voxlStats || {});
	const steady = async (p, what) => {
		await p.evaluate(() => (window.longTasks.length = 0));
		await sleep(3000);
		const r = await report(p);
		const long = await p.evaluate(() => window.longTasks);
		check(long.length === 0, `${what}: no main-thread task over 50 ms in 3 s (${long.join(', ') || 'none'})`);
		return r;
	};

	// 1) H.264 from the real server, WebCodecs in the worker.
	{
		const { ctx, p, errors, requests } = await open(`${base}/?stats`);
		const r0 = await until(async () => {
			const r = await report(p);
			return r.implementation && r.fps >= 15 ? r : null;
		}, 20000);
		check(!!r0 && r0.implementation === 'webcodecs' && /^avc1\./.test(r0.codec), `H.264: ${r0 ? `${r0.codec} via ${r0.implementation} (${r0.hw}), ${r0.fps.toFixed(1)} fps` : 'nothing decoded'}`);
		const shown = await p.evaluate(() => [...document.querySelectorAll('video.stripe')].map((v) => [v.style.display, v.getVideoPlaybackQuality().totalVideoFrames]));
		check(shown.length === 1 && shown[0][0] === '' && shown[0][1] > 0, `H.264 presented through <video> by default: ${JSON.stringify(shown)}`);
		const r = await steady(p, 'H.264');
		check(r.fps >= 15 && r.dropped === 0, `H.264 steady: ${r.fps?.toFixed(1)} fps, decode p50 ${r.decodeP50?.toFixed(1)} p95 ${r.decodeP95?.toFixed(1)} ms, ${r.dropped} dropped`);
		check(!requests.some((u) => /dav1d/.test(u)), `dav1d assets not fetched on the default path (${requests.filter((u) => /\.(js|wasm)$/.test(u)).join(' ')})`);
		check(errors.length === 0, `H.264: no decoder errors (${errors.join(' | ')})`);
		await ctx.close();
	}

	// 2) AV1 through dav1d-wasm, forced by the test hook, from the stand-in server.
	server = await serveStub();
	{
		const { ctx, p, errors, requests } = await open(`http://127.0.0.1:${stubPort}/?stats&nohwdecode`);
		const surface = await p.evaluate(() => ({ videos: document.querySelectorAll('video.stripe').length, canvas: document.getElementById('screen').style.display }));
		check(surface.videos === 0 && surface.canvas === '', `dav1d-wasm pictures on the canvas, no <video> stripes: ${JSON.stringify(surface)}`);
		await until(() => stub.acks > 30, 20000);
		const caps = stub.caps || [];
		check(caps.some((c) => c.codec === AV1 && c.implementation === 'dav1d-wasm' && !('hardwareAcceleration' in c)) && !caps.some((c) => c.codec === AV1 && c.implementation === 'webcodecs') && !caps.some((c) => c.hardwareAcceleration === 'prefer-hardware'), `capability hello: ${JSON.stringify(caps)}`);
		const a0 = stub.acks;
		const r = await steady(p, 'dav1d-wasm');
		const acked = stub.acks - a0;
		check(r.implementation === 'dav1d-wasm' && r.codec === AV1 && r.hw === null, `AV1: ${r.codec} via ${r.implementation}`);
		check(acked >= 60 && r.dropped === 0 && stub.errors.length === 0, `AV1 steady: ${acked} frames (2 stripes each) acked in 3 s, ${r.fps?.toFixed(1)} fps, decode p50 ${r.decodeP50?.toFixed(1)} p95 ${r.decodeP95?.toFixed(1)} ms, ${r.dropped} dropped, ${stub.keyRequests} keyframe requests, codec errors ${JSON.stringify(stub.errors)}`);
		check(requests.includes('/dav1d.js') && requests.includes('/dav1d.wasm'), 'dav1d.js and dav1d.wasm fetched once selected');
		// The background of both stripes: 0x3050a0 encoded BT.709 limited range.
		const png = await p.locator('#screen').screenshot();
		const box = await p.locator('#screen').boundingBox();
		const sx = box.width / 640, sy = box.height / 720;
		const px = [[600, 20], [600, 380], [20, 700]].map(([x, y]) => pngPixel(png, Math.round(x * sx), Math.round(y * sy)));
		const near = (c) => Math.abs(c[0] - 0x30) <= 8 && Math.abs(c[1] - 0x50) <= 8 && Math.abs(c[2] - 0xa0) <= 8;
		check(px.every(near), `decoded colour in both stripes ${JSON.stringify(px)} ~ [48,80,160]`);
		check(errors.length === 0, `AV1: no decoder errors (${errors.join(' | ')})`);

		// Decoder errors: the worker gives up on the epoch, and the next one decodes.
		stub.corrupt = true;
		stub.keyRequests = 0;
		await until(() => stub.errors.length > 0, 20000);
		check(stub.errors[0] === 'decode-error', `repeated decode errors reported as ${JSON.stringify(stub.errors)} after ${stub.keyRequests} keyframe requests`);
		const a1 = stub.acks;
		await until(() => stub.acks > a1 + 30, 10000);
		check(stub.acks > a1 + 30 && stub.ready === 2, `renegotiated: epoch 2 ready, ${stub.acks - a1} frames acked`);
		await ctx.close();
	}
} catch (e) {
	check(false, `threw ${e && e.stack}`);
} finally {
	await b?.close();
	server?.close();
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
