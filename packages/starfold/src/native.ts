import { spawn, spawnSync } from 'node:child_process';
import type { ChildProcess } from 'node:child_process';
import {
	accessSync,
	chmodSync,
	constants,
	existsSync,
	readdirSync,
} from 'node:fs';
import { createRequire } from 'node:module';
import { createServer } from 'node:net';
import { dirname, join } from 'node:path';
import { Starfold } from './client.js';
import type { StarfoldOptions } from './client.js';

/** The apt line for the shared libraries the Linux binary links. */
export const LINUX_LIBRARIES_HINT =
	'sudo apt install libyuv0 libopus0 libpulse0 (glibc 2.39+, Ubuntu 24.04 or newer)';

const PACKAGES: Record<string, string> = {
	'linux-x64-gnu': '@starfold/server-linux-x64-gnu',
	'linux-arm64-gnu': '@starfold/server-linux-arm64-gnu',
	'darwin-arm64': '@starfold/server-darwin-arm64',
	'win32-x64': '@starfold/server-win32-x64',
};

const SUPPORTED = 'Linux x64 and arm64 with glibc, macOS arm64, Windows x64';

function glibc(): boolean {
	const report = (
		process as {
			report?: {
				getReport?: () => { header?: { glibcVersionRuntime?: string } };
			};
		}
	).report;
	if (typeof report?.getReport === 'function') {
		try {
			return Boolean(report.getReport().header?.glibcVersionRuntime);
		} catch {
			// fall through to the loader check
		}
	}
	try {
		return !readdirSync('/lib').some((n) => n.startsWith('ld-musl-'));
	} catch {
		return true;
	}
}

/** The platform's key in the server packages' names, such as linux-x64-gnu. */
export function platformKey(): string {
	const base = `${process.platform}-${process.arch}`;
	if (process.platform !== 'linux') {
		return base;
	}
	return `${base}-${glibc() ? 'gnu' : 'musl'}`;
}

/** The server package for this platform, or null if there is none. */
export function serverPackage(key: string = platformKey()): string | null {
	return PACKAGES[key] ?? null;
}

function executable(path: string): string {
	if (process.platform === 'win32') {
		return path;
	}
	try {
		accessSync(path, constants.X_OK);
	} catch {
		// A tarball that lost the mode bits: make it runnable again.
		chmodSync(path, 0o755);
	}
	return path;
}

/**
 * The server binary: $STARFOLD_BIN if set, else the one in this platform's
 * @starfold/server-* package. Throws if neither is there; the client still works
 * against a remote server without it.
 */
export function binaryPath(): string {
	const env = process.env.STARFOLD_BIN;
	if (env) {
		if (!existsSync(env)) {
			throw new Error(`STARFOLD_BIN is ${env}, which does not exist`);
		}
		return executable(env);
	}
	const key = platformKey();
	const pkg = serverPackage(key);
	const remote =
		'The client still works against a remote server: new Starfold(url).';
	if (pkg === null) {
		throw new Error(
			`starfold has no server binary for ${key} (there are packages for ${SUPPORTED}); ` +
				`set STARFOLD_BIN to a starfold binary built for it. ${remote}`
		);
	}
	let dir: string;
	try {
		dir = dirname(
			createRequire(import.meta.url).resolve(`${pkg}/package.json`)
		);
	} catch {
		throw new Error(
			`the server binary for ${key} comes in ${pkg}, which is not installed ` +
				`(were optional dependencies left out?); install it, or set STARFOLD_BIN. ${remote}`
		);
	}
	const bin = join(
		dir,
		'bin',
		process.platform === 'win32' ? 'starfold.exe' : 'starfold'
	);
	if (!existsSync(bin)) {
		throw new Error(`${pkg} has no ${bin}; reinstall it, or set STARFOLD_BIN`);
	}
	return executable(bin);
}

