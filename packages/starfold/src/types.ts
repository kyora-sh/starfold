/** A request the server refused or could not answer. */
export class StarfoldError extends Error {
	readonly status: number | null;

	constructor(message: string, status: number | null = null) {
		super(message);
		this.name = new.target.name;
		this.status = status;
	}
}

/** Missing or wrong password (401). */
export class AuthError extends StarfoldError {}

/** The server refuses this (403), e.g. file transfer is turned off. */
export class ForbiddenError extends StarfoldError {}

/** The server runs with --read-only and takes no input. */
export class ReadOnlyError extends ForbiddenError {}

/** The server runs without --enable-recording, so it doesn't record. */
export class RecordingOffError extends ForbiddenError {}

/** No such file, folder or recording (404). */
export class NotFoundError extends StarfoldError {}

/** The server did not answer in time. */
export class StarfoldTimeoutError extends StarfoldError {}

/** The server could not be reached. */
export class StarfoldConnectionError extends StarfoldError {}

export class Screenshot {
	readonly png: Uint8Array;
	/** Pass as `since` to `wait({ change: true })` for changes after this screenshot. */
	readonly seq: number;

	constructor(png: Uint8Array, seq: number) {
		this.png = png;
		this.seq = seq;
	}

	/** [width, height], from the PNG header. */
	get size(): [number, number] {
		const v = new DataView(
			this.png.buffer,
			this.png.byteOffset,
			this.png.byteLength
		);
		return [v.getUint32(16), v.getUint32(20)];
	}

	/** Writes the PNG to `path` and returns the path. */
	async save(path: string): Promise<string> {
		const { writeFile } = await import('node:fs/promises');
		await writeFile(path, this.png);
		return path;
	}
}

export interface WaitResult {
	changed: boolean;
	timedOut: boolean;
	seq: number;
	waitedMs: number;
}

export interface InputResult {
	/** The screen's change count before the input: `wait({ change: true, since: seq })` returns once it changed. */
	seq: number;
	events: number;
}

/**
 * A started recording. `id` is the string the server returned once, to this caller; any
 * holder of it can stop the recording and obtain its clip, so treat it like a secret.
 */
export interface Recording {
	id: string;
	maxSeconds: number;
}

/**
 * A recording's MP4s, in order: one, unless another encoder took over mid-recording.
 * `endReason` says why it ended: stopped, time_limit, resized, segment_cap,
 * bad_parameter_sets, segment_lost or closed (null from a server that sends one MP4).
 */
export type Segments = Uint8Array[] & { endReason: string | null };

export function segments(
	parts: Uint8Array[],
	endReason: string | null = null
): Segments {
	return Object.assign(parts, { endReason });
}

export interface FileEntry {
	name: string;
	dir: boolean;
	size: number;
	mtimeMs: number;
}

export interface FileList {
	path: string;
	entries: FileEntry[];
}

export interface Upload {
	path: string;
	size: number;
}
