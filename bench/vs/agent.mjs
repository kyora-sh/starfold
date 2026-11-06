// VM side: one Xvfb, then per run one streamer and one source, each in its own process group, and
// a control port on loopback. Usage: node agent.mjs <starfold> <source> <novnc web dir> <password file> <max seconds> <lan|wan> <shaper.py> <page raw>
import { execSync, spawn } from 'child_process';
import fs from 'fs';
import http from 'http';
import { ARMS, CTL_PORT, HEIGHT, FPS, NOVNC_PORT, STARFOLD_ARGS, STARFOLD_PORT, WAN, WIDTH, X11VNC_ARGS } from './config.mjs';
const [bin, source, web, pwFile, maxS, link, shaper, page] = process.argv.slice(2);
const DISPLAY = ':11', SOCK = '/tmp/vs-x11vnc.sock', DUMP = '/tmp/vs-dump.raw';
const pw = fs.readFileSync(pwFile, 'utf8').trim();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const group = (c, a, o = {}) => spawn(c, a, { detached: true, stdio: 'ignore', ...o, env: { ...process.env, DISPLAY, ...o.env } });
const xvfb = group('Xvfb', [DISPLAY, '-screen', '0', `${WIDTH}x${HEIGHT}x24`, '-dpi', '96', '-shmem', '-ac', '-nolisten', 'tcp', '-noreset']);
await sleep(1000);
const shapers = link === 'wan' ? [STARFOLD_PORT, NOVNC_PORT].map((p) => group('python3', [shaper, p + WAN.portOffset, `127.0.0.1:${p}`, WAN.mbps, WAN.rttMs, WAN.queueBytes].map(String))) : [];
let run = [], src = null;
const kill = async (procs) => {
	for (const c of procs) { try { process.kill(-c.pid, 'SIGTERM'); } catch {} }
	await sleep(300);
	for (const c of procs) { try { process.kill(-c.pid, 'SIGKILL'); } catch {} }
};
async function stop() {
	await kill([...run, ...(src ? [src] : [])]);
	run = [];
	src = null;
	fs.rmSync(SOCK, { force: true });
}
const listening = async (port) => {
	for (let i = 0; i < 100; i++) {
		const up = await fetch(`http://127.0.0.1:${port}/`).then(() => true, () => false);
		if (up) return;
		await sleep(100);
	}
	throw new Error(`nothing on ${port}`);
};
async function start(arm, wl) {
	await stop();
	src = group(source, [wl, WIDTH, HEIGHT, FPS, DUMP, page].map(String), { stdio: ['ignore', 'pipe', 'ignore'] });
	await new Promise((res, rej) => {
		const t = setTimeout(() => rej(new Error('source not ready')), 15000);
		src.stdout.on('data', (b) => { if (String(b).includes('READY')) { clearTimeout(t); res(); } });
		src.on('exit', (c) => rej(new Error(`source exited ${c}`)));
	});
	if (ARMS[arm].kind === 'novnc') {
		run.push(group('x11vnc', ['-display', DISPLAY, ...X11VNC_ARGS, '-unixsock', SOCK]));
		for (let i = 0; i < 100 && !fs.existsSync(SOCK); i++) await sleep(50);
		if (!fs.existsSync(SOCK)) throw new Error('x11vnc did not open its socket');
		let relay = SOCK;
		if (process.env.VS_TAP) {
			relay = `${SOCK}.tap`;
			fs.rmSync(relay, { force: true });
			run.push(group('python3', ['bench/vs/rfb-tap.py', SOCK, relay, process.env.VS_TAP]));
			for (let i = 0; i < 100 && !fs.existsSync(relay); i++) await sleep(50);
		}
		run.push(group('websockify', ['--web', web, `--unix-target=${relay}`, `127.0.0.1:${NOVNC_PORT}`]));
		await listening(NOVNC_PORT);
	} else {
		const log = fs.openSync('/tmp/vs-starfold.log', 'a');
		const a = ARMS[arm];
		run.push(group(a.bin ? process.env[a.bin] : bin, ['--addr=127.0.0.1', `--port=${STARFOLD_PORT}`, ...(a.args ?? STARFOLD_ARGS)], { env: { VOXL_PASSWORD: pw, RUST_LOG: 'warn' }, stdio: ['ignore', log, log] }));
		await listening(STARFOLD_PORT);
	}
}
let dumped = null;
/** Stops the picture and resolves as soon as the last frame is drawn; the source then writes DUMP. */
function freeze() {
	return new Promise((res, rej) => {
		const t = setTimeout(() => rej(new Error('source did not freeze')), 15000);
		dumped = new Promise((done) => {
			src.stdout.on('data', (x) => {
				if (String(x).includes('FROZEN')) { clearTimeout(t); res(); }
				if (String(x).includes('DUMPED')) done();
			});
		});
		process.kill(src.pid, 'SIGUSR1');
	});
}
/** The X server's truth in `out`, once DUMP is complete. */
async function truth(out) {
	await dumped;
	execSync(`xwd -root -silent -display ${DISPLAY} -out ${out}`);
}
/** CPU seconds of a process group, every member and thread. */
function cpu(pgid) {
	if (!pgid) return 0;
	let j = 0;
	for (const d of fs.readdirSync('/proc')) {
		if (!/^\d+$/.test(d)) continue;
		try {
			const t = fs.readFileSync(`/proc/${d}/stat`, 'latin1');
			const f = t.slice(t.lastIndexOf(')') + 2).split(' ');
			if (Number(f[2]) === pgid) j += Number(f[11]) + Number(f[12]);
		} catch {}
	}
	return j / 100;
}
/** Host jiffies from /proc/stat: all and stolen by the hypervisor. */
function hostCpu() {
	const f = fs.readFileSync('/proc/stat', 'latin1').split('\n')[0].trim().split(/\s+/).slice(1, 9).map(Number);
	return { all: f.reduce((a, b) => a + b, 0), steal: f[7] };
}
/** TCP payload acked by the peer per connection of a server port: the same count for every arm. */
function bytes(port) {
	const out = {};
	let peer = null;
	for (const l of execSync(`ss -tinH state established '( sport = :${port} )'`, { encoding: 'utf8' }).split('\n')) {
		const c = l.match(/^\s*\d+\s+\d+\s+\S+:\d+\s+\S+:(\d+)/);
		if (c) { peer = c[1]; out[peer] = 0; continue; }
		const b = l.match(/bytes_acked:(\d+)/);
		if (b && peer) out[peer] = Number(b[1]);
	}
	return out;
}
/** Connections x11vnc has accepted on its socket: exactly the VNC viewers. */
const vncViewers = () => fs.readFileSync('/proc/net/unix', 'latin1').split('\n').filter((l) => { const f = l.trim().split(/\s+/); return f[7] === SOCK && f[5] === '03'; }).length;
const quit = async () => {
	await stop();
	await kill([xvfb, ...shapers]);
	process.exit(0);
};
for (const s of ['SIGINT', 'SIGTERM']) process.on(s, quit);
http.createServer(async (req, res) => {
	const u = new URL(req.url, 'http://x');
	try {
		if (u.pathname === '/start') { await start(u.searchParams.get('arm'), u.searchParams.get('wl')); res.end('ok'); }
		else if (u.pathname === '/stop') { await stop(); res.end('ok'); }
		else if (u.pathname === '/cpu') res.end(JSON.stringify({ server: run.reduce((n, c) => n + cpu(c.pid), 0), x: cpu(xvfb.pid), source: cpu(src?.pid), host: hostCpu() }));
		else if (u.pathname === '/bytes') res.end(JSON.stringify(bytes(Number(u.searchParams.get('port')))));
		else if (u.pathname === '/freeze') { await freeze(); res.end('ok'); }
		else if (u.pathname === '/truth') { await truth(u.searchParams.get('out')); res.end('ok'); }
		else if (u.pathname === '/vnc-viewers') res.end(String(vncViewers()));
		else if (u.pathname === '/quit') { res.end('bye'); quit(); }
		else res.end('ok');
	} catch (e) {
		res.statusCode = 500;
		res.end(String(e));
	}
}).listen(CTL_PORT, '127.0.0.1', () => console.log('AGENT READY'));
setTimeout(quit, Number(maxS) * 1000);