/** On Linux, the shared libraries `binary` needs that do not resolve (by ldd); elsewhere none. */
export function missingLibraries(binary: string): string[] {
	if (process.platform !== 'linux') {
		return [];
	}
	const r = spawnSync('ldd', [binary], { encoding: 'utf8' });
	if (r.error || typeof r.stdout !== 'string') {
		return [];
	}
	const missing: string[] = [];
	for (const line of r.stdout.split('\n')) {
		const m = /^\s*(\S+)\s+=>\s+not found/.exec(line);
		if (m) {
			missing.push(m[1]!);
		}
	}
	return missing;
}

/** The shared libraries the server binary needs are not installed. */
export class MissingLibrariesError extends Error {
	readonly missing: string[];

	constructor(missing: string[]) {
		super(
			`the starfold server needs shared libraries this system lacks (${missing.join(', ')}); ` +
				`install them with: ${LINUX_LIBRARIES_HINT}`
		);
		this.name = 'MissingLibrariesError';
		this.missing = missing;
	}
}

/** Throws a MissingLibrariesError naming what `binary` lacks (Linux only). */
export function checkLibraries(binary: string): void {
	const missing = missingLibraries(binary);
	if (missing.length > 0) {
		throw new MissingLibrariesError(missing);
	}
}

export interface StartServerOptions {
	/** The address to bind (default 127.0.0.1, loopback only). */
	addr?: string;
	/** The port (default: a free one). */
	port?: number;
	/** The password (default: a generated one, which `client()` holds). */
	password?: string;
	/** More server flags, such as --enable-recording or --read-only. */
	args?: string[];
	/** Extra environment for the server (DISPLAY, for one). */
	env?: Record<string, string>;
	/** The binary (default: binaryPath()). */
	binary?: string;
	/** How long to wait for the server to answer (default 30000). */
	timeoutMs?: number;
}

// Runs the server with the guard's stdin, a pipe from this process, held by a watcher: when
// that pipe closes (this process ended, however it ended) the watcher stops the server.
const GUARD = `exec 3<&0 0</dev/null
"$0" "$@" 3<&- &
p=$!
{ while read -r _ <&3; do :; done; kill "$p" 2>/dev/null; } &
w=$!
exec 3<&-
wait "$p"
rc=$?
kill "$w" 2>/dev/null
exit "$rc"`;

const TAIL_BYTES = 16 * 1024;

function freePort(addr: string): Promise<number> {
	return new Promise((resolve, reject) => {
		const s = createServer();
		s.once('error', reject);
		s.listen(0, addr, () => {
			const a = s.address();
			s.close(() =>
				typeof a === 'object' && a
					? resolve(a.port)
					: reject(new Error('no port'))
			);
		});
	});
}

function generatedPassword(): string {
	const b = new Uint8Array(16);
	crypto.getRandomValues(b);
	return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
}

function urlHost(addr: string): string {
	if (addr === '0.0.0.0' || addr === '') return '127.0.0.1';
	if (addr === '::') return '[::1]';
	return addr.includes(':') ? `[${addr}]` : addr;
}

async function answers(url: string): Promise<boolean> {
	try {
		const r = await fetch(url, { signal: AbortSignal.timeout(1000) });
		await r.arrayBuffer();
		return r.ok;
	} catch {
		return false;
	}
}

function unref(x: unknown): void {
	const u = (x as { unref?: () => void } | null)?.unref;
	if (typeof u === 'function') u.call(x);
}

/** A server this process started: `client()` is signed in; `close()` stops it. */
export class LocalServer {
	readonly url: string;
	readonly password: string;
	/** The child process (on Linux and macOS a small sh guard whose child is the server). */
	readonly process: ChildProcess;
	readonly #exited: Promise<void>;
	readonly #output: () => string;

	constructor(
		url: string,
		password: string,
		child: ChildProcess,
		exited: Promise<void>,
		output: () => string
	) {
		this.url = url;
		this.password = password;
		this.process = child;
		this.#exited = exited;
		this.#output = output;
	}

	/** A client for this server, already holding its password. */
	client(options: Omit<StarfoldOptions, 'password'> = {}): Starfold {
		return new Starfold(this.url, { ...options, password: this.password });
	}

