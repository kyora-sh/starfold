import { spawn, spawnSync } from 'node:child_process';
import type { ChildProcess } from 'node:child_process';
import {
	accessSync,
	chmodSync,
	constants,
	readdirSync,
	statSync,
} from 'node:fs';
import { Agent, request } from 'node:http';
import { createRequire } from 'node:module';
import { createServer } from 'node:net';
import { dirname, join } from 'node:path';
import { Starfold } from './client.js';
import type { StarfoldOptions } from './client.js';
import { StarfoldError } from './types.js';

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

/** No server binary: $STARFOLD_BIN isn't set (or isn't a file) and no platform package has one. */
export class BinaryNotFoundError extends StarfoldError {}

/** The shared libraries the server binary needs don't all resolve on this Linux system. */
export class MissingLibrariesError extends StarfoldError {
	readonly missing: string[];

	constructor(missing: string[]) {
		super(
			`the starfold server can't start: these shared libraries are missing: ${missing.join(', ')}. ` +
				`It needs glibc 2.39+ (Ubuntu 24.04 or newer) and: ${LINUX_LIBRARIES_HINT}`
		);
		this.missing = missing;
	}
}

/** startServer failed: the server exited, didn't answer in time, or its port was taken. */
export class ServerStartError extends StarfoldError {
	/** The last of what the server printed. */
	readonly output: string;

	constructor(message: string, output: string) {
		super(`${message}:\n${output}`);
		this.output = output;
	}
}

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

