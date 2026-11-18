// Embedded: the embedder's preamble, called before every connect. What it gives is copied into
// a plain array of strings; nothing here keeps, logs or reports it, and a failure is reported
// as a fixed value that carries nothing of what the preamble threw or gave.
import type { ConnectFailure, Preamble } from './types';

const failed = (cause: 'threw' | 'rejected' | 'not-text'): ConnectFailure => ({
	kind: 'preamble-failed',
	cause,
	message: {
		threw: 'the preamble threw',
		rejected: "the preamble's promise rejected",
		'not-text':
			'the preamble gave something other than a string or an array of strings',
	}[cause],
});

export const CONNECT_FAILED: ConnectFailure = {
	kind: 'connect-failed',
	message:
		'the WebSocket could not be created (check the URL and the subprotocols)',
};

export type PreambleResult =
	| { ok: true; msgs: string[] }
	| { ok: false; failure: ConnectFailure };

/** Calls `preamble` (none: nothing to send) and copies what it gives. */
export async function callPreamble(
	preamble: Preamble | undefined
): Promise<PreambleResult> {
	let given: unknown;
	try {
		given = preamble ? preamble() : [];
	} catch {
		return { ok: false, failure: failed('threw') };
	}
	try {
		given = await given;
	} catch {
		return { ok: false, failure: failed('rejected') };
	}
	try {
		return { ok: true, msgs: copyText(given) };
	} catch {
		return { ok: false, failure: failed('not-text') };
	}
}

/** A plain array of the strings in `v`, read by index (holes and proxies included); throws otherwise. */
function copyText(v: unknown): string[] {
	if (typeof v === 'string') return [v];
	if (!Array.isArray(v)) throw new TypeError('not text');
	const n = v.length;
	const out: string[] = [];
	for (let i = 0; i < n; i++) {
		const s: unknown = v[i];
		if (typeof s !== 'string') throw new TypeError('not text');
		out.push(s);
	}
	return out;
}
