import * as act from './act.js';
import type { Action, Button } from './act.js';
import {
	AuthError,
	ForbiddenError,
	NotFoundError,
	ReadOnlyError,
	RecordingOffError,
	Screenshot,
	StarfoldConnectionError,
	StarfoldError,
	StarfoldTimeoutError,
	segments,
} from './types.js';
import type {
	FileList,
	InputResult,
	Recording,
	Segments,
	Upload,
	WaitResult,
} from './types.js';
import { unzip } from './zip.js';

(Symbol as { asyncDispose?: symbol }).asyncDispose ??= Symbol.for(
	'Symbol.asyncDispose'
);

export const DEFAULT_URL = 'http://localhost:6901';
export const DEFAULT_TIMEOUT_MS = 30_000;

export interface StarfoldOptions {
	/** Defaults to $STARFOLD_PASSWORD. */
	password?: string;
	/** Per request; waits and recordings get their own length on top. */
	timeoutMs?: number;
	/** The fetch to use (default: the global one). */
	fetch?: typeof fetch;
}

export interface WaitOptions {
	/** How long the screen has to stay still (the server's default: 300). */
	idleMs?: number;
	timeoutMs?: number;
	/** Wait until the screen changes instead. */
	change?: boolean;
	/** [x, y, width, height] to watch instead of the whole screen. */
	region?: [number, number, number, number];
	/** A `seq` from a screenshot or an input: a change after it counts. */
	since?: number;
}

type Params = Record<string, string | number>;

interface Req<T> {
	method: string;
	path: string;
	parse: (r: Response) => Promise<T>;
	params?: Params;
	json?: unknown;
	body?: Uint8Array | Blob;
	headers?: Record<string, string>;
	/** Milliseconds the server may take on purpose (a wait, a recording), on top of the timeout. */
	extraMs?: number;
}

type Env = Record<string, string | undefined>;

function environment(): Env {
	const p = (globalThis as { process?: { env?: Env } }).process;
	try {
		return p?.env ?? {};
	} catch {
		return {};
	}
}

function warn(message: string): void {
	const p = (
		globalThis as { process?: { emitWarning?: (m: string, t: string) => void } }
	).process;
	if (typeof p?.emitWarning === 'function') {
		p.emitWarning(message, 'DeprecationWarning');
	} else {
		console.warn(`DeprecationWarning: ${message}`);
	}
}

function envPassword(): string | undefined {
	const env = environment();
	const fresh = env.STARFOLD_PASSWORD;
	const old = env.VOXL_PASSWORD;
	if (old !== undefined) {
		warn(
			fresh !== undefined
				? 'VOXL_PASSWORD is ignored: STARFOLD_PASSWORD is set'
				: 'VOXL_PASSWORD is deprecated: use STARFOLD_PASSWORD'
		);
	}
	return fresh ?? old;
}

async function errorFor(resp: Response): Promise<StarfoldError> {
	const text = await resp.text();
	let body: unknown = null;
	let message: string;
	try {
		body = JSON.parse(text);
		const e = (body as { error?: unknown }).error;
		if (e === undefined) {
			throw new Error('no error field');
		}
		message = String(e);
	} catch {
		body = null;
		message = text.trim() || resp.statusText;
	}
	const code = resp.status;
	const fields =
		body !== null && typeof body === 'object'
			? (body as Record<string, unknown>)
			: {};
	if (code === 401) {
		return new AuthError(message, code);
	}
	if (code === 403) {
		if (fields.read_only === true) {
			return new ReadOnlyError(message, code);
		}
		if (fields.recording === false) {
			return new RecordingOffError(message, code);
		}
		return new ForbiddenError(message, code);
	}
	if (code === 404) {
		return new NotFoundError(message, code);
	}
	return new StarfoldError(`${code}: ${message}`, code);
}

async function bytes(r: Response): Promise<Uint8Array> {
	return new Uint8Array(await r.arrayBuffer());
}

async function json(r: Response): Promise<Record<string, unknown>> {
	return (await r.json()) as Record<string, unknown>;
}

function seqHeader(headers: Headers): number {
	const v = headers.get('x-starfold-seq') ?? headers.get('x-voxl-seq');
	if (v === null) {
		throw new StarfoldError('the screenshot came without its sequence number');
	}
	return Number.parseInt(v, 10);
}

async function parseSegments(r: Response): Promise<Segments> {
	const type = (r.headers.get('content-type') ?? '')
		.split(';')[0]!
		.trim()
		.toLowerCase();
	const data = await bytes(r);
	if (type !== 'application/zip') {
		return segments([data]);
	}
	const members = await unzip(data);
	const member = (name: string): Uint8Array => {
		const m = members.get(name);
		if (m === undefined) {
			throw new StarfoldError(`the recording's zip lacks ${name}`);
		}
		return m;
	};
	const manifest = JSON.parse(
		new TextDecoder().decode(member('manifest.json'))
	) as {
		end_reason: string;
		segments: { file: string }[];
	};
	return segments(
		manifest.segments.map((s) => member(s.file)),
		manifest.end_reason
	);
}

