// Browser side. Every arm is measured by reading the marker strip back from the visible surface once
// per animation frame, so frame timing and latency have one definition everywhere; the viz swap trace
// runs alongside. QUALITY_ROUNDS adds screenshots against the X server's truth (quality.py).
// Env: CHROMIUM, PLAYWRIGHT_NODE_MODULES, PW, LINK, ROUNDS, WIN, OUT, BUDGET_S, CONTROL_ROUNDS, PARITY_ROUNDS, ARMS, WORKLOADS, SETTLE_MS, WS_TS, WHEEL.
import { execFileSync } from 'child_process';
import { createRequire } from 'module';
import fs from 'fs';
import { answer, delivered, KEY_CAP_MS } from './match.mjs';
import { armOf, expandArms, roundOrder } from './arms.mjs';
import { ARMS, CTL_PORT, HEIGHT, NOVNC_PORT, NOVNC_PROFILES, STARFOLD_PORT, WAN, WIDTH, WORKLOADS, KEYS_LOG } from './config.mjs';
const require = createRequire(process.env.PLAYWRIGHT_NODE_MODULES);
const { chromium } = require('playwright-core');
const link = process.env.LINK || 'lan', rounds = Number(process.env.ROUNDS || 5), win = Number(process.env.WIN || 10);
const controlRounds = Number(process.env.CONTROL_ROUNDS || 0), budget = Number(process.env.BUDGET_S || 1500) * 1000;
const token = fs.readFileSync(process.env.PW, 'utf8').trim();
const CTL = `http://127.0.0.1:${CTL_PORT}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const redact = (x) => String(x).replaceAll(token, 'REDACTED');
const ctl = async (p) => {
	const r = await fetch(CTL + p);
	const t = await r.text();
	if (!r.ok) throw new Error(`agent ${p}: ${t}`);
	return t;
};
const ctlJson = async (p) => JSON.parse(await ctl(p));
const pct = (v, q) => { const s = [...v].sort((a, b) => a - b); return s.length ? s[Math.min(s.length - 1, Math.max(0, Math.ceil(q * s.length) - 1))] : NaN; };
const stddev = (v) => { if (!v.length) return NaN; const m = v.reduce((a, b) => a + b, 0) / v.length; return Math.sqrt(v.reduce((a, b) => a + (b - m) ** 2, 0) / v.length); };
function chromeCpu() {
	let j = 0;
	for (const d of fs.readdirSync('/proc')) {
		if (!/^\d+$/.test(d)) continue;
		try {
			const t = fs.readFileSync(`/proc/${d}/stat`, 'latin1');
			if (!/^(chrome|headless)/.test(t.slice(t.indexOf('(') + 1, t.lastIndexOf(')')))) continue;
			const f = t.slice(t.lastIndexOf(')') + 2).split(' ');
			j += Number(f[11]) + Number(f[12]);
		} catch {}
	}
	return j / 100;
}
const offset = link === 'wan' ? WAN.portOffset : 0;
const portOf = (arm) => (armOf(arm).kind === 'novnc' ? NOVNC_PORT : STARFOLD_PORT) + offset;
const urlOf = (arm) => {
	const a = armOf(arm);
	if (a.kind === 'starfold') return `http://127.0.0.1:${portOf(arm)}/?token=${token}${a.query}`;
	const { quality, compression } = NOVNC_PROFILES[a.profile];
	return `http://127.0.0.1:${portOf(arm)}/novnc-viewer.html?quality=${quality}&compression=${compression}`;
};
const serverStats = async (arm) => (armOf(arm).kind === 'starfold' ? (await fetch(`http://127.0.0.1:${STARFOLD_PORT}/stats`, { headers: { authorization: `Bearer ${token}` } })).json() : {});
const statsDelta = (a, b) => Object.fromEntries([['keyframes', 'keyframes'], ['paintovers', 'paintovers'], ['refines', 'refines'], ['refinedFrames', 'paintover_frames'], ['refinedBytes', 'paintover_bytes'], ['flowBlocked', 'flow_blocked'], ['encoded', 'encoded'], ['bytes', 'bytes']].filter(([, k]) => typeof b[k] === 'number').map(([n, k]) => [n, b[k] - a[k]]));
/** Throws unless the one viewer is being served the arm's codec: a fallback must never count as that arm's result. */
function assertCodec(arm, stats, when) {
	if (armOf(arm).kind !== 'starfold' || armOf(arm).codec === null) return;
	const want = armOf(arm).codec ?? 'h264';
	const got = (stats.clients || []).map((c) => c.stream?.codec);
	if (got.length !== 1 || got[0] !== want) throw new Error(`codec ${when}: wanted ${want}, served ${JSON.stringify(got)}`);
}
async function viewers(arm) {
	if (armOf(arm).kind === 'starfold') {
		const r = await fetch(`http://127.0.0.1:${STARFOLD_PORT}/stats`, { headers: { authorization: `Bearer ${token}` } });
		return ((await r.json()).clients || []).length;
	}
	return Number(await ctl('/vnc-viewers'));
}
const SETTLE_MS = process.env.SETTLE_MS ? process.env.SETTLE_MS.split(',').map(Number) : [0, 250, 500, 1000, 2000, 3000, 5000, 8000];
const CELL = 16, STRIP_W = 20 * CELL, STRIP_H = 3 * CELL;
/** Client side of each update request: time from the last message received before it, and from the one before that request. */
function wsGaps(ws) {
	const held = [], cycle = [];
	let last = null, prevReq = null;
	for (const [t, n] of ws) {
		if (n > 0) { last = t; continue; }
		if (last !== null) held.push(t - last);
		if (prevReq !== null) cycle.push(t - prevReq);
		prevReq = t;
		last = null;
	}
	return { wsReqs: cycle.length, wsHeldP50: pct(held, 0.5), wsHeldP90: pct(held, 0.9), wsCycleP50: pct(cycle, 0.5) };
}
function reader({ cell, w, h, readback, wsTs }) {
	const vs = (window.vs = { frames: [], tags: [], keys: [], bad: 0, ticks: 0, long: 0, longMs: 0, seen: false, sync: 0 });
	addEventListener('starfold:firstframe', () => (vs.firstFrame = performance.now()));
	new PerformanceObserver((l) => { for (const e of l.getEntries()) { vs.long++; vs.longMs += e.duration; } }).observe({ type: 'longtask' });
	addEventListener('keydown', (e) => vs.keys.push([performance.now(), e.key]), true);
	if (wsTs) {
		vs.ws = [];
		const send = WebSocket.prototype.send;
		WebSocket.prototype.send = function (d) {
			const u = d instanceof ArrayBuffer ? new Uint8Array(d) : ArrayBuffer.isView(d) ? new Uint8Array(d.buffer, d.byteOffset, d.byteLength) : null;
			if (u && u[0] === 3) vs.ws.push([performance.now(), 0]);
			return send.call(this, d);
		};
		const on = Object.getOwnPropertyDescriptor(WebSocket.prototype, 'onmessage');
		Object.defineProperty(WebSocket.prototype, 'onmessage', {
			configurable: true,
			get: on.get,
			set(f) { on.set.call(this, typeof f === 'function' ? function (e) { vs.ws.push([performance.now(), e.data?.byteLength || 1]); return f.call(this, e); } : f); },
		});
	}
	if (!readback) return;
	const c = document.createElement('canvas');
	c.width = w;
	c.height = h;
	const x = c.getContext('2d', { willReadFrequently: true });
	const pick = () => {
		for (const el of document.querySelectorAll('canvas,video')) {
			const r = el.getBoundingClientRect();
			if (r.width > 0 && r.top <= 0 && r.left <= 0 && (el.videoWidth || el.width) > 0 && getComputedStyle(el).display !== 'none') return el;
		}
		return null;
	};
	// A canvas is read only after something drew on it: reading every frame holds up a page that decodes on its main thread.
	let drawn = true;
	for (const m of ['drawImage', 'putImageData', 'fillRect']) {
		const f = CanvasRenderingContext2D.prototype[m];
		CanvasRenderingContext2D.prototype[m] = function (...a) { if (this.canvas !== c) drawn = true; return f.apply(this, a); };
	}
	const own = new WeakMap();
	const contextOf = (el) => {
		if (!own.has(el)) {
			let ctx = null;
			try { ctx = el.getContext('2d'); } catch {}
			own.set(el, ctx);
		}
		return own.get(el);
	};
	let last = -1, lastTag = -1, pending = false, badV = -1, badCk = -1;
	const decode = (t, on) => {
		let v = 0, ck = 0;
		for (let b = 0; b < 32; b++) v = (v | (on(b % 20, b >= 20 ? 1 : 0) << b)) >>> 0;
		for (let b = 32; b < 40; b++) ck |= on(b % 20, 1) << (b - 32);
		if (((v ^ (v >>> 8) ^ (v >>> 16) ^ (v >>> 24)) & 0xff) === ck) {
			vs.seen = true;
			const n = v >>> 20;
			if (n !== last) {
				last = n;
				let lat = (Date.now() & 0xfffff) - (v & 0xfffff);
				if (lat < -500000) lat += 0x100000;
				vs.frames.push([t, n, lat]);
			}
		} else if (v !== badV || ck !== badCk) { badV = v; badCk = ck; vs.bad++; }
		let tag = 0, inv = 0;
		for (let b = 0; b < 5; b++) { tag |= on(b, 2) << b; inv |= on(b + 5, 2) << b; }
		if ((tag ^ inv) === 31 && tag !== lastTag) { lastTag = tag; vs.tags.push([t, tag]); }
	};
	const centre = (col, row) => [col * cell + cell / 2, row * cell + cell / 2];
	const fromRgba = (t, d) => decode(t, (col, row) => { const [px, py] = centre(col, row); const i = (py * w + px) * 4; return d[i] + d[i + 1] + d[i + 2] > 382 ? 1 : 0; });
	// The least the page can do per arm, so the reading doesn't slow what it reads: a 2D canvas is read
	// in place, a <video> gives up only the strip's luma, and a transferred canvas has to be drawn.
	const tick = () => {
		vs.ticks++;
		const el = pick();
		if (el && !pending) {
			const t = performance.now();
			const ctx = el.tagName === 'CANVAS' ? contextOf(el) : null;
			if (ctx) { if (drawn) { drawn = false; fromRgba(t, ctx.getImageData(0, 0, w, h).data); } }
			else if (el.tagName === 'VIDEO' && !vs.noFrames) {
				let f = null;
				try { f = new VideoFrame(el, { timestamp: 0 }); } catch { vs.noFrames = true; }
				if (f && ['I420', 'I420A', 'NV12'].includes(f.format)) {
					pending = true;
					const rect = { x: 0, y: 0, width: w, height: h };
					const buf = new Uint8Array(f.allocationSize({ rect }));
					f.copyTo(buf, { rect }).then((layout) => {
						const { offset, stride } = layout[0];
						decode(t, (col, row) => { const [px, py] = centre(col, row); return buf[offset + py * stride + px] > 126 ? 1 : 0; });
					}).catch(() => { vs.bad++; }).finally(() => { pending = false; f.close(); });
				} else {
					if (f) { vs.format = f.format; f.close(); }
					vs.noFrames = true;
				}
			} else {
				x.drawImage(el, 0, 0, w, h, 0, 0, w, h);
				fromRgba(t, x.getImageData(0, 0, w, h).data);
			}
		}
		requestAnimationFrame(tick);
	};
	requestAnimationFrame(tick);
}
const b = await chromium.launch({ executablePath: process.env.CHROMIUM, args: ['--no-sandbox', '--autoplay-policy=no-user-gesture-required'], headless: true });
console.log(`chrome ${b.version()} link ${link}`);
function swapsOf(buf) {
	const ev = JSON.parse(buf.toString()).traceEvents || [];
	const sync = ev.find((e) => e.name === 'vs-sync');
	const raw = ev.filter((e) => e.name === 'Display::DrawAndSwap' && e.ph === 'X').map((e) => (e.ts + (e.dur || 0)) / 1000).sort((x, y) => x - y);
	return { raw, sync: sync ? sync.ts / 1000 : null };
}
async function typeKeys(p, wl, until) {
	const letters = 'abcdefghijklmnopqrstuvwxyz';
	const gap = wl === 'typing' ? [150, 100] : wl === 'mixed' || wl === 'paged' ? [700, 600] : [300, 100];
	const sent = [];
	let prev = '';
	while (Date.now() < until) {
		let k;
		do k = letters[Math.floor(Math.random() * 26)]; while (k === prev);
		prev = k;
		sent.push(k);
		await p.keyboard.press(k);
		await sleep(gap[0] + Math.random() * gap[1]);
	}
	return sent;
}
async function run(arm, wl, readback = true) {
	await ctl(`/start?arm=${arm}&wl=${wl}`);
	const port = portOf(arm);
	const ctx = await b.newContext({ viewport: { width: WIDTH, height: HEIGHT } });
	try {
		const p = await ctx.newPage();
		const errs = [];
		p.on('pageerror', (e) => errs.push(String(e)));
		await p.addInitScript(reader, { cell: CELL, w: STRIP_W, h: STRIP_H, readback, wsTs: !!process.env.WS_TS });
		await p.goto(urlOf(arm), { waitUntil: 'commit', timeout: 20000 });
		const ready = armOf(arm).kind === 'starfold'
			? () => window.vs?.firstFrame !== undefined || window.starfoldFirstFrame !== undefined
			: () => window.novncConnected === true;
		await p.waitForFunction(ready, null, { timeout: 15000 }).catch((e) => { throw new Error(`${String(e.message).split('\n')[0]}${errs.length ? ` page: ${errs[0]}` : ''}`); });
		if (readback) await p.waitForFunction(() => window.vs.seen, null, { timeout: 15000 });
		else await sleep(1000);
		await sleep(3000);
		await p.mouse.click(WIDTH / 2, HEIGHT / 2);
		await sleep(200);
		await b.startTracing(p, { categories: ['viz', 'blink.user_timing'] });
		await p.evaluate(() => {
			const vs = window.vs;
			vs.frames.length = vs.tags.length = vs.keys.length = 0;
			vs.bad = vs.ticks = vs.long = vs.longMs = 0;
			if (vs.ws) vs.ws.length = 0;
			vs.sync = performance.now();
			performance.mark('vs-sync');
		});
		const f0 = await serverStats(arm);
		assertCodec(arm, f0, 'at start');
		const c0 = chromeCpu(), s0 = await ctlJson('/cpu'), b0 = await ctlJson(`/bytes?port=${port}`), t0 = Date.now();
		const typing = typeKeys(p, wl, t0 + win * 1000);
		await sleep((win * 1000) / 2);
		const vMid = await viewers(arm);
		await sleep((win * 1000) / 2);
		const dt = (Date.now() - t0) / 1000, c1 = chromeCpu(), s1 = await ctlJson('/cpu'), b1 = await ctlJson(`/bytes?port=${port}`);
		const ticks = await p.evaluate(() => window.vs.ticks);
		const f1 = await serverStats(arm);
		assertCodec(arm, f1, 'at end');
		const vEnd = await viewers(arm);
		await typing;
		await sleep(KEY_CAP_MS);
		const r = await p.evaluate(() => {
			const vs = window.vs;
			return { ws: vs.ws ?? null, frames: vs.frames, tags: vs.tags, keys: vs.keys, bad: vs.bad, ticks: vs.ticks, long: vs.long, longMs: vs.longMs, sync: vs.sync, noFrames: !!vs.noFrames, format: vs.format, stats: window.voxlStats ? { codec: window.voxlStats.codec, implementation: window.voxlStats.implementation, hw: window.voxlStats.hw, encoder: window.voxlStats.encoder, dropped: window.voxlStats.dropped } : null };
		});
		const tr = swapsOf(await b.stopTracing());
		const off = tr.sync === null ? null : tr.sync - r.sync;
		const swaps = off === null ? [] : tr.raw.map((t) => t - off).filter((t) => t >= r.sync && t - r.sync <= dt * 1000);
		const swapIv = swaps.slice(1).map((t, i) => t - swaps[i]);
		const f = r.frames.filter((x) => x[0] - r.sync <= dt * 1000);
		const iv = f.slice(1).map((x, i) => x[0] - f[i][0]);
		const lat = f.map((x) => x[2]);
		const span = f.length > 1 ? ((f.at(-1)[1] - f[0][1] + 4096) % 4096) + 1 : 0;
		const { i2p, noOwnFrame } = answer(r.keys, r.tags);
		const sent = r.keys.map((k) => k[1]);
		const received = fs.readFileSync(KEYS_LOG, 'utf8').split('\n').filter(Boolean).map((l) => l.split(' ')[1]);
		const reached = delivered(sent, received);
		if (reached !== sent.length || received.length !== sent.length)
			throw new Error(`keys lost before the source: ${sent.length} sent, ${received.length} received, ${sent.length - reached} never arrived`);
		const acked = Object.keys(b1).filter((k) => k in b0).map((k) => b1[k] - b0[k]);
		const frames = wl === 'typing' || !readback ? {} : {
			fps: f.length / dt, ivP50: pct(iv, 0.5), ivP95: pct(iv, 0.95), ivP99: pct(iv, 0.99), ivMax: Math.max(...iv), jitter: stddev(iv),
			gaps50: iv.filter((x) => x > 50).length, gaps100: iv.filter((x) => x > 100).length,
			frameLatP50: pct(lat, 0.5), frameLatP95: pct(lat, 0.95), skipped: span - f.length,
		};
		return {
			...frames,
			...statsDelta(f0, f1),
			keys: r.keys.length, keysReceived: received.length, keysSeen: i2p.length, keysNoOwnFrame: noOwnFrame,
			i2pP50: pct(i2p, 0.5), i2pP95: pct(i2p, 0.95), i2pMax: Math.max(...i2p),
			mbps: acked.length ? (Math.max(...acked) * 8) / dt / 1e6 : NaN,
			serverCpu: ((s1.server - s0.server) / dt) * 100, xCpu: ((s1.x - s0.x) / dt) * 100, sourceCpu: ((s1.source - s0.source) / dt) * 100, stealPct: ((s1.host.steal - s0.host.steal) / (s1.host.all - s0.host.all)) * 100, browserCpu: ((c1 - c0) / dt) * 100,
			swapsPerS: swaps.length / dt, swapP50: pct(swapIv, 0.5), swapP95: pct(swapIv, 0.95), swapP99: pct(swapIv, 0.99),
			rafHz: ticks / dt, noFrames: r.noFrames ? 1 : 0, format: r.format, badReads: r.bad, long: r.long, longMs: r.longMs, errs: errs.length, vMid, vEnd,
			stats: r.stats,
			...(r.ws ? wsGaps(r.ws) : {}),
		};
	} finally {
		try { await b.stopTracing(); } catch {}
		await ctx.close();
		await ctl('/stop').catch(() => {});
		await sleep(500);
	}
}
/** Screenshots are the compositor's output, the same path for every arm. Own runs: they disturb the timing. */
async function quality(arm, wl, round) {
	await ctl(`/start?arm=${arm}&wl=${wl}`);
	const dir = `/tmp/vs-q/${arm}-${wl}-${round}`;
	fs.mkdirSync(dir, { recursive: true });
	const ctx = await b.newContext({ viewport: { width: WIDTH, height: HEIGHT } });
	try {
		const p = await ctx.newPage();
		await p.addInitScript(reader, { cell: CELL, w: STRIP_W, h: STRIP_H, readback: true });
		await p.goto(urlOf(arm), { waitUntil: 'commit', timeout: 20000 });
		await p.waitForFunction(() => window.vs.seen, null, { timeout: 15000 });
		await sleep(3000);
		await p.mouse.click(WIDTH / 2, HEIGHT / 2);
		await sleep(200);
		const s0 = await serverStats(arm);
		assertCodec(arm, s0, 'at start');
		if (wl === 'typing') await typeKeys(p, wl, Date.now() + 5000);
		else if (wl === 'mixed' || wl === 'paged') await typeKeys(p, wl, Date.now() + Number(process.env.MIXED_S || 30) * 1000);
		else {
			// WHEEL: wheel input reaches the desktop throughout the motion, as a user scrolling would send it.
			let wheeling = !!process.env.WHEEL, wheelErr;
			const wheel = (async () => { while (wheeling) { await p.mouse.wheel(0, 120); await sleep(100); } })().catch((e) => { wheelErr = e; });
			try {
				for (let i = 0; i < 6; i++) { await p.screenshot({ path: `${dir}/motion-${i}.png`, animations: 'allow' }); await sleep(800); }
			} finally {
				wheeling = false;
				await wheel;
			}
			if (wheelErr) throw wheelErr;
		}
		const s1 = await serverStats(arm);
		assertCodec(arm, s1, 'at end');
		const viewerCount = await viewers(arm);
		await ctl('/freeze');
		const t0 = Date.now(), shotAt = {}, after = {}, timeline = [];
		let polling = !!process.env.TIMELINE;
		const acked0 = polling ? Object.values(await ctlJson(`/bytes?port=${portOf(arm)}`)) : [];
		const poller = (async () => { while (polling) { const a = Object.values(await ctlJson(`/bytes?port=${portOf(arm)}`)); const st = statsDelta(s1, await serverStats(arm)); timeline.push([Date.now() - t0, Math.max(0, ...a.map((x, i) => x - (acked0[i] ?? 0))), st.refinedFrames ?? 0, st.refinedBytes ?? 0, st.paintovers ?? 0]); await sleep(40); } })();
		for (const ms of SETTLE_MS) {
			await sleep(Math.max(0, ms - (Date.now() - t0)));
			shotAt[ms] = Date.now() - t0;
			await p.screenshot({ path: `${dir}/settled-${ms}.png`, animations: 'allow' });
			after[ms] = statsDelta(s1, await serverStats(arm));
		}
		polling = false;
		await poller;
		await ctl(`/truth?out=${dir}/truth.xwd`);
		const crops = process.env.CROPS && round === 0 ? [`${process.env.CROPS}/${arm}-${wl}`] : [];
		const scores = JSON.parse(execFileSync('python3', ['bench/vs/quality.py', dir, wl, '/tmp/vs-dump.raw', ...crops], { encoding: 'utf8', maxBuffer: 1 << 24 }));
		fs.rmSync(dir, { recursive: true, force: true });
		return { ...scores, viewerCount, motion: statsDelta(s0, s1), shotAt, afterFreeze: after, timeline };
	} finally {
		await ctx.close();
		await ctl('/stop').catch(() => {});
		await sleep(500);
	}
}
/** A long mixed run: thousands of refinements, a browser decoding all of it, then the settled picture against the truth. */
async function soak(arm, seconds) {
	await ctl(`/start?arm=${arm}&wl=mixed`);
	const dir = `/tmp/vs-q/${arm}-soak`;
	fs.mkdirSync(dir, { recursive: true });
	const ctx = await b.newContext({ viewport: { width: WIDTH, height: HEIGHT } });
	try {
		const p = await ctx.newPage();
		const errs = [], decodeErrs = [];
		p.on('pageerror', (e) => errs.push(String(e)));
		p.on('console', (m) => { const t = m.text(); if (/decoder? error|decode failed|keyframe needed|EncodingError|OperationError/i.test(t)) decodeErrs.push(t.slice(0, 200)); });
		await p.addInitScript(reader, { cell: CELL, w: STRIP_W, h: STRIP_H, readback: true });
		await p.goto(urlOf(arm), { waitUntil: 'commit', timeout: 20000 });
		await p.waitForFunction(() => window.vs.seen, null, { timeout: 15000 });
		await sleep(3000);
		await p.mouse.click(WIDTH / 2, HEIGHT / 2);
		await sleep(200);
		await p.evaluate(() => { const vs = window.vs; vs.frames.length = vs.tags.length = vs.keys.length = 0; vs.bad = vs.ticks = 0; });
		const s0 = await serverStats(arm);
		assertCodec(arm, s0, 'at start');
		const t0 = Date.now(), until = t0 + seconds * 1000, windows = [];
		const typing = typeKeys(p, 'mixed', until);
		let prev = s0;
		while (Date.now() < until) {
			await sleep(Math.min(60000, Math.max(0, until - Date.now())));
			const s = await serverStats(arm);
			const dl = statsDelta(prev, s);
			const v = await p.evaluate(() => { const vs = window.vs; const r = { frames: vs.frames.length, bad: vs.bad, tags: vs.tags.slice(), keys: vs.keys.slice(), lat: vs.frames.map((f) => f[2]) }; vs.frames.length = 0; vs.tags.length = 0; vs.keys.length = 0; return r; });
			const { i2p, noOwnFrame } = answer(v.keys, v.tags);
			windows.push({ at: Math.round((Date.now() - t0) / 1000), targetKbps: s.target_kbps, ...dl, viewerFrames: v.frames, badReads: v.bad, frameLatP50: pct(v.lat, 0.5), frameLatP95: pct(v.lat, 0.95), i2pP50: pct(i2p, 0.5), i2pP95: pct(i2p, 0.95), i2pMax: i2p.length ? Math.max(...i2p) : null, keysSeen: i2p.length, keysNoOwnFrame: noOwnFrame, decodeErrs: decodeErrs.length, errs: errs.length });
			console.log(`SOAK ${arm} ${JSON.stringify(windows.at(-1))}`);
			assertCodec(arm, s, `at ${windows.at(-1).at} s`);
			prev = s;
		}
		await typing;
		const s1 = await serverStats(arm);
		await ctl('/freeze');
		await sleep(1500);
		await p.screenshot({ path: `${dir}/settled-1500.png`, animations: 'allow' });
		await ctl(`/truth?out=${dir}/truth.xwd`);
		const scores = JSON.parse(execFileSync('python3', ['bench/vs/quality.py', dir, 'typing', '/tmp/vs-dump.raw'], { encoding: 'utf8', maxBuffer: 1 << 24 }));
		fs.rmSync(dir, { recursive: true, force: true });
		const viewerCount = await viewers(arm);
		const stats = await p.evaluate(() => (window.voxlStats ? { dropped: window.voxlStats.dropped, codec: window.voxlStats.codec, implementation: window.voxlStats.implementation } : null));
		return { ...scores, total: statsDelta(s0, s1), windows, decodeErrs: decodeErrs.slice(0, 5), decodeErrCount: decodeErrs.length, errs: errs.slice(0, 5), viewerCount, stats };
	} finally {
		await ctx.close();
		await ctl('/stop').catch(() => {});
		await sleep(500);
	}
}
// AA=0 drops the A/A twin of the first arm; RUN=<i> continues the arm rotation of earlier runs of `rounds` rounds.
const listed = process.env.ARMS ? process.env.ARMS.split(',') : Object.keys(ARMS).filter((a) => !ARMS[a].probe && (link === 'wan' || !ARMS[a].wanOnly));
const arms = expandArms(listed, process.env.AA !== '0');
const order = (i, w, n) => roundOrder(arms, i, w, Number(process.env.RUN || 0), n);
const wls = process.env.WORKLOADS ? process.env.WORKLOADS.split(',') : WORKLOADS;
const results = {}, ledger = {};
const out = process.env.OUT ? fs.createWriteStream(process.env.OUT) : null;
const emit = (o) => { const s = JSON.stringify({ link, ...o }); console.log(`RUN ${s}`); out?.write(`${s}\n`); };
const started = Date.now();
async function slot(arm, wl, round, readback, setName) {
	const key = `${setName}|${wl}|${arm}`;
	const l = (ledger[key] ??= { attempted: 0, valid: 0, failed: 0, why: [] });
	l.attempted++;
	try {
		const r = await run(arm, wl, readback);
		if (r.vMid !== 1 || r.vEnd !== 1) throw new Error(`viewers mid ${r.vMid} end ${r.vEnd}`);
		l.valid++;
		(results[key] ??= []).push(r);
		emit({ set: setName, wl, arm, round, ...r });
	} catch (e) {
		l.failed++;
		const why = redact(e).split('\n')[0];
		l.why.push(why);
		emit({ set: setName, wl, arm, round, failed: why });
	}
}
for (let i = 0; i < rounds && Date.now() - started < budget; i++) {
	console.log(`ROUND ${i} at ${Math.round((Date.now() - started) / 1000)} s`);
	for (const [w, wl] of wls.entries())
		for (const arm of order(i, w, rounds)) await slot(arm, wl, i, true, 'main');
}
// Reader against no reader, each arm's pair side by side: whether reading the strip moves what the link delivers.
for (let i = 0; i < Number(process.env.PARITY_ROUNDS || 0) && Date.now() - started < budget; i++)
	for (const [w, wl] of wls.entries()) {
		const pairs = arms.flatMap((arm) => [[arm, true], [arm, false]]);
		for (let k = 0; k < pairs.length; k++) {
			const [arm, readback] = pairs[(k + i + w) % pairs.length];
			await slot(arm, wl, i, readback, readback ? 'parity-reader' : 'parity-none');
		}
	}
