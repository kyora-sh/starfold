import { afterEach, describe, expect, test } from 'bun:test';
import { spawn } from 'node:child_process';
import {
	chmodSync,
	existsSync,
	mkdirSync,
	mkdtempSync,
	readFileSync,
	writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { StarfoldError } from 'starfold';
import * as native from '../src/index.ts';

const here = import.meta.dir;
const posix = process.platform !== 'win32';
const linux = process.platform === 'linux';

function scratch(): string {
	return mkdtempSync(join(tmpdir(), 'sf-edges-'));
}

function fakeBinary(dir: string): string {
	const bin = join(dir, 'fake-starfold');
	writeFileSync(
		bin,
		`#!/bin/sh\nexec "${process.execPath}" "${join(here, 'fake-server-bin.ts')}" "$@"\n`
	);
	chmodSync(bin, 0o755);
	return bin;
}

async function caught(p: Promise<unknown>): Promise<Error> {
	try {
		await p;
	} catch (e) {
		return e as Error;
	}
	throw new Error('expected a rejection');
}

describe.if(posix)('startServer', () => {
	let server: native.LocalServer | null = null;
	afterEach(async () => {
		await server?.close();
		server = null;
	});

	test('an address that makes no URL is named, not --base-path', async () => {
		const dir = scratch();
		const e = await caught(
			native.startServer({
				binary: fakeBinary(dir),
				addr: 'not an addr',
				port: 4567,
				env: { FAKE_RECORD: join(dir, 'rec.json') },
			})
		);
		expect(e.name).toBe('ServerStartError');
		expect(e.message).toContain('not an addr');
		expect(e.message).not.toContain('--base-path');
	});

	test('close() after the server exited ends the guard stdin', async () => {
		const dir = scratch();
		server = await native.startServer({
			binary: fakeBinary(dir),
			env: { FAKE_RECORD: join(dir, 'rec.json'), FAKE_EXIT_AFTER: '200' },
		});
		const guard = server.process;
		for (let i = 0; i < 50 && guard.exitCode === null; i++) {
			// oxlint-disable-next-line no-await-in-loop
			await Bun.sleep(100);
		}
		expect(guard.exitCode).toBe(0);
		await server.close();
		expect(guard.stdin?.destroyed).toBe(true);
	});

	test.if(process.platform === 'linux')(
		'a server that exits at once leaves nothing in the guard group (30 runs)',
		async () => {
			const dir = scratch();
			const bin = join(dir, 'exits-at-once');
			const groups = join(dir, 'groups');
			writeFileSync(
				bin,
				`#!/bin/sh\nps -o pgid= -p $$ | tr -d ' ' >> "${groups}"\nexit 3\n`
			);
			chmodSync(bin, 0o755);
			for (let i = 0; i < 30; i++) {
				// oxlint-disable-next-line no-await-in-loop
				await caught(native.startServer({ binary: bin, timeoutMs: 3000 }));
			}
			const ids = readFileSync(groups, 'utf8').trim().split('\n');
			expect(ids.length).toBe(30);
			// Give a stopping watcher a moment, then count who is still in each guard's group.
			await Bun.sleep(1000);
			const left = ids.filter((g) =>
				Bun.spawnSync(['ps', '-o', 'pid=', '-g', g]).stdout.toString().trim()
			);
			expect({ groupsLeft: left.length, of: ids.length }).toEqual({
				groupsLeft: 0,
				of: 30,
			});
		},
		60_000
	);

	for (const args of [['--base-path=/desk/'], ['--base-path', '/desk']]) {
		test(`waits for the server under ${args.join(' ')}`, async () => {
			const dir = scratch();
			server = await native.startServer({
				binary: fakeBinary(dir),
				args,
				timeoutMs: 3000,
				env: { FAKE_RECORD: join(dir, 'rec.json') },
			});
			expect(server.url).toEndWith('/desk');
			expect(await server.client().key('a')).toEqual({ seq: 1, events: 1 });
		});
	}

	test('a server that sends its headers and then stalls fails within the timeout', async () => {
		const dir = scratch();
		const started = Date.now();
		const e = await caught(
			native.startServer({
				binary: fakeBinary(dir),
				timeoutMs: 1500,
				env: { FAKE_RECORD: join(dir, 'rec.json'), FAKE_STALL: '1' },
			})
		);
		expect(e.name).toBe('ServerStartError');
		expect(Date.now() - started).toBeLessThan(5000);
	}, 10_000);

	test('a --base-path that makes no URL is a ServerStartError, the server closed', async () => {
		const dir = scratch();
		const rec = join(dir, 'rec.json');
		const e = await caught(
			native.startServer({
				binary: fakeBinary(dir),
				args: ['--base-path=desktop'],
				timeoutMs: 3000,
				env: { FAKE_RECORD: rec },
			})
		);
		expect(e).toBeInstanceOf(StarfoldError);
		expect(e.name).toBe('ServerStartError');
		expect(e.message).toContain('--base-path');
		if (existsSync(rec)) {
			const { pid } = JSON.parse(readFileSync(rec, 'utf8')) as { pid: number };
			let alive = true;
			for (let i = 0; i < 50 && alive; i++) {
				try {
					process.kill(pid, 0);
					// oxlint-disable-next-line no-await-in-loop
					await Bun.sleep(100);
				} catch {
					alive = false;
				}
			}
			expect(alive).toBe(false);
		}
	});

	test('an empty password counts as none: one is generated', async () => {
		const dir = scratch();
		server = await native.startServer({
			binary: fakeBinary(dir),
			password: '',
			env: { FAKE_RECORD: join(dir, 'rec.json') },
		});
		expect(server.password).toMatch(/^[0-9a-f]{32}$/);
		expect(await server.client().key('a')).toEqual({ seq: 1, events: 1 });
	});

	test('the readiness poll takes no proxy from the environment', async () => {
		const dir = scratch();
		const saved = process.env.HTTP_PROXY;
		process.env.HTTP_PROXY = 'http://127.0.0.1:9';
		try {
			server = await native.startServer({
				binary: fakeBinary(dir),
				timeoutMs: 3000,
				env: { FAKE_RECORD: join(dir, 'rec.json') },
			});
		} finally {
			if (saved === undefined) delete process.env.HTTP_PROXY;
			else process.env.HTTP_PROXY = saved;
		}
		expect(server.url).toStartWith('http://127.0.0.1:');
	});

	test('a port taken before the server binds it is tried again', async () => {
		const dir = scratch();
		const taken = join(dir, 'taken');
		writeFileSync(taken, '');
		server = await native.startServer({
			binary: fakeBinary(dir),
			env: { FAKE_RECORD: join(dir, 'rec.json'), FAKE_TAKEN: taken },
		});
		expect(await server.client().key('a')).toEqual({ seq: 1, events: 1 });
	});

	test('a fixed port that is taken is not tried again', async () => {
		const dir = scratch();
		const taken = join(dir, 'taken');
		writeFileSync(taken, '');
		const e = await caught(
			native.startServer({
				binary: fakeBinary(dir),
				port: 1,
				env: { FAKE_RECORD: join(dir, 'rec.json'), FAKE_TAKEN: taken },
			})
		);
		expect(e).toBeInstanceOf(StarfoldError);
		expect(e.name).toBe('ServerStartError');
		expect(e.message).toContain('Address already in use');
	});

	test('a server that exits early is a ServerStartError with its output', async () => {
		const dir = scratch();
		const e = await caught(
			native.startServer({
				binary: fakeBinary(dir),
				env: { FAKE_EXIT: '3', FAKE_RECORD: join(dir, 'r') },
			})
		);
		expect(e).toBeInstanceOf(StarfoldError);
		expect(e.name).toBe('ServerStartError');
		expect((e as Error & { output?: string }).output).toContain(
			'fake: cannot open display'
		);
	});
});

describe('the errors of the native surface are StarfoldErrors', () => {
	const saved = process.env.STARFOLD_BIN;
	afterEach(() => {
		if (saved === undefined) delete process.env.STARFOLD_BIN;
		else process.env.STARFOLD_BIN = saved;
	});

	test('no binary: BinaryNotFoundError', () => {
		process.env.STARFOLD_BIN = '/nonexistent/starfold';
		let e: Error | null = null;
		try {
			native.binaryPath();
		} catch (x) {
			e = x as Error;
		}
		expect(e).toBeInstanceOf(StarfoldError);
		expect(e?.name).toBe('BinaryNotFoundError');
	});

	test('a directory is not a binary', () => {
		const dir = scratch();
		process.env.STARFOLD_BIN = dir;
		expect(() => native.binaryPath()).toThrow(
			`STARFOLD_BIN is ${dir}, which isn't a file`
		);
	});

	test('missing libraries: MissingLibrariesError', () => {
		const e = new native.MissingLibrariesError(['libyuv.so.0']);
		expect(e).toBeInstanceOf(StarfoldError);
		expect(e.name).toBe('MissingLibrariesError');
	});
});

describe.if(linux)('the Linux library check', () => {
	const savedPath = process.env.PATH;
	afterEach(() => {
		process.env.PATH = savedPath;
	});

	test('reads ldd stderr too, glibc versions included', () => {
		const dir = scratch();
		const bin = join(dir, 'bin');
		mkdirSync(bin);
		writeFileSync(
			join(bin, 'ldd'),
			'#!/bin/sh\n' +
				'[ "$1" = -- ] || { echo "ldd: no -- before $1" >&2; exit 9; }\n' +
				'echo "\tlibyuv.so.0 => not found"\n' +
				`echo "$2: /lib/x86_64-linux-gnu/libc.so.6: version \\\`GLIBC_2.39' not found (required by $2)" >&2\n` +
				'exit 1\n'
		);
		chmodSync(join(bin, 'ldd'), 0o755);
		process.env.PATH = `${bin}:${savedPath}`;
		const missing = native.missingLibraries('/some/starfold');
		expect(missing).toEqual(['libyuv.so.0', 'GLIBC_2.39']);
		const e = new native.MissingLibrariesError(missing);
		expect(e.message).toContain('GLIBC_2.39');
		expect(e.message).toContain('Ubuntu 24.04');
	});
});

describe.if(posix)('the starfold command', () => {
	test('a SIGINT sent to it reaches the server', async () => {
		const dir = scratch();
		const bin = join(dir, 'server');
		const marker = join(dir, 'interrupted');
		writeFileSync(
			bin,
			`#!/bin/sh\ntrap 'echo int > "${marker}"; exit 0' INT\necho ready\nwhile :; do sleep 0.05; done\n`
		);
		chmodSync(bin, 0o755);
		const wrapper = spawn(
			process.execPath,
			[resolve(here, '../bin/starfold.js')],
			{
				env: { ...process.env, STARFOLD_BIN: bin },
				stdio: ['ignore', 'pipe', 'inherit'],
				detached: true,
			}
		);
		const exited = new Promise<string>((ok) =>
			wrapper.once('exit', (code, signal) => ok(`${code ?? signal}`))
		);
		let out = '';
		await new Promise<void>((ok) =>
			wrapper.stdout!.on('data', (d) => {
				out += String(d);
				if (out.includes('ready')) ok();
			})
		);
		// The server's trap is in place once it printed; the wrapper's handlers since it spawned.
		await Bun.sleep(200);
		process.kill(wrapper.pid!, 'SIGINT');
		const ended = await Promise.race([
			exited,
			Bun.sleep(3000).then(() => 'running'),
		]);
		if (ended === 'running') process.kill(-wrapper.pid!, 'SIGKILL');
		expect({ ended, interrupted: await Bun.file(marker).exists() }).toEqual({
			ended: '0',
			interrupted: true,
		});
	});
});
