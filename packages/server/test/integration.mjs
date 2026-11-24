// startServer (the built dist/) with a real server on Xvfb, under any of Node, Bun and Deno.
// Needs Linux with Xvfb, and STARFOLD_BIN pointing at a starfold binary; skips without them:
//
//   STARFOLD_BIN=target/release/starfold node test/integration.mjs   (or bun, or deno run -A)
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import {
	existsSync,
	mkdtempSync,
	readFileSync,
	readdirSync,
	writeFileSync,
} from 'node:fs';
import { networkInterfaces, tmpdir } from 'node:os';
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
	!have('Xvfb')
) {
	console.log('skip: set STARFOLD_BIN and install Xvfb to run (Linux)');
	process.exit(0);
}

const { startServer } = await import(dist);
const { AuthError, Starfold } = await import('starfold');
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

const work = mkdtempSync(join(tmpdir(), 'starfold-server-it-'));

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

const serverEnv = { DISPLAY: display, STARFOLD_TEST_RUN: marker };
const serverArgs = ['--audio-enabled=false'];

/** The server process under a LocalServer: the guard's child that runs STARFOLD_BIN. */
function serverPid() {
	for (const pid of ours()) {
		try {
			const argv = readFileSync(`/proc/${pid}/cmdline`).toString().split('\0');
			if (argv[0] === process.env.STARFOLD_BIN) return { pid, argv };
		} catch {
			// gone
		}
	}
	return null;
}

await check('startServer binds to loopback by default', async () => {
	const s = await startServer({ env: serverEnv, args: serverArgs });
	try {
		const u = new URL(s.url);
		assert.equal(u.hostname, '127.0.0.1');
		assert.ok(await healthy(s.url));
		const { argv } = serverPid();
		assert.ok(argv.includes('--addr=127.0.0.1'), argv.join(' '));
		const outside = Object.values(networkInterfaces())
			.flat()
			.find((i) => i && i.family === 'IPv4' && !i.internal);
		if (outside) {
			assert.equal(
				await healthy(`http://${outside.address}:${u.port}`),
				false,
				`answers on ${outside.address}`
			);
			console.log(`  not reachable on ${outside.address}:${u.port}`);
		}
	} finally {
		await s.close();
	}
});

await check('startServer generates a password the client holds', async () => {
	const s = await startServer({ env: serverEnv, args: serverArgs });
	try {
		assert.match(s.password, /^[0-9a-f]{32}$/);
		const client = s.client();
		assert.ok(
			client instanceof Starfold,
			'client() is not the starfold Starfold'
		);
		const shot = await client.screenshot();
		assert.deepEqual(shot.size, [1280, 800]);
		await assert.rejects(
			new Starfold(s.url, { password: '' }).screenshot(),
			AuthError
		);
		await assert.rejects(
			new Starfold(s.url, { password: 'wrong' }).screenshot(),
			AuthError
		);
		const { argv } = serverPid();
		assert.ok(
			!argv.join(' ').includes(s.password),
			'the password is on the command line'
		);
		const other = await startServer({ env: serverEnv, args: serverArgs });
		assert.notEqual(other.password, s.password);
		await other.close();
	} finally {
		await s.close();
	}
	await until('the server outlived close()', 5000, () => serverPid() === null);
});

await check(
	'startServer leaves no orphan after the parent is killed',
	async () => {
		const script = join(work, 'parent.mjs');
		writeFileSync(
			script,
			`const { startServer } = await import(${JSON.stringify(dist)});
const s = await startServer({ env: ${JSON.stringify(serverEnv)}, args: ${JSON.stringify(serverArgs)} });
console.log(s.url);
setInterval(() => {}, 1000);
`
		);
		const cmd =
			runtime === 'deno'
				? [process.execPath, 'run', '-A', script]
				: [process.execPath, script];
		const parent = spawn(cmd[0], cmd.slice(1), {
			env: baseEnv,
			stdio: ['ignore', 'pipe', 'inherit'],
		});
		const url = await new Promise((ok, fail) => {
			parent.stdout.once('data', (d) => ok(String(d).trim()));
			parent.once('exit', (c) => fail(new Error(`parent exited with ${c}`)));
		});
		assert.ok(await healthy(url));
		const server = serverPid();
		assert.ok(server, 'no server process');
		parent.kill('SIGKILL');
		await until(
			'the server survived its parent',
			10_000,
			async () => !(await healthy(url)) && serverPid() === null
		);
		const others = () =>
			ours().flatMap((p) => {
				try {
					const line = readFileSync(`/proc/${p}/cmdline`)
						.toString()
						.replaceAll('\0', ' ')
						.trim();
					const state = /^State:\s+(\S)/m.exec(
						readFileSync(`/proc/${p}/status`, 'utf8')
					)?.[1];
					return /^(\S*\/)?Xvfb /.test(line) || state === 'Z'
						? []
						: [`${p} ${state} ${line}`];
				} catch {
					return [];
				}
			});
		try {
			await until(
				'processes outlived the parent',
				5000,
				() => others().length === 0
			);
		} catch (e) {
			throw new Error(`${e.message}: ${others().join('; ')}`, { cause: e });
		}
		console.log(
			`  server ${server.pid} gone after SIGKILL of parent ${parent.pid}`
		);
	}
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
