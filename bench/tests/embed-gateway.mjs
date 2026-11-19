// A stand-in for an integrator that reaches starfold only through an exec-style byte stream.
// It serves the page and the packed embed client, mints single-use tickets (POST /mint), and
// takes the browser's WebSocket on /ws. The first text message there must be a ticket it
// issued and nobody used; anything else gets close 4401. With a good ticket it starts a
// child process that relays bytes between its stdin/stdout and starfold's TCP port
// (embed-relay.mjs), speaks HTTP/1.1 and the WebSocket upgrade to starfold over that stream
// (Host: localhost, no Origin, Authorization: Bearer <password>), then passes every text and
// binary message both ways unchanged and answers both sides' pings itself. Close frames are not
// passed through as they are: a browser close is echoed back with its code only and sent on to
// starfold with that code and no reason; a starfold close reaches the browser with its code and
// reason, a close without a code (1005) as 1000. Every HTTP request line it gets is in `access`,
// every upgrade request (to any path) is counted in `upgrades`, and every attempt's first
// message is in `firsts`.
import { spawn } from 'child_process';
import crypto from 'crypto';
import fs from 'fs';
import http from 'http';
import path from 'path';
import { fileURLToPath } from 'url';

const GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';
const accept = (key) => crypto.createHash('sha1').update(key + GUID).digest('base64');
const TYPES = { '.html': 'text/html', '.js': 'text/javascript', '.wasm': 'application/wasm' };

// WebSocket framing, both ends: `frame` builds one (masked when we are the client), and
// `Frames` reassembles messages from a byte stream and hands each to `on(opcode, payload)`.
function frame(opcode, payload, mask) {
	const n = payload.length;
	const head = n < 126 ? 2 : n < 65536 ? 4 : 10;
	const out = Buffer.alloc(head + (mask ? 4 : 0) + n);
	out[0] = 0x80 | opcode;
	out[1] = (mask ? 0x80 : 0) | (n < 126 ? n : n < 65536 ? 126 : 127);
	if (head === 4) out.writeUInt16BE(n, 2);
	if (head === 10) out.writeBigUInt64BE(BigInt(n), 2);
	if (!mask) {
		payload.copy(out, head);
		return out;
	}
	const key = crypto.randomBytes(4);
	key.copy(out, head);
	for (let i = 0; i < n; i++) out[head + 4 + i] = payload[i] ^ key[i & 3];
	return out;
}

const closePayload = (code, reason) => {
	const r = Buffer.from(reason);
	const b = Buffer.alloc(2 + r.length);
	b.writeUInt16BE(code, 0);
	r.copy(b, 2);
	return b;
};

class Frames {
	constructor(on) {
		this.on = on;
		this.buf = Buffer.alloc(0);
		this.parts = [];
		this.op = 0;
	}
	push(chunk) {
		this.buf = this.buf.length ? Buffer.concat([this.buf, chunk]) : chunk;
		for (;;) {
			const b = this.buf;
			if (b.length < 2) return;
			let n = b[1] & 0x7f;
			let at = 2;
			if (n === 126) {
				if (b.length < 4) return;
				n = b.readUInt16BE(2);
				at = 4;
			} else if (n === 127) {
				if (b.length < 10) return;
				n = Number(b.readBigUInt64BE(2));
				at = 10;
			}
			const masked = (b[1] & 0x80) !== 0;
			const key = masked ? b.subarray(at, at + 4) : null;
			if (masked) at += 4;
			if (b.length < at + n) return;
			const payload = Buffer.from(b.subarray(at, at + n));
			if (key) for (let i = 0; i < n; i++) payload[i] ^= key[i & 3];
			this.buf = b.subarray(at + n);
			const fin = (b[0] & 0x80) !== 0;
			const op = b[0] & 0x0f;
			if (op >= 8) {
				this.on(op, payload);
				continue;
			}
			if (op !== 0) this.op = op;
			this.parts.push(payload);
			if (fin) {
				const whole = this.parts.length === 1 ? this.parts[0] : Buffer.concat(this.parts);
				this.parts = [];
				this.on(this.op, whole);
			}
		}
	}
}

