import { afterEach, describe, expect, test } from 'bun:test';
import { spawn, spawnSync } from 'node:child_process';
import {
	chmodSync,
	existsSync,
	mkdtempSync,
	readFileSync,
	statSync,
	writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { AuthError, Starfold } from 'starfold';
import {
	LINUX_LIBRARIES_HINT,
	MissingLibrariesError,
	binaryPath,
	missingLibraries,
	platformKey,
	serverPackage,
	startServer,
} from '../src/index.ts';
import type { LocalServer } from '../src/index.ts';

const here = import.meta.dir;
const posix = process.platform !== 'win32';

function scratch(): string {
	return mkdtempSync(join(tmpdir(), 'sf-native-'));
}

/** An executable that runs the fake server with this runtime. */
function fakeBinary(dir: string): string {
	const bin = join(dir, 'fake-starfold');
	writeFileSync(
		bin,
		`#!/bin/sh\nexec "${process.execPath}" "${join(here, 'fake-server-bin.ts')}" "$@"\n`
	);
	chmodSync(bin, 0o755);
	return bin;
}

function alive(pid: number): boolean {
	try {
		process.kill(pid, 0);
		return true;
	} catch {
		return false;
	}
}

async function gone(pid: number, ms = 5000): Promise<boolean> {
	const end = Date.now() + ms;
	while (Date.now() < end) {
		if (!alive(pid)) return true;
		// oxlint-disable-next-line no-await-in-loop
		await Bun.sleep(50);
	}
	return !alive(pid);
}

interface Record {
	pid: number;
	argv: string[];
	password: string;
	port: number;
}

const record = (path: string) =>
	JSON.parse(readFileSync(path, 'utf8')) as Record;

describe('binaryPath', () => {
	const saved = process.env.STARFOLD_BIN;
	afterEach(() => {
		if (saved === undefined) delete process.env.STARFOLD_BIN;
		else process.env.STARFOLD_BIN = saved;
	});

	test('STARFOLD_BIN wins', () => {
		const bin = fakeBinary(scratch());
		process.env.STARFOLD_BIN = bin;
		expect(binaryPath()).toBe(bin);
	});

	test('a STARFOLD_BIN that is not there', () => {
		process.env.STARFOLD_BIN = '/nonexistent/starfold';
		expect(() => binaryPath()).toThrow(
			"STARFOLD_BIN is /nonexistent/starfold, which isn't a file"
		);
	});

	test.if(posix)('a binary without its exec bit gets it back', () => {
		const bin = fakeBinary(scratch());
		chmodSync(bin, 0o644);
		process.env.STARFOLD_BIN = bin;
		expect(binaryPath()).toBe(bin);
		expect(statSync(bin).mode & 0o111).toBe(0o111);
	});

	test('without STARFOLD_BIN or a server package, the error names the package and the remote option', () => {
		delete process.env.STARFOLD_BIN;
		const pkg = serverPackage();
		const message = (() => {
			try {
				binaryPath();
				return '';
			} catch (e) {
				return (e as Error).message;
			}
		})();
		if (pkg === null) {
			expect(message).toContain(`no server binary for ${platformKey()}`);
		} else {
			expect(message).toContain(`comes in ${pkg}, which is not installed`);
		}
		expect(message).toContain('new Starfold(url)');
	});

	test('platform packages', () => {
		expect(serverPackage('linux-x64-gnu')).toBe(
			'@starfold/server-linux-x64-gnu'
		);
		expect(serverPackage('linux-arm64-gnu')).toBe(
			'@starfold/server-linux-arm64-gnu'
		);
		expect(serverPackage('darwin-arm64')).toBe('@starfold/server-darwin-arm64');
		expect(serverPackage('win32-x64')).toBe('@starfold/server-win32-x64');
		expect(serverPackage('linux-x64-musl')).toBeNull();
		expect(serverPackage('darwin-x64')).toBeNull();
		if (process.platform === 'linux') {
			expect(platformKey()).toMatch(/^linux-(x64|arm64)-(gnu|musl)$/);
		}
	});
});

describe('the Linux library check', () => {
	test('the error names what is missing and the apt line', () => {
		const e = new MissingLibrariesError(['libyuv.so.0', 'libopus.so.0']);
		expect(e.missing).toEqual(['libyuv.so.0', 'libopus.so.0']);
		expect(e.message).toContain('libyuv.so.0, libopus.so.0');
		expect(e.message).toContain('sudo apt install libyuv0 libopus0 libpulse0');
		expect(LINUX_LIBRARIES_HINT).toBe(
			'sudo apt install libyuv0 libopus0 libpulse0 (glibc 2.39+, Ubuntu 24.04 or newer)'
		);
	});

	test('a binary whose libraries all resolve', () => {
		expect(missingLibraries(process.execPath)).toEqual([]);
	});
});

describe.if(posix)('startServer', () => {
	let server: LocalServer | null = null;
	afterEach(async () => {
		await server?.close();
		server = null;
	});

	test('loopback and a generated password by default', async () => {
		const dir = scratch();
		const rec = join(dir, 'rec.json');
		server = await startServer({
			binary: fakeBinary(dir),
			env: { FAKE_RECORD: rec },
		});
		const r = record(rec);
		expect(r.argv).toEqual(['--addr=127.0.0.1', `--port=${r.port}`]);
		expect(server.url).toBe(`http://127.0.0.1:${r.port}`);
		expect(server.password).toMatch(/^[0-9a-f]{32}$/);
		expect(r.password).toBe(server.password);
		expect(r.argv.join(' ')).not.toContain(server.password);
		expect(await server.client().key('a')).toEqual({ seq: 1, events: 1 });
		const stranger = server.client();
		expect(stranger.url).toBe(server.url);
		expect(
			new Starfold(server.url, { password: 'wrong' }).key('a')
		).rejects.toBeInstanceOf(AuthError);
		const other = await startServer({
			binary: fakeBinary(dir),
			env: { FAKE_RECORD: join(dir, 'rec2.json') },
		});
		expect(other.password).not.toBe(server.password);
		await other.close();
	});

	test('address, port, password and flags as given', async () => {
		const dir = scratch();
		const rec = join(dir, 'rec.json');
		server = await startServer({
			binary: fakeBinary(dir),
			addr: '0.0.0.0',
			password: 'chosen',
			args: ['--enable-recording'],
			env: { FAKE_RECORD: rec },
		});
		const r = record(rec);
		expect(r.argv).toEqual([
			'--addr=0.0.0.0',
			`--port=${r.port}`,
			'--enable-recording',
		]);
		expect(r.password).toBe('chosen');
		expect(server.url).toBe(`http://127.0.0.1:${r.port}`);
	});

	test('close stops the server', async () => {
		const dir = scratch();
		const rec = join(dir, 'rec.json');
		server = await startServer({
			binary: fakeBinary(dir),
			env: { FAKE_RECORD: rec },
		});
		const { pid } = record(rec);
		expect(alive(pid)).toBe(true);
		await server.close();
		expect(await gone(pid)).toBe(true);
		expect(server.process.exitCode).toBe(0);
		await server.close();
	});

	test('a server that exits early, with its stderr', async () => {
		const dir = scratch();
		const e = await startServer({
			binary: fakeBinary(dir),
			env: { FAKE_EXIT: '3', FAKE_RECORD: join(dir, 'r') },
		}).catch((x: unknown) => x as Error);
		expect(e).toBeInstanceOf(Error);
		expect((e as Error).message).toContain(
			'exited (code 3) before it answered'
		);
		expect((e as Error).message).toContain('fake: cannot open display');
	});

	test('a server that never answers', async () => {
		const dir = scratch();
		const bin = join(dir, 'silent');
		writeFileSync(
			bin,
			'#!/bin/sh\necho "$$" > "$FAKE_RECORD"\nexec sleep 60\n'
		);
		chmodSync(bin, 0o755);
		const rec = join(dir, 'pid');
		const e = await startServer({
			binary: bin,
			timeoutMs: 500,
			env: { FAKE_RECORD: rec },
		}).catch((x: unknown) => x as Error);
		expect((e as Error).message).toContain('did not answer within 500 ms');
		expect(await gone(Number(readFileSync(rec, 'utf8')))).toBe(true);
	});

	test('no orphan after the parent is killed', async () => {
		const dir = scratch();
		const rec = join(dir, 'rec.json');
		const script = join(dir, 'parent.ts');
		writeFileSync(
			script,
			`import { startServer } from ${JSON.stringify(resolve(here, '../src/index.ts'))};
const s = await startServer({ binary: ${JSON.stringify(fakeBinary(dir))}, env: { FAKE_RECORD: ${JSON.stringify(rec)} } });
console.log(s.url);
setInterval(() => {}, 1000);
`
		);
		const parent = spawn(process.execPath, [script], {
			stdio: ['ignore', 'pipe', 'inherit'],
		});
		const url = await new Promise<string>((ok) =>
			parent.stdout!.once('data', (d) => ok(String(d).trim()))
		);
		const { pid } = record(rec);
		expect(alive(pid)).toBe(true);
		expect((await fetch(`${url}/healthz`)).status).toBe(200);
		parent.kill('SIGKILL');
		expect(await gone(pid)).toBe(true);
		expect(fetch(`${url}/healthz`)).rejects.toThrow();
	});
});

describe.if(posix)('the starfold command', () => {
	test('runs the binary with the arguments and passes the exit code through', () => {
		const dir = scratch();
		const bin = join(dir, 'fake');
		writeFileSync(bin, '#!/bin/sh\necho "args: $*"\nexit 3\n');
		chmodSync(bin, 0o755);
		const launcher = resolve(here, '../bin/starfold.js');
		expect(existsSync(resolve(here, '../dist/native.js'))).toBe(true);
		const r = spawnSync(process.execPath, [launcher, '--version', 'x y'], {
			env: { ...process.env, STARFOLD_BIN: bin },
			encoding: 'utf8',
		});
		expect(r.stdout).toBe('args: --version x y\n');
		expect(r.status).toBe(3);
	});

	test('a missing binary is an error on stderr with exit code 1', () => {
		const r = spawnSync(
			process.execPath,
			[resolve(here, '../bin/starfold.js')],
			{
				env: { ...process.env, STARFOLD_BIN: '/nonexistent/starfold' },
				encoding: 'utf8',
			}
		);
		expect(r.stderr).toContain(
			'starfold: STARFOLD_BIN is /nonexistent/starfold'
		);
		expect(r.status).toBe(1);
	});
});
