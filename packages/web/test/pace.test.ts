import { describe, expect, test } from 'bun:test';
import { displayHz, FellBehind, MIN_QUEUED, StreamRate } from '../src/pace';

const stamps = (n: number, ms: number, jitter = 0) =>
	Array.from({ length: n }, (_, i) => i * ms + (i % 2 ? jitter : 0));

describe('displayHz', () => {
	test('reads 60, 120 and 144 Hz', () => {
		expect(displayHz(stamps(30, 1000 / 60))).toBe(60);
		expect(displayHz(stamps(60, 1000 / 120, 0.4))).toBe(120);
		expect(displayHz(stamps(72, 1000 / 144))).toBe(144);
	});
	test('a missed frame does not halve the rate', () => {
		const s = stamps(30, 1000 / 120);
		s.splice(10, 1);
		expect(displayHz(s)).toBe(120);
	});
	test('too few frames tell nothing', () => {
		expect(displayHz(stamps(5, 1000 / 60))).toBeUndefined();
	});
});

const at = (fps: number, n = 120) => {
	const r = new StreamRate();
	for (let i = 0; i < n; i++) r.note(1000 + (i * 1000) / fps);
	return r.maxQueued();
};

describe('StreamRate', () => {
	test('16 chunks up to 64 fps, as before', () => {
		expect(new StreamRate().maxQueued()).toBe(MIN_QUEUED);
		expect(at(30)).toBe(16);
		expect(at(60)).toBe(16);
	});
	test('250 ms of frames above that', () => {
		expect(at(120)).toBe(30);
		expect(at(144)).toBe(36);
	});
	test('a still is not a slow stream', () => {
		const r = new StreamRate();
		for (let i = 0; i < 120; i++) r.note((i * 1000) / 120);
		r.note(5000);
		expect(r.maxQueued()).toBe(30);
	});
});

describe('FellBehind', () => {
	test('the second time within 10 s, once', () => {
		const f = new FellBehind();
		expect(f.note(0)).toBe(false);
		expect(f.note(9_000)).toBe(true);
		expect(f.note(10_000)).toBe(false);
	});
	test('two falls far apart are not slow', () => {
		const f = new FellBehind();
		expect(f.note(0)).toBe(false);
		expect(f.note(20_000)).toBe(false);
		expect(f.note(40_000)).toBe(false);
	});
});