/** The upgrade request the gateway sends starfold; `password` null leaves out Authorization. */
const upgradeRequest = (key, password) =>
	'GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n' +
	`Sec-WebSocket-Key: ${key}\r\nSec-WebSocket-Version: 13\r\n` +
	(password === null ? '' : `Authorization: Bearer ${password}\r\n`) +
	'\r\n';

const relayPath = path.join(path.dirname(fileURLToPath(import.meta.url)), 'embed-relay.mjs');

/**
 * Sends starfold the gateway's own upgrade request through a relay child, with `password` or
 * (null) without Authorization, and resolves with the response's status line.
 */
export function probeUpgrade(starfoldPort, password, ms = 5000) {
	return new Promise((resolve, reject) => {
		const child = spawn(process.execPath, [relayPath, '127.0.0.1', String(starfoldPort)], {
			stdio: ['pipe', 'pipe', 'ignore'],
		});
		let got = Buffer.alloc(0);
		const timer = setTimeout(() => {
			child.kill();
			reject(new Error('no upgrade response'));
		}, ms);
		child.stdin.on('error', () => {});
		child.stdout.on('data', (d) => {
			got = Buffer.concat([got, d]);
			const at = got.indexOf('\r\n');
			if (at < 0) return;
			clearTimeout(timer);
			child.kill();
			resolve(got.subarray(0, at).toString());
		});
		child.on('close', () => {
			clearTimeout(timer);
			reject(new Error('relay closed before a response'));
		});
		child.stdin.write(upgradeRequest(crypto.randomBytes(16).toString('base64'), password));
	});
}

// One browser connection: waits for the ticket, then bridges to starfold through the relay.
function session(gw, sock, head) {
	let upstream = null;
	let child = null;
	const pending = [];
	let ticketed = false;
	let done = false;
	const toBrowser = (op, payload) => {
		if (!sock.destroyed) sock.write(frame(op, payload, false));
	};
	const end = (code, reason) => {
		if (done) return;
		done = true;
		toBrowser(8, closePayload(code, reason));
		setTimeout(() => sock.destroy(), 500).unref();
		if (upstream) upstream.write(frame(8, closePayload(1000, ''), true));
		setTimeout(() => child?.kill(), 500).unref();
	};
	const timer = setTimeout(() => end(4401, 'no ticket'), 5000);
	gw.sessions.add({ end });

	const fromBrowser = new Frames((op, payload) => {
		if (op === 9) return toBrowser(10, payload);
		if (op === 10) return;
		if (op === 8) {
			const code = payload.length >= 2 ? payload.readUInt16BE(0) : 1000;
			if (!done) {
				toBrowser(8, payload.subarray(0, 2));
				if (upstream) upstream.write(frame(8, closePayload(code, ''), true));
			}
			done = true;
			sock.end();
			setTimeout(() => child?.kill(), 500).unref();
			return;
		}
		if (done) return;
		if (!ticketed) {
			clearTimeout(timer);
			const ticket = op === 1 ? payload.toString() : '';
			gw.firsts.push(op === 1 ? ticket : `<opcode ${op}>`);
			if (!gw.issued.delete(ticket)) {
				gw.rejected++;
				return end(4401, 'ticket rejected');
			}
			ticketed = true;
			gw.admitted++;
			return bridge();
		}
		if (upstream) upstream.write(frame(op, payload, true));
		else pending.push([op, payload]);
	});
	sock.on('data', (d) => fromBrowser.push(d));
	sock.on('error', () => {});
	sock.on('close', () => {
		done = true;
		if (child) setTimeout(() => child.kill(), 500).unref();
	});
	if (head.length) fromBrowser.push(head);

	function bridge() {
		child = spawn(process.execPath, [gw.relay, '127.0.0.1', String(gw.starfoldPort)], {
			stdio: ['pipe', 'pipe', 'ignore'],
		});
		gw.children.add(child);
		// 'close', not 'exit': it comes after the last of stdout has been read.
		child.on('close', () => {
			gw.children.delete(child);
			if (!done) end(1011, 'bridge ended');
		});
		const out = child.stdin;
		out.on('error', () => {});
		const key = crypto.randomBytes(16).toString('base64');
		out.write(upgradeRequest(key, gw.password));
		let response = Buffer.alloc(0);
		const fromStarfold = new Frames((op, payload) => {
			if (op === 9) return out.write(frame(10, payload, true));
			if (op === 10) return;
			if (op === 8) {
				const code = payload.length >= 2 ? payload.readUInt16BE(0) : 1000;
				const reason = payload.length > 2 ? payload.subarray(2).toString() : '';
				return end(code === 1005 ? 1000 : code, reason);
			}
			if (!done) toBrowser(op, payload);
		});
		child.stdout.on('data', (d) => {
			if (upstream) return fromStarfold.push(d);
			response = Buffer.concat([response, d]);
			const at = response.indexOf('\r\n\r\n');
			if (at < 0) return;
			const head = response.subarray(0, at).toString();
			const rest = response.subarray(at + 4);
			const [status, ...lines] = head.split('\r\n');
			const headers = new Map(
				lines.map((l) => [l.slice(0, l.indexOf(':')).trim().toLowerCase(), l.slice(l.indexOf(':') + 1).trim()])
			);
			const ok =
				/^HTTP\/1\.1 101 /.test(status) &&
				headers.get('upgrade')?.toLowerCase() === 'websocket' &&
				/(^|,)\s*upgrade\s*(,|$)/i.test(headers.get('connection') ?? '') &&
				headers.get('sec-websocket-accept') === accept(key);
			if (!ok) {
				gw.upstreamRefused.push(status);
				return end(1011, 'upstream refused');
			}
			upstream = out;
			for (const [op, payload] of pending.splice(0)) out.write(frame(op, payload, true));
			if (rest.length) fromStarfold.push(rest);
		});
	}
}

