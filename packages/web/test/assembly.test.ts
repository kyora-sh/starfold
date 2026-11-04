import { describe, expect, test } from 'bun:test';
import { FrameQueue, Percentiles } from '../src/assembly';
import { Bin, isCurrent, parseVideoHeader } from '../src/protocol';
import { yuvToRgb } from '../src/dav1d/yuv';

class Pic {
	closed = false;
	constructor(readonly name: string) {}
	close() {
		this.closed = true;
	}
}

function queue(max = 8) {
	const q = new FrameQueue<Pic>(max);
	const shown: [number, string[]][] = [];
	const flush = () =>
		q.flush((id, parts) => shown.push([id, parts.map((p) => p.pic.name)]));
	return { q, shown, flush };
}

const video = (type: number, frame: number, epoch?: number) => {
	const b = new Uint8Array(24);
	b[0] = type;
	b[1] = 1;
	new DataView(b.buffer).setUint32(2, frame);
	b[12] = 1;
	if (epoch !== undefined) new DataView(b.buffer).setUint32(16, epoch);
	return parseVideoHeader(b);
};

const apply = (matrix: number, full: boolean, yuv: number[]) => {
	const { m, off } = yuvToRgb(matrix, full);
	const v = yuv.map((x, i) => x / 255 - off[i]);
	return [0, 1, 2].map(
		(r) => (m[r * 3] * v[0] + m[r * 3 + 1] * v[1] + m[r * 3 + 2] * v[2]) * 255
	);
};

const encode = (kr: number, kb: number, full: boolean, rgb: number[]) => {
	const [r, g, b] = rgb.map((x) => x / 255);
	const y = kr * r + (1 - kr - kb) * g + kb * b;
	const cb = (b - y) / (2 * (1 - kb));
	const cr = (r - y) / (2 * (1 - kr));
	return full
		? [y * 255, cb * 255 + 128, cr * 255 + 128]
		: [16 + 219 * y, 128 + 224 * cb, 128 + 224 * cr];
};

describe('frame assembly', () => {
	test('a frame is presented once every stripe decoded, in arrival order', () => {
		const { q, shown, flush } = queue();
		q.open(1, 2, 0);
		q.open(2, 2, 0);
		q.add(2, { i: 0, y: 0, pic: new Pic('2a') });
		q.add(2, { i: 1, y: 540, pic: new Pic('2b') });
		flush();
		expect(shown).toEqual([]);
		q.add(1, { i: 1, y: 540, pic: new Pic('1b') });
		flush();
		expect(shown).toEqual([]);
		q.add(1, { i: 0, y: 0, pic: new Pic('1a') });
		flush();
		expect(shown).toEqual([
			[1, ['1b', '1a']],
			[2, ['2a', '2b']],
		]);
		expect(q.size).toBe(0);
	});

	test('a skipped stripe completes its frame with the others', () => {
		const { q, shown, flush } = queue();
		q.open(5, 2, 0);
		q.skip(5);
		q.add(5, { i: 1, y: 540, pic: new Pic('5b') });
		flush();
		expect(shown).toEqual([[5, ['5b']]]);
	});

	test('a frame with every stripe skipped is still presented, to be acked', () => {
		const { q, shown, flush } = queue();
		q.open(7, 1, 0);
		q.skip(7);
		flush();
		expect(shown).toEqual([[7, []]]);
	});

	test('the queue is bounded', () => {
		const { q } = queue(2);
		expect(q.open(1, 1, 0)).toBe(true);
		expect(q.open(2, 1, 0)).toBe(true);
		expect(q.open(2, 1, 0)).toBe(true);
		expect(q.open(3, 1, 0)).toBe(false);
	});

	test('a cut drops the stripes left of every frame begun before it', () => {
		const { q, shown, flush } = queue();
		expect(q.seen(10)).toBe(true);
		q.open(10, 2, 0);
		const p = new Pic('10a');
		q.add(10, { i: 0, y: 0, pic: p });
		expect(q.cut()).toBe(10);
		expect(p.closed).toBe(true);
		expect(q.seen(10)).toBe(false);
		expect(q.seen(9)).toBe(false);
		expect(q.seen(11)).toBe(true);
		q.open(11, 1, 0);
		q.add(11, { i: 0, y: 0, pic: new Pic('11') });
		flush();
		expect(shown).toEqual([[11, ['11']]]);
	});

	test('the cut survives frame ids wrapping, and a new stream forgets it', () => {
		const { q } = queue();
		q.seen(0xffffffff);
		expect(q.cut()).toBe(0xffffffff);
		expect(q.seen(0xffffffff)).toBe(false);
		expect(q.seen(0)).toBe(true);
		q.reset();
		expect(q.seen(3)).toBe(true);
		expect(q.cut()).toBe(3);
	});

	test('a cut before any frame has nothing to report', () => {
		expect(queue().q.cut()).toBeNull();
	});

	test('pictures of a gone frame stay with the caller; clear closes the rest', () => {
		const { q } = queue();
		expect(q.add(9, { i: 0, y: 0, pic: new Pic('x') })).toBe(false);
		q.open(1, 2, 0);
		const p = new Pic('1a');
		q.add(1, { i: 0, y: 0, pic: p });
		q.clear();
		expect(p.closed).toBe(true);
		expect(q.size).toBe(0);
		expect(q.has(1)).toBe(false);
	});
});