	/** The last of what the server printed. */
	output(): string {
		return this.#output();
	}

	/** Stops the server and waits for it to exit. */
	async close(): Promise<void> {
		const child = this.process;
		if (child.exitCode !== null || child.signalCode !== null) {
			return;
		}
		if (process.platform === 'win32') {
			child.kill();
		} else {
			child.stdin?.end();
		}
		let timer: ReturnType<typeof setTimeout> | undefined;
		const late = new Promise<boolean>((r) => {
			timer = setTimeout(() => r(true), 5000);
		});
		const forced = await Promise.race([this.#exited.then(() => false), late]);
		clearTimeout(timer);
		if (forced) {
			try {
				if (process.platform === 'win32' || child.pid === undefined) {
					child.kill('SIGKILL');
				} else {
					process.kill(-child.pid, 'SIGKILL');
				}
			} catch {
				// already gone
			}
			await this.#exited;
		}
	}

	async [Symbol.asyncDispose](): Promise<void> {
		await this.close();
	}
}

/**
 * Starts the server and resolves once it answers. It binds to loopback with a generated
 * password unless told otherwise, and stops when this process exits. On Linux it needs an
 * X display ($DISPLAY, Xvfb for one); it does not start one.
 */
export async function startServer(
	options: StartServerOptions = {}
): Promise<LocalServer> {
	const binary = options.binary ?? binaryPath();
	checkLibraries(binary);
	const addr = options.addr ?? '127.0.0.1';
	const port = options.port ?? (await freePort(addr));
	const password = options.password ?? generatedPassword();
	const timeoutMs = options.timeoutMs ?? 30_000;
	const env: Record<string, string | undefined> = {
		...process.env,
		...options.env,
		STARFOLD_PASSWORD: password,
	};
	delete env.VOXL_PASSWORD;
	const args = [`--addr=${addr}`, `--port=${port}`, ...(options.args ?? [])];
	const windows = process.platform === 'win32';
	const child = windows
		? spawn(binary, args, {
				env,
				stdio: ['ignore', 'pipe', 'pipe'],
				windowsHide: true,
			})
		: spawn('/bin/sh', ['-c', GUARD, binary, ...args], {
				env,
				stdio: ['pipe', 'pipe', 'pipe'],
				detached: true,
			});

	let tail = '';
	const keep = (chunk: Buffer | string) => {
		tail = (tail + chunk.toString()).slice(-TAIL_BYTES);
	};
	child.stdout?.on('data', keep);
	child.stderr?.on('data', keep);
	let exitedWith: string | null = null;
	const exited = new Promise<void>((resolve) => {
		child.once('exit', (code, signal) => {
			exitedWith = signal ? `signal ${signal}` : `code ${code}`;
			resolve();
		});
		child.once('error', (e) => {
			exitedWith = `error ${e.message}`;
			resolve();
		});
	});
	if (windows) {
		const stop = () => child.kill();
		process.once('exit', stop);
		void exited.then(() => process.off('exit', stop));
	}

	const url = `http://${urlHost(addr)}:${port}`;
	const server = new LocalServer(url, password, child, exited, () => tail);
	const deadline = Date.now() + timeoutMs;
	// oxlint-disable-next-line no-await-in-loop
	while (!(await answers(`${url}/healthz`))) {
		if (exitedWith !== null) {
			throw new Error(
				`the starfold server exited (${exitedWith}) before it answered:\n${tail}`
			);
		}
		if (Date.now() > deadline) {
			// oxlint-disable-next-line no-await-in-loop
			await server.close();
			throw new Error(
				`the starfold server did not answer within ${timeoutMs} ms:\n${tail}`
			);
		}
		// oxlint-disable-next-line no-await-in-loop
		await new Promise((r) => setTimeout(r, 100));
	}
	if (exitedWith !== null) {
		throw new Error(
			`the starfold server exited (${exitedWith}) before it answered:\n${tail}`
		);
	}
	unref(child);
	unref(child.stdin);
	unref(child.stdout);
	unref(child.stderr);
	return server;
}