function flatten(actions: Iterable<Action | Action[]>): Action[] {
	const flat: Action[] = [];
	for (const a of actions) {
		if (Array.isArray(a)) {
			flat.push(...a);
		} else {
			flat.push(a);
		}
	}
	return flat;
}

function recordingId(recording: Recording | string): string {
	const id =
		typeof recording === 'object' && recording !== null
			? (recording as { id?: unknown }).id
			: (recording as unknown);
	if (typeof id !== 'string') {
		const kind = id === null ? 'null' : typeof id;
		throw new TypeError(
			`a recording id is a string (the Recording recordStart returned, or its .id), not ${kind}`
		);
	}
	return id;
}

async function readData(
	data: Uint8Array | ArrayBuffer | Blob | string | URL
): Promise<Uint8Array | Blob> {
	if (typeof data === 'string' || data instanceof URL) {
		const { readFile } = await import('node:fs/promises');
		return new Uint8Array(await readFile(data));
	}
	if (data instanceof ArrayBuffer) {
		return new Uint8Array(data);
	}
	return data;
}

/**
 * A starfold desktop, for agents: screenshots, waiting for the screen, mouse and keyboard.
 *
 * `password` defaults to $STARFOLD_PASSWORD. `timeoutMs` applies per request; waits and
 * recordings get their own length on top.
 */
export class Starfold {
	readonly url: string;
	readonly timeoutMs: number;
	readonly #headers: Record<string, string>;
	readonly #fetch: typeof fetch;

	constructor(url: string = DEFAULT_URL, options: StarfoldOptions = {}) {
		this.url = url.replace(/\/+$/, '');
		const password =
			options.password !== undefined ? options.password : envPassword();
		this.#headers = password ? { Authorization: `Bearer ${password}` } : {};
		this.timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
		this.#fetch = options.fetch ?? globalThis.fetch.bind(globalThis);
	}

	/** Nothing to release (each request is its own); here so the client can be closed like the Python one. */
	async close(): Promise<void> {}

	async [Symbol.asyncDispose](): Promise<void> {
		await this.close();
	}

