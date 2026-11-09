// The worker itself (src/worker.ts), loaded with a WebSocket that behaves like a browser's
// where it matters here: what goes out on each send path, and which close codes it takes.
import { beforeAll, describe, expect, test } from 'bun:test';
import { frameBinary, frameText, Kind } from '../src/frame';

type Sent = string | number[];

class FakeSocket {
	static OPEN = 1;
	static all: FakeSocket[] = [];
	readyState = 0;
	binaryType = 'blob';
	sent: Sent[] = [];
	closes: (number | undefined)[] = [];
	private on = new Map<string, ((e: unknown) => void)[]>();
	constructor(public url: string) {
		FakeSocket.all.push(this);
	}
	addEventListener(t: string, f: (e: unknown) => void) {
		this.on.set(t, [...(this.on.get(t) ?? []), f]);
	}
	emit(t: string, e: unknown = {}) {
		for (const f of this.on.get(t) ?? []) f(e);
	}
	open() {
		this.readyState = 1;
		this.emit('open');
	}
	receive(data: string | ArrayBuffer) {
		this.emit('message', { data });
	}
	send(d: string | ArrayBuffer | ArrayBufferView) {
		if (typeof d === 'string') this.sent.push(d);
		else if (d instanceof ArrayBuffer) this.sent.push([...new Uint8Array(d)]);
		else
			this.sent.push([...new Uint8Array(d.buffer, d.byteOffset, d.byteLength)]);
	}
	close(code?: number) {
		// As in browsers: any other code throws InvalidAccessError.
		if (code !== undefined && code !== 1000 && (code < 3000 || code > 4999))
			throw new DOMException(`close code ${code}`, 'InvalidAccessError');
		this.closes.push(code);
		this.readyState = 3;
		this.emit('close', { code });
	}
}

const posted: { t: string; [k: string]: unknown }[] = [];
let toWorker: (e: { data: unknown }) => void = () => {};
const enc = new TextEncoder();
const bytes = (s: string) => [...enc.encode(s)];
const framedText = (s: string) => [Kind.Text, ...bytes(s)];
const buf = (u: Uint8Array) =>
	u.buffer.slice(u.byteOffset, u.byteOffset + u.byteLength);
const hello = JSON.stringify({ t: 'hello', w: 64, h: 32, audio: false });
const canvas = { width: 0, height: 0, getContext: () => ({}) };

function connect(framed: boolean): FakeSocket {
	const n = FakeSocket.all.length;
	toWorker({
		data: { t: 'init', canvas, url: 'ws://x/ws', presentToMain: false, framed },
	});
	expect(FakeSocket.all.length).toBe(n + 1);
	const s = FakeSocket.all[n];
	s.open();
	return s;
}

beforeAll(async () => {
	Object.assign(globalThis, {
		WebSocket: FakeSocket,
		self: {
			location: { search: '' },
			postMessage: (m: { t: string }) => posted.push(m),
			addEventListener: (t: string, f: typeof toWorker) => {
				if (t === 'message') toWorker = f;
			},
		},
	});
	await import('../src/worker');
});

describe('the worker, framed', () => {
	test('every send path frames, every receive path unframes', () => {
		const s = connect(true);
		s.receive(buf(frameText(hello)));
		expect(posted.some((m) => m.t === 'hello' && m.w === 64)).toBe(true);
		s.receive(buf(frameText('{"t":"ping","s":5}')));
		toWorker({ data: { t: 'send', d: 'k,65,1' } });
		toWorker({ data: { t: 'sendBin', d: new Uint8Array([0x11, 9]).buffer } });
		s.receive(buf(frameBinary(new Uint8Array([0x03, 7, 8]))));
		expect(s.sent).toEqual([
			framedText('au,0'),
			framedText('v,1'),
			framedText('po,5'),
			framedText('k,65,1'),
			[Kind.Binary, 0x11, 9],
		]);
		const cursor = posted.find((m) => m.t === 'cursor');
		expect([...new Uint8Array(cursor?.buf as ArrayBuffer)]).toEqual([
			0x03, 7, 8,
		]);
	});

	test('anything not framed closes the socket with a code browsers take', () => {
		for (const bad of [
			'{"t":"ping","s":1}',
			new Uint8Array([9, 1]).buffer,
			new Uint8Array([Kind.Text, 0xff]).buffer,
			new ArrayBuffer(0),
		]) {
			const s = connect(true);
			s.receive(bad);
			expect(s.closes).toEqual([4003]);
			expect(posted.at(-1)).toEqual({ t: 'status', s: 'closed' });
		}
	});
});

describe('the worker, unframed', () => {
	test('text goes out as text and binary as it is', () => {
		const s = connect(false);
		s.receive(hello);
		s.receive('{"t":"ping","s":5}');
		toWorker({ data: { t: 'sendBin', d: new Uint8Array([0x11, 9]).buffer } });
		expect(s.sent).toEqual(['au,0', 'v,1', 'po,5', [0x11, 9]]);
	});
});