function isFile(path: string): boolean {
	try {
		return statSync(path).isFile();
	} catch {
		return false;
	}
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
 * @starfold/server-* package. Throws a BinaryNotFoundError if neither is there; the client
 * still works against a remote server without it.
 */
export function binaryPath(): string {
	const env = process.env.STARFOLD_BIN;
	if (env) {
		if (!isFile(env)) {
			throw new BinaryNotFoundError(
				`STARFOLD_BIN is ${env}, which isn't a file`
			);
		}
		return executable(env);
	}
	const key = platformKey();
	const pkg = serverPackage(key);
	const remote =
		'The client still works against a remote server: new Starfold(url).';
	if (pkg === null) {
		throw new BinaryNotFoundError(
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
		throw new BinaryNotFoundError(
			`the server binary for ${key} comes in ${pkg}, which is not installed ` +
				`(were optional dependencies left out?); install it, or set STARFOLD_BIN. ${remote}`
		);
	}
	const bin = join(
		dir,
		'bin',
		process.platform === 'win32' ? 'starfold.exe' : 'starfold'
	);
	if (!isFile(bin)) {
		throw new BinaryNotFoundError(
			`${pkg} has no ${bin}; reinstall it, or set STARFOLD_BIN`
		);
	}
	return executable(bin);
}

/**
 * On Linux, what `binary` needs and the system lacks, by ldd: the shared libraries that don't
 * resolve and the glibc versions that are too new (GLIBC_2.39, say); elsewhere nothing.
 */
export function missingLibraries(binary: string): string[] {
	if (process.platform !== 'linux') {
		return [];
	}
	const r = spawnSync('ldd', ['--', binary], { encoding: 'utf8' });
	if (r.error) {
		return [];
	}
	const missing: string[] = [];
	for (const line of `${r.stdout ?? ''}\n${r.stderr ?? ''}`.split('\n')) {
		const lib = /^\s*(\S+)\s+=>\s+not found/.exec(line);
		const version = /version `([^']+)' not found/.exec(line);
		const name = lib?.[1] ?? version?.[1];
		if (name && !missing.includes(name)) {
			missing.push(name);
		}
	}
	return missing;
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
	/** The port (default: a free one, tried again if another process takes it first). */
	port?: number;
	/** The password (default, and when empty: a generated one, which `client()` holds). */
	password?: string;
	/** More server flags, such as --enable-recording or --base-path. */
	args?: string[];
	/** Environment for the server, on top of this process's (DISPLAY, for one). */
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
const PORT_TRIES = 3;

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

/** The --base-path among the server flags, read as the server reads it, without a trailing /. */
function basePath(args: string[]): string {
	let found = '';
	for (let i = 0; i < args.length; i++) {
		const a = args[i]!;
		const m = /^--base[-_]path(=(.*))?$/.exec(a);
		if (!m) continue;
		if (m[1] !== undefined) {
			found = m[2]!;
		} else if (i + 1 < args.length && !args[i + 1]!.startsWith('--')) {
			found = args[++i]!;
		}
	}
	return found.replace(/\/$/, '');
}

// Straight to the server: node:http with an agent of its own, since the global one takes the
// environment's proxy under NODE_USE_ENV_PROXY=1.
const direct = new Agent();

function answers(url: string): Promise<boolean> {
	return new Promise((resolve) => {
		const req = request(url, { timeout: 1000, agent: direct }, (res) => {
			res.resume();
			res.once('end', () => resolve(res.statusCode === 200));
			res.once('error', () => resolve(false));
		});
		req.once('timeout', () => req.destroy());
		req.once('error', () => resolve(false));
		req.end();
	});
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
			await this.#exited;
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

class PortTaken extends Error {}

/**
 * Starts the server and resolves once it answers. It binds to loopback with a generated
 * password unless told otherwise, and stops when this process exits. On Linux it needs an
 * X display ($DISPLAY, Xvfb for one); it does not start one. Every failure to start is a
 * ServerStartError carrying what the server printed.
 */
export async function startServer(
	options: StartServerOptions = {}
): Promise<LocalServer> {
	const binary = options.binary ?? binaryPath();
	checkLibraries(binary);
	const addr = options.addr ?? '127.0.0.1';
	const password = options.password || generatedPassword();
	const deadline = Date.now() + (options.timeoutMs ?? 30_000);
	for (let attempt = 1; ; attempt++) {
		// oxlint-disable-next-line no-await-in-loop
		const port = options.port ?? (await freePort(addr));
		try {
			// oxlint-disable-next-line no-await-in-loop
			return await launch(binary, addr, port, password, options, deadline);
		} catch (e) {
			if (!(e instanceof PortTaken)) throw e;
			if (options.port !== undefined || attempt >= PORT_TRIES) {
				throw new ServerStartError(`port ${port} is taken`, e.message);
			}
		}
	}
}

async function launch(
	binary: string,
	addr: string,
	port: number,
	password: string,
	options: StartServerOptions,
	deadline: number
): Promise<LocalServer> {
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
	// 'close', after the output pipes have drained, so the tail holds the server's last words.
	const exited = new Promise<void>((resolve) => {
		child.once('close', (code, signal) => {
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

	const url = `http://${urlHost(addr)}:${port}${basePath(options.args ?? [])}`;
	const server = new LocalServer(url, password, child, exited, () => tail);
	const failed = (): Error => {
		if (/address (already )?in use/i.test(tail)) return new PortTaken(tail);
		return new ServerStartError(
			`the starfold server exited (${exitedWith}) before it answered`,
			tail
		);
	};
	// oxlint-disable-next-line no-await-in-loop
	while (!(await answers(`${url}/healthz`))) {
		if (exitedWith !== null) {
			throw failed();
		}
		if (child.exitCode !== null || child.signalCode !== null) {
			// oxlint-disable-next-line no-await-in-loop
			await exited;
			throw failed();
		}
		if (Date.now() > deadline) {
			// oxlint-disable-next-line no-await-in-loop
			await server.close();
			throw new ServerStartError(
				`the starfold server did not answer within ${options.timeoutMs ?? 30_000} ms`,
				tail
			);
		}
		// oxlint-disable-next-line no-await-in-loop
		await new Promise((r) => setTimeout(r, 100));
	}
	if (exitedWith !== null) {
		throw failed();
	}
	unref(child);
	unref(child.stdin);
	unref(child.stdout);
	unref(child.stderr);
	return server;
}