	async #do<T>(req: Req<T>): Promise<T> {
		const query = req.params
			? `?${new URLSearchParams(Object.entries(req.params).map(([k, v]) => [k, String(v)]))}`
			: '';
		const headers: Record<string, string> = {
			...this.#headers,
			...req.headers,
		};
		let body: Uint8Array | Blob | string | undefined = req.body;
		if (req.json !== undefined) {
			body = JSON.stringify(req.json);
			headers['Content-Type'] = 'application/json';
		}
		const ms = this.timeoutMs + (req.extraMs ?? 0);
		const abort = new AbortController();
		const timer = setTimeout(() => abort.abort(), ms);
		const late = () => new StarfoldTimeoutError(`no answer in time: ${ms} ms`);
		try {
			let resp: Response;
			try {
				resp = await this.#fetch(`${this.url}${req.path}${query}`, {
					method: req.method,
					headers,
					body: body as BodyInit | undefined,
					signal: abort.signal,
				});
			} catch (e) {
				throw abort.signal.aborted
					? late()
					: new StarfoldConnectionError(
							`cannot reach the server: ${describe(e)}`
						);
			}
			try {
				if (!resp.ok) {
					throw await errorFor(resp);
				}
				return await req.parse(resp);
			} catch (e) {
				throw abort.signal.aborted && !(e instanceof StarfoldError)
					? late()
					: e;
			}
		} finally {
			clearTimeout(timer);
		}
	}

	screenshot(): Promise<Screenshot> {
		return this.#do({
			method: 'GET',
			path: '/screenshot',
			parse: async (r) => new Screenshot(await bytes(r), seqHeader(r.headers)),
		});
	}

	/** Until the screen stays still for `idleMs` (default 300), or with `change` until it changes. */
	wait(options: WaitOptions = {}): Promise<WaitResult> {
		const params: Params = {};
		if (options.idleMs !== undefined) params.idle = options.idleMs;
		if (options.timeoutMs !== undefined) params.timeout = options.timeoutMs;
		if (options.change) params.change = 1;
		if (options.region !== undefined)
			params.region = options.region.map((v) => Math.trunc(v)).join(',');
		if (options.since !== undefined) params.since = options.since;
		return this.#do({
			method: 'GET',
			path: '/wait',
			params,
			extraMs: options.timeoutMs ?? 10_000,
			parse: async (r) => {
				const d = await json(r);
				return {
					changed: Boolean(d.changed),
					timedOut: Boolean(d.timed_out),
					seq: Number(d.seq),
					waitedMs: Number(d.waited_ms),
				};
			},
		});
	}

	/** Sends `act` actions in one request, applied in order. */
	actions(actions: Iterable<Action | Action[]>): Promise<InputResult> {
		const flat = flatten(actions);
		let extraMs = 0;
		for (const a of flat) {
			if (a.type === 'pause') extraMs += a.ms;
			// The answer comes once the input is done; a character the keymap lacks takes ~10 ms.
			if (a.type === 'type') extraMs += a.text.length * 10;
		}
		return this.#do({
			method: 'POST',
			path: '/input',
			json: flat,
			extraMs,
			parse: async (r) => {
				const d = await json(r);
				return { seq: Number(d.seq), events: Number(d.events) };
			},
		});
	}

	move(x: number, y: number): Promise<InputResult> {
		return this.actions([act.move(x, y)]);
	}

	click(
		x?: number,
		y?: number,
		button: Button = 'left',
		count = 1
	): Promise<InputResult> {
		return this.actions([act.click(x, y, button, count)]);
	}

	doubleClick(
		x?: number,
		y?: number,
		button: Button = 'left'
	): Promise<InputResult> {
		return this.actions([act.click(x, y, button, 2)]);
	}

	mouseDown(
		x?: number,
		y?: number,
		button: Button = 'left'
	): Promise<InputResult> {
		return this.actions([act.down(x, y, button)]);
	}

	mouseUp(
		x?: number,
		y?: number,
		button: Button = 'left'
	): Promise<InputResult> {
		return this.actions([act.up(x, y, button)]);
	}

	drag(
		x1: number,
		y1: number,
		x2: number,
		y2: number,
		button: Button = 'left',
		steps = 10,
		stepMs = 10
	): Promise<InputResult> {
		return this.actions(act.drag(x1, y1, x2, y2, button, steps, stepMs));
	}

	scroll(dx = 0, dy = 0, x?: number, y?: number): Promise<InputResult> {
		return this.actions([act.scroll(dx, dy, x, y)]);
	}

	key(combo: string): Promise<InputResult> {
		return this.actions([act.key(combo)]);
	}

	typeText(text: string): Promise<InputResult> {
		return this.actions([act.typeText(text)]);
	}

	/** An MP4 of the next `seconds`. */
	record(seconds: number): Promise<Segments> {
		return this.#do({
			method: 'GET',
			path: '/record',
			params: { seconds, format: 'zip' },
			extraMs: seconds * 1000,
			parse: parseSegments,
		});
	}

	recordStart(): Promise<Recording> {
		return this.#do({
			method: 'POST',
			path: '/record/start',
			parse: async (r) => {
				const d = await json(r);
				return { id: String(d.id), maxSeconds: Number(d.max_seconds) };
			},
		});
	}

	async recordStop(recording: Recording | string): Promise<Segments> {
		const id = recordingId(recording);
		return this.#do({
			method: 'POST',
			path: '/record/stop',
			params: { id, format: 'zip' },
			parse: parseSegments,
		});
	}

	filesList(path = ''): Promise<FileList> {
		return this.#do({
			method: 'GET',
			path: '/files/list',
			params: { path },
			parse: async (r) => {
				const d = (await r.json()) as {
					path: string;
					entries: {
						name: string;
						dir: unknown;
						size: unknown;
						mtime: unknown;
					}[];
				};
				return {
					path: d.path,
					entries: d.entries.map((e) => ({
						name: e.name,
						dir: Boolean(e.dir),
						size: Number(e.size),
						mtimeMs: Number(e.mtime),
					})),
				};
			},
		});
	}

	/** A file's bytes, or a folder as a zip. */
	filesDownload(path: string): Promise<Uint8Array> {
		return this.#do({
			method: 'GET',
			path: '/files/download',
			params: { path },
			parse: bytes,
		});
	}

	/** Uploads `data` (bytes, a Blob, or a local file's path) as `name` into `path` (default: the upload folder). */
	async filesUpload(
		name: string,
		data: Uint8Array | ArrayBuffer | Blob | string | URL,
		path?: string,
		sub?: string
	): Promise<Upload> {
		const params: Params = { name };
		if (path !== undefined) params.path = path;
		if (sub !== undefined) params.sub = sub;
		return this.#do({
			method: 'PUT',
			path: '/files/upload',
			params,
			body: await readData(data),
			headers: { 'X-Starfold-Upload': '1', 'X-Voxl-Upload': '1' },
			parse: async (r) => {
				const d = await json(r);
				return { path: String(d.path), size: Number(d.size) };
			},
		});
	}
}

function describe(e: unknown): string {
	if (e instanceof Error) {
		const cause = (e as { cause?: unknown }).cause;
		return cause instanceof Error
			? `${e.message} (${cause.message})`
			: e.message;
	}
	return String(e);
}