describe('epochs', () => {
	test('protocol 2 takes video of the current epoch once its decoders are ready', () => {
		expect(isCurrent(video(Bin.VideoV2, 1, 7), 2, 7, true)).toBe(true);
		expect(isCurrent(video(Bin.VideoV2, 1, 7), 2, 7, false)).toBe(false);
		expect(isCurrent(video(Bin.VideoV2, 1, 6), 2, 7, true)).toBe(false);
		expect(isCurrent(video(Bin.VideoV2, 1, 8), 2, 7, true)).toBe(false);
		expect(isCurrent(video(Bin.Video, 1), 2, 7, true)).toBe(false);
	});

	test('protocol 1 takes only protocol 1 video', () => {
		expect(isCurrent(video(Bin.Video, 1), 1, 0, false)).toBe(true);
		expect(isCurrent(video(Bin.VideoV2, 1, 1), 1, 0, true)).toBe(false);
	});

	test('the header carries the epoch only in protocol 2', () => {
		expect(video(Bin.VideoV2, 0xfffffffe, 0x01020304)).toMatchObject({
			key: true,
			frame: 0xfffffffe,
			parts: 1,
			epoch: 0x01020304,
		});
		expect(video(Bin.Video, 3).epoch).toBeUndefined();
	});
});

test('decode time percentiles', () => {
	const p = new Percentiles();
	for (let i = 1; i <= 100; i++) p.add(i);
	expect(p.take()).toEqual({ p50: 50, p95: 95 });
	expect(p.take()).toEqual({ p50: 0, p95: 0 });
});

describe('Y′CbCr to R′G′B′', () => {
	const rgb = [48, 80, 160];
	for (const [name, matrix, kr, kb] of [
		['BT.709', 1, 0.2126, 0.0722],
		['BT.601', 6, 0.299, 0.114],
		['unspecified as BT.709', 2, 0.2126, 0.0722],
	] as const)
		for (const full of [false, true])
			test(`${name}, ${full ? 'full' : 'limited'} range`, () => {
				const out = apply(matrix, full, encode(kr, kb, full, rgb));
				out.forEach((c, i) => expect(c).toBeCloseTo(rgb[i], 6));
			});
	test('limited-range black and white', () => {
		apply(1, false, [16, 128, 128]).forEach((c) => expect(c).toBeCloseTo(0, 6));
		apply(1, false, [235, 128, 128]).forEach((c) =>
			expect(c).toBeCloseTo(255, 6)
		);
	});
});
