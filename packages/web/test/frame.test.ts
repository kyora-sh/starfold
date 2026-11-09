import { describe, expect, test } from 'bun:test';
import {
	frameBinary,
	frameText,
	Kind,
	MAX_MESSAGE,
	unframe,
} from '../src/frame';
import { cmd, msg } from '../src/protocol';

const buf = (b: Uint8Array) =>
	b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);

describe('framed messages', () => {
	test('text round-trips, the protocol inside unchanged', () => {
		for (const s of [
			cmd.key(65, true),
			msg.hello(3, [], 60),
			'{"t":"clip","d":"größe ✓"}',
		]) {
			const m = frameText(s);
			expect(m[0]).toBe(Kind.Text);
			expect(unframe(buf(m))).toEqual({ t: 'text', s });
		}
	});
	test('binary round-trips with its type byte', () => {
		const video = new Uint8Array([0x05, 1, 0, 0, 0, 7, 0xaa]);
		const m = frameBinary(video.buffer);
		expect([...m]).toEqual([Kind.Binary, ...video]);
		const u = unframe(buf(m));
		expect(u.t).toBe('binary');
		if (u.t === 'binary') expect([...new Uint8Array(u.b)]).toEqual([...video]);
		const view = new Uint8Array([9, 0x03, 4, 9]).subarray(1, 3);
		expect([...frameBinary(view)]).toEqual([Kind.Binary, 0x03, 4]);
	});
	test('empty payloads are messages, an empty message is not', () => {
		expect(unframe(buf(frameText('')))).toEqual({ t: 'text', s: '' });
		const u = unframe(buf(frameBinary(new Uint8Array())));
		expect(u.t === 'binary' && u.b.byteLength).toBe(0);
		expect(() => unframe(new ArrayBuffer(0))).toThrow(TypeError);
	});
	test('unknown kinds and bad UTF-8 are refused', () => {
		for (const k of [2, 0x11, 255])
			expect(() => unframe(new Uint8Array([k, 1]).buffer)).toThrow(/kind/);
		expect(() =>
			unframe(new Uint8Array([Kind.Text, 0xff, 0xfe]).buffer)
		).toThrow();
	});
	test('size is capped with the kind byte', () => {
		const max = new Uint8Array(MAX_MESSAGE - 1);
		expect(frameBinary(max).length).toBe(MAX_MESSAGE);
		expect(() => frameBinary(new Uint8Array(MAX_MESSAGE))).toThrow(RangeError);
		expect(() => unframe(new ArrayBuffer(MAX_MESSAGE + 1))).toThrow(RangeError);
	});
});