/**
 * Starts the gateway on 127.0.0.1 (an ephemeral port). `page` is served at /, `pkgDir` (the
 * unpacked embed package) under /pkg/.
 */
export async function startGateway({ page, pkgDir, starfoldPort, password }) {
	const gw = {
		relay: relayPath,
		starfoldPort,
		password,
		issued: new Set(),
		minted: [],
		firsts: [],
		access: [],
		sessions: new Set(),
		children: new Set(),
		admitted: 0,
		rejected: 0,
		upgrades: 0,
		upstreamRefused: [],
	};
	const server = http.createServer((req, res) => {
		gw.access.push(`${req.method} ${req.url}`);
		const url = new URL(req.url, 'http://gateway');
		if (req.method === 'POST' && url.pathname === '/mint') {
			const ticket = crypto.randomBytes(24).toString('hex');
			gw.issued.add(ticket);
			gw.minted.push(ticket);
			res.writeHead(200, { 'content-type': 'application/json', 'cache-control': 'no-store' });
			return res.end(JSON.stringify({ ticket }));
		}
		let file = null;
		if (url.pathname === '/') file = page;
		else if (url.pathname.startsWith('/pkg/') && !url.pathname.includes('..'))
			file = path.join(pkgDir, url.pathname.slice(5));
		if (!file || !fs.existsSync(file)) {
			res.writeHead(404);
			return res.end();
		}
		res.writeHead(200, { 'content-type': TYPES[path.extname(file)] || 'application/octet-stream' });
		fs.createReadStream(file).pipe(res);
	});
	server.on('upgrade', (req, sock, head) => {
		gw.access.push(`${req.method} ${req.url} (upgrade)`);
		gw.upgrades++;
		const key = req.headers['sec-websocket-key'];
		if (req.url !== '/ws' || !key) return sock.destroy();
		sock.write(
			'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n' +
				`Sec-WebSocket-Accept: ${accept(key)}\r\n\r\n`
		);
		session(gw, sock, head);
	});
	await new Promise((r) => server.listen(0, '127.0.0.1', r));
	gw.port = server.address().port;
	/** Ends every open session with this close, as the integrator ending a desktop session. */
	gw.endAll = (code, reason) => {
		for (const s of gw.sessions) s.end(code, reason);
		gw.sessions.clear();
	};
	gw.close = () => {
		gw.endAll(1001, 'gateway stopping');
		for (const c of gw.children) c.kill();
		server.closeAllConnections();
		server.close();
	};
	return gw;
}
