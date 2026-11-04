// Browser side. Every arm is measured by reading the marker strip back from the visible surface once
// per animation frame, so frame timing and latency have one definition everywhere; the viz swap trace
// runs alongside. Env: CHROMIUM, PLAYWRIGHT_NODE_MODULES, PW, LINK, ROUNDS, WIN, KEYS, OUT, BUDGET_S.
import { createRequire } from 'module';
import fs from 'fs';
import { ARMS, CTL_PORT, HEIGHT, NOVNC_PORT, NOVNC_PROFILES, STARFOLD_PORT, WAN, WIDTH, WORKLOADS } from './config.mjs';
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
const portOf = (arm) => (ARMS[arm].kind === 'novnc' ? NOVNC_PORT : STARFOLD_PORT) + offset;
const urlOf = (arm) => {
	const a = ARMS[arm];
	if (a.kind === 'starfold') return `http://127.0.0.1:${portOf(arm)}/?token=${token}${a.query}`;
	const { quality, compression } = NOVNC_PROFILES[a.profile];
	return `http://127.0.0.1:${portOf(arm)}/novnc-viewer.html?quality=${quality}&compression=${compression}`;
};
async function viewers(arm) {
	if (ARMS[arm].kind === 'starfold') {
		const r = await fetch(`http://127.0.0.1:${STARFOLD_PORT}/stats`, { headers: { authorization: `Bearer ${token}` } });
		return ((await r.json()).clients || []).length;
	}
	return Number(await ctl('/vnc-viewers'));
}
const CELL = 16, STRIP_W = 20 * CELL, STRIP_H = 3 * CELL;
function reader({ cell, w, h, readback }) {
	const vs = (window.vs = { frames: [], tags: [], keys: [], bad: 0, ticks: 0, long: 0, longMs: 0, seen: false, sync: 0 });
	addEventListener('starfold:firstframe', () => (vs.firstFrame = performance.now()));
	new PerformanceObserver((l) => { for (const e of l.getEntries()) { vs.long++; vs.longMs += e.duration; } }).observe({ type: 'longtask' });
	addEventListener('keydown', (e) => vs.keys.push([performance.now(), e.key]), true);
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
	let last = -1, lastTag = -1;
	const tick = () => {
		vs.ticks++;
		const el = pick();
		if (el) {
			x.drawImage(el, 0, 0, w, h, 0, 0, w, h);
			const d = x.getImageData(0, 0, w, h).data;
			const bit = (col, row) => { const i = ((row * cell + cell / 2) * w + col * cell + cell / 2) * 4; return d[i] + d[i + 1] + d[i + 2] > 382 ? 1 : 0; };
			const t = performance.now();
			let v = 0, ck = 0;
			for (let b = 0; b < 32; b++) v = (v | (bit(b % 20, b >= 20 ? 1 : 0) << b)) >>> 0;
			for (let b = 32; b < 40; b++) ck |= bit(b % 20, 1) << (b - 32);
			if (((v ^ (v >>> 8) ^ (v >>> 16) ^ (v >>> 24)) & 0xff) === ck) {
				vs.seen = true;
				const n = v >>> 20;
				if (n !== last) {
					last = n;
					let lat = (Date.now() & 0xfffff) - (v & 0xfffff);
					if (lat < -500000) lat += 0x100000;
					vs.frames.push([t, n, lat]);
				}
			} else vs.bad++;
			let tag = 0, inv = 0;
			for (let b = 0; b < 5; b++) { tag |= bit(b, 2) << b; inv |= bit(b + 5, 2) << b; }
			if ((tag ^ inv) === 31 && tag !== lastTag) { lastTag = tag; vs.tags.push([t, tag]); }
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
	const gap = wl === 'typing' ? [150, 100] : [300, 100];
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
/** Each key's first visible frame: tags appear in order, so a tag only answers its own key. */
function answer(keys, tags) {
	const i2p = [];
	let at = 0, missing = 0;
	for (const [t, key] of keys) {
		const v = key.charCodeAt(0) - 97;
		let i = at;
		while (i < tags.length && !(tags[i][0] >= t && tags[i][1] === v)) i++;
		if (i === tags.length) { missing++; continue; }
		at = i + 1;
		i2p.push(tags[i][0] - t);
	}
	return { i2p, missing };
}
async function run(arm, wl, readback = true) {
	await ctl(`/start?arm=${arm}&wl=${wl}`);
	const port = portOf(arm);
	const ctx = await b.newContext({ viewport: { width: WIDTH, height: HEIGHT } });
	try {
		const p = await ctx.newPage();
		const errs = [];
		p.on('pageerror', (e) => errs.push(String(e)));
		await p.addInitScript(reader, { cell: CELL, w: STRIP_W, h: STRIP_H, readback });
		await p.goto(urlOf(arm), { waitUntil: 'commit', timeout: 20000 });
		const ready = ARMS[arm].kind === 'starfold'
			? () => window.vs?.firstFrame !== undefined || window.starfoldFirstFrame !== undefined
			: () => window.novncConnected === true;
		await p.waitForFunction(ready, null, { timeout: 15000 });
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
			vs.sync = performance.now();
			performance.mark('vs-sync');
		});
		const c0 = chromeCpu(), s0 = await ctlJson('/cpu'), b0 = await ctlJson(`/bytes?port=${port}`), t0 = Date.now();
		const typing = typeKeys(p, wl, t0 + win * 1000);
		await sleep((win * 1000) / 2);
		const vMid = await viewers(arm);
		await sleep((win * 1000) / 2);
		const dt = (Date.now() - t0) / 1000, c1 = chromeCpu(), s1 = await ctlJson('/cpu'), b1 = await ctlJson(`/bytes?port=${port}`);
		const vEnd = await viewers(arm);
		await typing;
		await sleep(2000);
		const r = await p.evaluate(() => {
			const vs = window.vs;
			return { frames: vs.frames, tags: vs.tags, keys: vs.keys, bad: vs.bad, ticks: vs.ticks, long: vs.long, longMs: vs.longMs, sync: vs.sync, stats: window.voxlStats ? { codec: window.voxlStats.codec, implementation: window.voxlStats.implementation, hw: window.voxlStats.hw, encoder: window.voxlStats.encoder, dropped: window.voxlStats.dropped } : null };
		});
		const tr = swapsOf(await b.stopTracing());
		const off = tr.sync === null ? null : tr.sync - r.sync;
		const swaps = off === null ? [] : tr.raw.map((t) => t - off);
		const swapIv = swaps.slice(1).map((t, i) => t - swaps[i]);
		const f = r.frames.filter((x) => x[0] - r.sync <= dt * 1000);
		const iv = f.slice(1).map((x, i) => x[0] - f[i][0]);
		const lat = f.map((x) => x[2]);
		const span = f.length > 1 ? ((f.at(-1)[1] - f[0][1] + 4096) % 4096) + 1 : 0;
		const { i2p, missing } = answer(r.keys, r.tags);
		const acked = Object.keys(b1).filter((k) => k in b0).map((k) => b1[k] - b0[k]);
		const frames = wl === 'typing' || !readback ? {} : {
			fps: f.length / dt, ivP50: pct(iv, 0.5), ivP95: pct(iv, 0.95), ivP99: pct(iv, 0.99), ivMax: Math.max(...iv), jitter: stddev(iv),
			gaps50: iv.filter((x) => x > 50).length, gaps100: iv.filter((x) => x > 100).length,
			frameLatP50: pct(lat, 0.5), frameLatP95: pct(lat, 0.95), skipped: span - f.length,
		};
		return {
			...frames,
			keys: r.keys.length, keysSeen: i2p.length, keysMissing: missing,
			i2pP50: pct(i2p, 0.5), i2pP95: pct(i2p, 0.95), i2pMax: Math.max(...i2p),
			mbps: acked.length ? (Math.max(...acked) * 8) / dt / 1e6 : NaN,
			serverCpu: ((s1.server - s0.server) / dt) * 100, xCpu: ((s1.x - s0.x) / dt) * 100, sourceCpu: ((s1.source - s0.source) / dt) * 100, browserCpu: ((c1 - c0) / dt) * 100,
			swapsPerS: swaps.length / dt, swapP50: pct(swapIv, 0.5), swapP95: pct(swapIv, 0.95), swapP99: pct(swapIv, 0.99),
			rafHz: r.ticks / dt, badReads: r.bad, long: r.long, longMs: r.longMs, errs: errs.length, vMid, vEnd,
			stats: r.stats,
		};
	} finally {
		try { await b.stopTracing(); } catch {}
		await ctx.close();
		await ctl('/stop').catch(() => {});
		await sleep(500);
	}
}
const arms = Object.keys(ARMS).filter((a) => link === 'wan' || !ARMS[a].wanOnly).filter((a) => !process.env.ARMS || process.env.ARMS.split(',').includes(a));
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
		for (let k = 0; k < arms.length; k++) await slot(arms[(k + i + w) % arms.length], wl, i, true, 'main');
}
for (let i = 0; i < controlRounds && Date.now() - started < budget; i++)
	for (let k = 0; k < arms.length; k++) await slot(arms[(k + i) % arms.length], 'video', i, false, 'no-readback');
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