for (let i = 0; i < controlRounds && Date.now() - started < budget; i++)
	for (const [w, wl] of wls.entries())
		for (const arm of order(i, w, controlRounds)) await slot(arm, wl, i, false, 'no-readback');
for (let i = 0; i < Number(process.env.QUALITY_ROUNDS || 0) && Date.now() - started < budget; i++)
	for (const [w, wl] of wls.entries())
		for (const arm of order(i, w, Number(process.env.QUALITY_ROUNDS))) {
			const key = `quality|${wl}|${arm}`;
			const l = (ledger[key] ??= { attempted: 0, valid: 0, failed: 0, why: [] });
			l.attempted++;
			try {
				const r = await quality(arm, wl, i);
				if (r.viewerCount !== 1) throw new Error(`viewers ${r.viewerCount}`);
				l.valid++;
				emit({ set: 'quality', wl, arm, round: i, ...r });
			} catch (e) {
				l.failed++;
				const why = redact(e).split('\n')[0];
				l.why.push(why);
				emit({ set: 'quality', wl, arm, round: i, failed: why });
			}
		}
if (process.env.SOAK_S) for (const arm of arms) {
	try { emit({ set: 'soak', arm, ...(await soak(arm, Number(process.env.SOAK_S))) }); }
	catch (e) { emit({ set: 'soak', arm, failed: redact(e).split('\n')[0] }); }
}
const median = (v) => pct(v, 0.5);
console.log('LEDGER', JSON.stringify(ledger));
for (const key of Object.keys(results).sort()) {
	console.log(`== ${key}`);
	const runs = results[key];
	for (const m of Object.keys(runs[0])) {
		const v = runs.map((r) => r[m]).filter((x) => typeof x === 'number' && Number.isFinite(x));
		if (v.length) console.log(`  ${m}: ${median(v).toFixed(1)} [${Math.min(...v).toFixed(1)}..${Math.max(...v).toFixed(1)}] n=${v.length}`);
	}
	console.log(`  stats: ${JSON.stringify(runs.at(-1).stats)}`);
}
out?.end();
await b.close();
process.exit(0);
