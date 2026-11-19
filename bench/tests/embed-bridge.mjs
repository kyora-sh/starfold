// The embeddable client end to end through an integrator that reaches starfold only through an
// exec-style byte stream (embed-gateway.mjs, embed-relay.mjs): starfold with a password on its
// own Xvfb, the packed client mounted in a page with a ticket-minting preamble, real Chrome.
// Checks the first frame, a key reaching an X app, a reused and a never-issued ticket each
// ending in onClose(4401) with no reconnect, connect() after a close fetching a new ticket
// that works, and no ticket in any request URL. Runs in under a minute.
// Usage: node embed-bridge.mjs   (XVFB, STARFOLD_BIN, CHROMIUM, PLAYWRIGHT_NODE_MODULES;
// EMBED_DISPLAY: the X display to start Xvfb on, else Xvfb picks a free one; EMBED_PKG: an
// unpacked package/ from packages/web/embed/pack.sh, else it packs one, which needs esbuild
// and bun; needs xev, from x11-utils). CHROMIUM (else google-chrome, google-chrome-stable,
// chromium or chromium-browser, the first found on PATH) must decode H.264 (Google
// Chrome, or a Chromium built with its proprietary codecs): Playwright's own Chromium offers
// no decoder, and starfold answers it with no-common-codec and no frames.
import { createRequire } from 'module';
import { execFileSync, spawn } from 'child_process';
import crypto from 'crypto';
import fs from 'fs';
import os from 'os';
import path from 'path';
import { fileURLToPath } from 'url';
import { probeUpgrade, startGateway } from './embed-gateway.mjs';
const require = createRequire(process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/');
let pw;
try {
	pw = require('playwright');
} catch {
	pw = require('playwright-core');
}
const { chromium } = pw;

const HERE = path.dirname(fileURLToPath(import.meta.url));
const bin = process.env.STARFOLD_BIN || path.join(HERE, '../../target/release/starfold');
const port = Number(process.env.STARFOLD_PORT || '6911');
const password = crypto.randomBytes(16).toString('hex');
const log = '/tmp/starfold-embed-bridge.log';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
/** How long after an attempt ends the test watches for a reconnect it must not see. */
const QUIET_MS = 5000;
let failed = 0;
const check = (ok, what) => {
	console.log(`${ok ? 'PASS' : 'FAIL'} ${what}`);
	if (!ok) failed++;
};
const until = async (f, ms = 10000) => {
	for (const t = Date.now(); Date.now() - t < ms; await sleep(100)) {
		const v = await f();
		if (v) return v;
	}
	return null;
};

const group = (cmd, args, opts) => {
	const c = spawn(cmd, args, { detached: true, ...opts });
	c.on('error', (e) => check(false, `${cmd} did not start: ${e.message}`));
	procs.push(c);
	return c;
};
const procs = [];
let work;
let b;
let gw;
let cleaned = null;
const cleanup = () =>
	(cleaned ??= (async () => {
		await b?.close().catch(() => {});
		try {
			gw?.close();
		} catch {}
		for (const c of procs.toReversed()) {
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
		if (work) fs.rmSync(work, { recursive: true, force: true });
	})());
for (const [sig, code] of [
	['SIGINT', 130],
	['SIGTERM', 143],
])
	process.on(sig, () => {
		console.log(`${sig}: cleaning up`);
		cleanup().finally(() => process.exit(code));
	});

/**
 * Starts Xvfb and resolves with its display once it accepts clients (-displayfd); rejects if
 * it exits first (an occupied display) or is not ready within 10 s.
 */
function startXvfb() {
	const want = process.env.EMBED_DISPLAY;
	// No -shmem: with -displayfd there is no lock file, so starfold could not tell this
	// server's framebuffer segment from one an earlier Xvfb left behind.
	const args = [...(want ? [want] : []), '-displayfd', '3', '-screen', '0', '1920x1080x24', '-ac', '-nolisten', 'tcp'];
	const x = group(process.env.XVFB || 'Xvfb', args, { stdio: ['ignore', 'ignore', 'ignore', 'pipe'] });
	return new Promise((resolve, reject) => {
		let got = '';
		const timer = setTimeout(() => reject(new Error('Xvfb was not ready within 10 s')), 10000);
		x.stdio[3].on('data', (d) => {
			got += d;
			if (!got.includes('\n')) return;
			clearTimeout(timer);
			resolve(`:${got.trim()}`);
		});
		x.on('exit', (code, sig) => {
			clearTimeout(timer);
			reject(new Error(`Xvfb exited before it was ready (${code ?? sig}${want ? `; is ${want} in use?` : ''})`));
		});
	});
}

/** CHROMIUM, else the first Chrome or Chromium on PATH. */
function browserPath() {
	if (process.env.CHROMIUM) return process.env.CHROMIUM;
	for (const name of ['google-chrome', 'google-chrome-stable', 'chromium', 'chromium-browser'])
		for (const dir of (process.env.PATH || '').split(path.delimiter)) {
			const f = path.join(dir, name);
			try {
				fs.accessSync(f, fs.constants.X_OK);
				return f;
			} catch {}
		}
	throw new Error('no Chrome found: set CHROMIUM to a Chrome that decodes H.264');
}

try {
	const chrome = browserPath();
	work = fs.mkdtempSync(path.join(os.tmpdir(), 'embed-bridge-'));
	let pkgDir = process.env.EMBED_PKG;
	if (!pkgDir) {
		const tgz = execFileSync('bash', [path.join(HERE, '../../packages/web/embed/pack.sh'), 'e2e', work]).toString().trim().split('\n').pop();
		execFileSync('tar', ['-xzf', tgz, '-C', work]);
		pkgDir = path.join(work, 'package');
	}
	const DISPLAY = await startXvfb();
	// The X app the keys must reach: xev over the whole screen, so it has the keyboard focus
	// wherever the pointer is (no window manager: focus follows the pointer).
	let xevOut = '';
	const xev = group('stdbuf', ['-oL', 'xev', '-geometry', '1920x1080+0+0', '-event', 'keyboard'], { env: { ...process.env, DISPLAY }, stdio: ['ignore', 'pipe', 'ignore'] });
	xev.stdout.on('data', (d) => (xevOut += d));
	const out = fs.openSync(log, 'w');
	const server = group(bin, [`--port=${port}`, '--audio-enabled=false', `--basic-auth-password=${password}`, '--framerate=30'], { env: { ...process.env, DISPLAY, RUST_LOG: 'info' }, stdio: ['ignore', out, out] });
	let serverExited = false;
	server.on('exit', () => (serverExited = true));

	// Ready means our starfold answers /healthz; each request gets 1 s, the whole wait 10 s.
	const healthy = await until(async () => {
		if (serverExited) return false;
		try {
			return (await fetch(`http://127.0.0.1:${port}/healthz`, { signal: AbortSignal.timeout(1000) })).ok;
		} catch {
			return false;
		}
	}, 10000);
	if (!healthy || serverExited) throw new Error(`starfold was not ready on ${port} within 10 s${serverExited ? ' (it exited)' : ''}`);
	// The gateway's own upgrade request over the relay, without and then with the password.
	const bare = await probeUpgrade(port, null);
	check(/^HTTP\/1\.1 401 /.test(bare), `starfold asks for its password: the upgrade without it answers ${bare}`);
	const authed = await probeUpgrade(port, password);
	check(/^HTTP\/1\.1 101 /.test(authed), `with the password the same upgrade answers ${authed}`);

	gw = await startGateway({ page: path.join(HERE, 'embed-bridge.html'), pkgDir, starfoldPort: port, password });
	// Signals are ours: Playwright's handlers would exit before cleanup has stopped starfold.
	b = await chromium.launch({ executablePath: chrome, args: ['--no-sandbox'], handleSIGINT: false, handleSIGTERM: false, handleSIGHUP: false });
	const p = await b.newPage({ viewport: { width: 1280, height: 720 } });
	const urls = [];
	p.on('request', (r) => urls.push(r.url()));
	p.on('websocket', (w) => urls.push(w.url()));
	p.on('pageerror', (e) => console.log('[page error]', String(e)));
	await p.goto(`http://127.0.0.1:${gw.port}/`);

	const state = () => p.evaluate(() => {
		const t = window.embedTest;
		return { statuses: [...t.statuses], closes: [...t.closes], preambles: t.preambles, tickets: [...t.tickets] };
	});
	// Frames Chrome presented on the stripe <video>s since `mark()`, counted by the page's video
	// frame callbacks (embed-bridge.html), which dropped frames never get.
	let painted0 = 0;
	const presented = async () => (await p.evaluate(() => window.embedTest.painted)) - painted0;
	const mark = async () => {
		painted0 = await p.evaluate(() => window.embedTest.painted);
	};
	const opens = async () => (await state()).statuses.filter((s) => s === 'open').length;
	// The gateway completes the upgrade before it reads the ticket, so a rejected attempt
	// opens too. `ends` runs `act`, waits for a close and QUIET_MS more, and returns everything
	// that happened from before `act` on; `upgrades` counts every upgrade request the gateway
	// got, to any path. A retry scheduled later than QUIET_MS is caught by the final tally.
	const ends = async (act) => {
		const s0 = await state();
		const u0 = gw.upgrades;
		const f0 = gw.firsts.length;
		await act();
		await until(async () => (await state()).closes.length > s0.closes.length);
		await sleep(QUIET_MS);
		const s1 = await state();
		return { closes: s1.closes.slice(s0.closes.length), preambles: s1.preambles - s0.preambles, upgrades: gw.upgrades - u0, firsts: gw.firsts.slice(f0), status: s1.statuses.at(-1) };
	};
	const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
	const connect = (mode) =>
		p.evaluate((m) => {
			window.ticketMode = m;
			window.embedTest.handle.connect();
		}, mode);

	// 1) The first connect: a minted ticket, the bridge, frames.
	const first = await until(async () => (await opens()) === 1 && (await presented()) > 0, 20000);
	const s1 = await state();
	check(!!first, `first frame: status ${s1.statuses.join(' > ')}, ${await presented()} frames presented`);
	check(gw.admitted === 1 && same(gw.firsts, s1.tickets) && gw.upstreamRefused.length === 0, `gateway admitted ${gw.admitted} ticket(s), the minted one; starfold refused ${JSON.stringify(gw.upstreamRefused)}`);

	// 2) A key typed into the embed reaches the X app: a KeyPress record for q, after the press.
	await p.mouse.click(640, 360);
	await sleep(200);
	const x0 = xevOut.length;
	await p.keyboard.press('KeyQ');
	const pressed = () => xevOut.slice(x0).split(/\n\s*\n/).some((r) => /^KeyPress event/.test(r.trim()) && /keysym 0x71, q\)/.test(r));
	check(!!(await until(pressed, 5000)), 'q typed into the embed reached xev as a KeyPress');

	// 3) The integrator ends the session: one close, and no reconnect.
	const d3 = await ends(() => gw.endAll(4001, 'session ended'));
	check(same(d3.closes, [{ code: 4001, reason: 'session ended' }]) && d3.preambles === 0 && d3.upgrades === 0 && d3.status === 'closed', `server-ended session: ${JSON.stringify(d3)}`);

	// 4) The used ticket again, then one never issued: each sent first, onClose(4401) once, no
	// reconnect.
	const used = (await state()).tickets[0];
	const d4 = await ends(() => connect('reuse'));
	check(same(d4.closes, [{ code: 4401, reason: 'ticket rejected' }]) && same(d4.firsts, [used]) && d4.preambles === 1 && d4.upgrades === 1 && d4.status === 'closed', `reused ticket: ${JSON.stringify({ ...d4, firsts: d4.firsts.map((f) => (f === used ? 'the used ticket' : f)) })}`);
	const unknown = `not-a-ticket-${(await state()).preambles + 1}`;
	const d5 = await ends(() => connect('bad'));
	check(same(d5.closes, [{ code: 4401, reason: 'ticket rejected' }]) && same(d5.firsts, [unknown]) && d5.preambles === 1 && d5.upgrades === 1 && d5.status === 'closed', `unknown ticket: ${JSON.stringify(d5)}`);
	check(gw.rejected === 2 && gw.admitted === 1, `gateway rejected ${gw.rejected}, admitted ${gw.admitted}`);

	// 5) connect() after a close calls the preamble again, which mints a new ticket that works.
	const before = await state();
	const o0 = await opens();
	await mark();
	await connect('fresh');
	const again = await until(async () => (await opens()) === o0 + 1 && (await presented()) > 0, 15000);
	const s5 = await state();
	check(!!again && s5.preambles === before.preambles + 1, `reconnect: preamble called again (${s5.preambles}), open, ${await presented()} new frames`);
	check(s5.tickets.length === 2 && s5.tickets[0] !== s5.tickets[1] && gw.firsts.at(-1) === s5.tickets[1] && gw.admitted === 2, `a new ticket (${s5.tickets.length} minted, distinct: ${s5.tickets[0] !== s5.tickets[1]}), sent first and admitted (${gw.admitted})`);

	// 6) Over the whole run, after one more quiet window: exactly the four upgrade requests the
	// steps made (first, reused, unknown, reconnect), so no retry came late either, and the
	// reconnected session is still open, with no close since it opened.
	await sleep(QUIET_MS);
	check(gw.upgrades === 4, `${gw.upgrades} upgrade requests over the run, 4 expected`);
	const s7 = await state();
	check(s7.closes.length === s5.closes.length && s7.statuses.at(-1) === 'open', `the reconnected session stayed open: ${s7.closes.length - s5.closes.length} closes since, status ${s7.statuses.at(-1)}`);

	// 7) After that window, so a late request counts too: no ticket, issued or not, in any URL
	// the page requested or the gateway was asked for, raw or percent-decoded.
	const secrets = [...gw.minted, 'not-a-ticket'];
	// Each component is decoded on its own, so one malformed escape can't hide another.
	const decode = (x) => {
		try {
			return decodeURIComponent(x.replaceAll('+', ' '));
		} catch {
			return x;
		}
	};
	const forms = (u) => [u, decode(u), ...u.split(/[/?&=#;]/).map(decode)];
	const leaked = [...urls, ...gw.access].filter((u) => forms(u).some((f) => secrets.some((s) => f.includes(s))));
	check(leaked.length === 0 && gw.access.length > 0, `no ticket in ${urls.length} page request URLs or ${gw.access.length} gateway access-log lines${leaked.length ? ': ' + leaked.join(' | ') : ''}`);
	console.log('gateway access log:\n  ' + gw.access.join('\n  '));
} catch (e) {
	check(false, `threw ${e && e.stack}`);
} finally {
	await cleanup();
}
console.log(failed ? `${failed} FAILED (server log: ${log})` : 'all passed');
process.exit(failed ? 1 : 0);
