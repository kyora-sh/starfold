// The client (the built dist/) against a real server on Xvfb, typing into an xterm, under any of
// Node, Bun and Deno. Needs Linux with Xvfb and xterm, and STARFOLD_BIN pointing at a starfold
// binary; skips without them (startServer's own test is packages/server's):
//
//   STARFOLD_BIN=target/release/starfold node test/integration.mjs   (or bun, or deno run -A)
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import {
	existsSync,
	mkdirSync,
	mkdtempSync,
	readFileSync,
	readdirSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const dist = join(
	dirname(fileURLToPath(import.meta.url)),
	'..',
	'dist',
	'index.js'
);
const runtime =
	typeof Bun !== 'undefined'
		? 'bun'
		: typeof Deno !== 'undefined'
			? 'deno'
			: 'node';
const have = (cmd) => spawnSync('sh', ['-c', `command -v ${cmd}`]).status === 0;

if (
	!process.env.STARFOLD_BIN ||
	process.platform !== 'linux' ||
	!have('Xvfb') ||
	!have('xterm')
) {
	console.log(
		'skip: set STARFOLD_BIN and install Xvfb and xterm to run (Linux)'
	);
	process.exit(0);
}

const { Starfold } = await import(dist);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const marker = randomBytes(8).toString('hex');
const baseEnv = {
	...process.env,
	STARFOLD_TEST_RUN: marker,
	STARFOLD_TEST_HOOKS: '1',
};
const procs = [];

/** Processes started by this run, found by the marker in their environment. */
function ours() {
	const found = [];
	for (const d of readdirSync('/proc')) {
		if (!/^\d+$/.test(d) || Number(d) === process.pid) continue;
		try {
			if (
				readFileSync(`/proc/${d}/environ`)
					.toString()
					.split('\0')
					.includes(`STARFOLD_TEST_RUN=${marker}`)
			) {
				found.push(Number(d));
			}
		} catch {
			// gone, or not ours to read
		}
	}
	return found;
}

function start(cmd, args, opts = {}) {
	const p = spawn(cmd, args, {
		env: baseEnv,
		stdio: 'ignore',
		detached: true,
		...opts,
	});
	procs.push(p);
	return p;
}

async function until(what, ms, ready) {
	const end = Date.now() + ms;
	for (;;) {
		// oxlint-disable-next-line no-await-in-loop
		if (await ready()) return;
		assert.ok(Date.now() < end, what);
		// oxlint-disable-next-line no-await-in-loop
		await sleep(100);
	}
}

async function healthy(url) {
	try {
		return (
			(await fetch(`${url}/healthz`, { signal: AbortSignal.timeout(1000) }))
				.status === 200
		);
	} catch {
		return false;
	}
}

const work = mkdtempSync(join(tmpdir(), 'starfold-it-'));

// Xvfb picks a free display and writes its number, once it takes connections, to -displayfd: here
// a file, read up to the newline. (Without /tmp/.X11-unix, a user's Xvfb listens on the abstract
// socket only, so waiting for the socket file would never end.)
const displayFile = join(work, 'display');
const xvfb = start('sh', [
	'-c',
	'exec Xvfb -displayfd 3 -screen 0 1280x800x24 -nolisten tcp 3>"$0"',
	displayFile,
]);
await until('Xvfb did not name its display', 15_000, () => {
	assert.equal(xvfb.exitCode, null, 'Xvfb exited');
	return (
		existsSync(displayFile) && readFileSync(displayFile, 'utf8').endsWith('\n')
	);
});
const display = `:${readFileSync(displayFile, 'utf8').trim()}`;
baseEnv.DISPLAY = display;

const results = [];
async function check(name, fn) {
	const t = Date.now();
	try {
		await fn();
		results.push([name, 'pass']);
		console.log(`pass ${name} (${Date.now() - t} ms)`);
	} catch (e) {
		results.push([name, 'FAIL']);
		console.log(`FAIL ${name}: ${e?.stack ?? e}`);
	}
}

/** A server started by hand as the Python test does, with an xterm in front of it. */
async function desktop(fn) {
	const port = 20_000 + Math.floor(Math.random() * 20_000);
	const password = randomBytes(16).toString('hex');
	const files = join(work, `files-${port}`);
	mkdirSync(files);
	const server = start(
		process.env.STARFOLD_BIN,
		[
			`--port=${port}`,
			'--addr=127.0.0.1',
			'--audio-enabled=false',
			'--enable-recording',
			`--files-root=${files}`,
		],
		{ env: { ...baseEnv, STARFOLD_PASSWORD: password } }
	);
	const url = `http://127.0.0.1:${port}`;
	const sf = new Starfold(url, { password, timeoutMs: 15_000 });
	await until('server did not come up', 20_000, () => {
		assert.equal(server.exitCode, null, 'server exited');
		return healthy(url);
	});
	const before = (await sf.wait({ idleMs: 300 })).seq;
	const xterm = start('xterm', ['-geometry', '80x24+0+0', '-e', 'sh'], {
		cwd: work,
	});
	assert.ok(
		(await sf.wait({ change: true, since: before, timeoutMs: 15_000 })).changed,
		'xterm never drew'
	);
	await sf.wait({ idleMs: 300 });
	try {
		await fn(sf, files);
	} finally {
		for (const p of [xterm, server]) {
			try {
				process.kill(-p.pid, 'SIGTERM');
			} catch {
				// gone
			}
		}
		await sleep(500);
	}
}

await check('type into xterm and see it', () =>
	desktop(async (sf) => {
		const shot = await sf.screenshot();
		assert.deepEqual(shot.size, [1280, 800]);
		await sf.click(100, 100);
		const typed = join(work, 'typed.txt');
		const r = await sf.typeText(`printf 'Hello, World!' > ${typed}`);
		await sf.key('Return');
		const w = await sf.wait({ change: true, since: r.seq, timeoutMs: 5000 });
		assert.ok(w.changed);
		assert.equal(w.timedOut, false);
		assert.ok(w.seq > r.seq);
		await until(
			'nothing typed',
			10_000,
			() => existsSync(typed) && readFileSync(typed, 'utf8') === 'Hello, World!'
		);
		assert.equal(
			(await sf.wait({ idleMs: 300, timeoutMs: 5000 })).timedOut,
			false
		);
		const after = await sf.screenshot();
		assert.ok(after.seq > shot.seq);
		assert.notDeepEqual(after.png, shot.png);
		await sf.scroll(0, 1, 100, 100);
		await sf.move(200, 200);
		await sf.drag(10, 10, 50, 50);
		await sf.doubleClick(100, 100);
		await sf.mouseDown(100, 100);
		await sf.mouseUp(100, 100);
	})
);

await check('files and recording', () =>
	desktop(async (sf, files) => {
		const up = await sf.filesUpload(
			'note.txt',
			new TextEncoder().encode('from the sdk'),
			''
		);
		assert.equal(up.size, 12);
		assert.equal(readFileSync(join(files, 'note.txt'), 'utf8'), 'from the sdk');
		assert.ok(
			(await sf.filesList('')).entries.map((e) => e.name).includes('note.txt')
		);
		assert.equal(
			new TextDecoder().decode(await sf.filesDownload('note.txt')),
			'from the sdk'
		);

		const rec = await sf.recordStart();
		await sf.typeText('echo recording\n');
		await sf.wait({ idleMs: 300 });
		const clips = await sf.recordStop(rec);
		assert.equal(clips.length, 1);
		assert.equal(new TextDecoder().decode(clips[0].subarray(4, 8)), 'ftyp');
		assert.equal(clips.endReason, 'stopped');

		const short = await sf.record(1);
		assert.ok(short.length >= 1);
		assert.equal(new TextDecoder().decode(short[0].subarray(4, 8)), 'ftyp');
	})
);

for (const p of procs.toReversed()) {
	try {
		process.kill(-p.pid, 'SIGKILL');
	} catch {
		// gone
	}
}
await sleep(300);
const left = ours();
console.log(
	`${runtime}: ${results.filter((r) => r[1] === 'pass').length}/${results.length} passed; left behind: [${left}]`
);
process.exit(
	results.every((r) => r[1] === 'pass') && left.length === 0 ? 0 : 1
);
