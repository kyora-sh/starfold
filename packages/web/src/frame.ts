// Framed messages (?framed=1, the server's --framed) for hops that carry binary WebSocket
// messages only: every message is binary, its first byte the kind of what follows, 0 for
// binary and 1 for UTF-8 text. The protocol inside is unchanged (server: src/frame.rs).

export const Kind = { Binary: 0, Text: 1 } as const;

/** The largest message either side takes, kind byte included. */
export const MAX_MESSAGE = 64 << 20;

export type Unframed =
	| { t: 'text'; s: string }
	| { t: 'binary'; b: ArrayBuffer };

const encoder = new TextEncoder();
const decoder = new TextDecoder('utf-8', { fatal: true });

function frame(kind: number, payload: Uint8Array): Uint8Array<ArrayBuffer> {
	if (payload.length + 1 > MAX_MESSAGE)
		throw new RangeError(`a framed message of ${payload.length + 1} bytes`);
	const m = new Uint8Array(payload.length + 1);
	m[0] = kind;
	m.set(payload, 1);
	return m;
}

export const frameText = (s: string) => frame(Kind.Text, encoder.encode(s));

export const frameBinary = (b: ArrayBuffer | Uint8Array) =>
	frame(Kind.Binary, b instanceof Uint8Array ? b : new Uint8Array(b));

/** A framed message as plain WebSockets would have carried it; throws on anything else. */
export function unframe(m: ArrayBuffer): Unframed {
	if (m.byteLength > MAX_MESSAGE)
		throw new RangeError(`a framed message of ${m.byteLength} bytes`);
	const b = new Uint8Array(m);
	if (b.length === 0) throw new TypeError('an empty framed message');
	switch (b[0]) {
		case Kind.Binary:
			return { t: 'binary', b: m.slice(1) };
		case Kind.Text:
			return { t: 'text', s: decoder.decode(b.subarray(1)) };
	}
	throw new TypeError(`unknown framed message kind ${b[0]}`);
}
