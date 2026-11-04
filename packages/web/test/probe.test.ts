import { describe, expect, test } from 'bun:test';
import { dav1dDecodes, probe } from '../src/probe';
import type { ProbeEnv } from '../src/probe';
import type { VideoOffer } from '../src/protocol';

const offer = (
	codec: VideoOffer['codec'],
	config: string,
	w = 1920,
	h = 1080
): VideoOffer => ({
	codec,
	format: codec === 'av1' ? 'av1-low-overhead' : 'annexb',
	stripes: [
		{ i: 0, y: 0, h, config: { codec: config, codedWidth: w, codedHeight: h } },
	],
});
const AV1 = offer('av1', 'av01.0.08M.08');
const HEVC = offer('hevc', 'hvc1.1.6.L120.90');
const H264 = offer('h264', 'avc1.42c028');

/** A browser decoding `hw` with hardware and `sw` only without a preference. */
function env(hw: string[], sw: string[], dav1d = true) {
	const asked: string[] = [];
	let loads = 0;
	const e: ProbeEnv = {
		supported: async (c) => {
			asked.push(`${c.codec}:${c.hardwareAcceleration}`);
			const prefix = c.codec.slice(0, 4);
			return c.hardwareAcceleration === 'prefer-hardware'
				? hw.includes(prefix)
				: hw.includes(prefix) || sw.includes(prefix);
		},
		dav1d: async () => {
			loads++;
			return dav1d;
		},
		dav1dDecodes,
	};
	return { e, asked, loads: () => loads };
}

describe('probe', () => {
	test('prefer-hardware first, then no-preference; the successful one is recorded', async () => {
		const { e, asked } = env(['hvc1'], ['avc1']);
		const r = await probe([HEVC, H264], e);
		expect(
			r.decode.map((d) => [d.codec, d.implementation, d.hardwareAcceleration])
		).toEqual([
			['hvc1.1.6.L120.90', 'webcodecs', 'prefer-hardware'],
			['avc1.42c028', 'webcodecs', 'no-preference'],
		]);
		expect(asked).toEqual([
			'hvc1.1.6.L120.90:prefer-hardware',
			'avc1.42c028:prefer-hardware',
			'avc1.42c028:no-preference',
		]);
		expect(r.accepted.get('avc1.42c028/1920x1080')).toEqual({
			implementation: 'webcodecs',
			hardwareAcceleration: 'no-preference',
		});
	});

	test('dav1d-wasm is offered for AV1 only when nothing decodes through WebCodecs', async () => {
		const { e, loads } = env([], []);
		const r = await probe([AV1, H264], e);
		expect(r.decode.map((d) => [d.codec, d.implementation])).toEqual([
			['av01.0.08M.08', 'dav1d-wasm'],
		]);
		expect(r.decode[0].hardwareAcceleration).toBeUndefined();
		expect(r.accepted.get('av01.0.08M.08/1920x1080')?.implementation).toBe(
			'dav1d-wasm'
		);
		expect(loads()).toBe(1);
	});

	test('software WebCodecs H.264 outranks dav1d-wasm, which is then not even fetched', async () => {
		const { e, loads } = env([], ['avc1']);
		const r = await probe([AV1, H264], e);
		expect(r.decode.map((d) => d.implementation)).toEqual(['webcodecs']);
		expect(loads()).toBe(0);
	});

	test('an offer WebCodecs covers only in part does not rule dav1d-wasm out', async () => {
		const two: VideoOffer = {
			...H264,
			stripes: [
				H264.stripes[0],
				{
					i: 1,
					y: 1080,
					h: 1080,
					config: { codec: 'avc1.640034', codedWidth: 1920, codedHeight: 1080 },
				},
			],
		};
		const e: ProbeEnv = {
			supported: async (c) =>
				c.hardwareAcceleration === 'no-preference' && c.codec === 'avc1.42c028',
			dav1d: async () => true,
			dav1dDecodes,
		};
		const r = await probe([AV1, two], e);
		expect(r.decode.map((d) => [d.codec, d.implementation])).toEqual([
			['avc1.42c028', 'webcodecs'],
			['av01.0.08M.08', 'dav1d-wasm'],
		]);
	});

	test('WebCodecs AV1 at one size rules dav1d-wasm out at another', async () => {
		const small = offer('av1', 'av01.0.04M.08', 640, 360);
		const e: ProbeEnv = {
			supported: async (c) =>
				c.hardwareAcceleration === 'no-preference' && c.codedWidth === 640,
			dav1d: async () => true,
			dav1dDecodes,
		};
		const r = await probe([AV1, small], e);
		expect(r.decode.map((d) => [d.codec, d.implementation])).toEqual([
			['av01.0.04M.08', 'webcodecs'],
		]);
	});

	test('any hardware WebCodecs decoder rules dav1d-wasm out, and it is not even loaded', async () => {
		const runs = [['avc1'], ['hvc1']].map(async (hw) => {
			const { e, loads } = env(hw, ['avc1']);
			const r = await probe([AV1, HEVC, H264], e);
			return [r.decode.some((d) => d.implementation === 'dav1d-wasm'), loads()];
		});
		expect(await Promise.all(runs)).toEqual([
			[false, 0],
			[false, 0],
		]);
	});

	test('hardware for a codec the server did not offer rules dav1d-wasm out too', async () => {
		const { e, asked, loads } = env(['hvc1'], []);
		const r = await probe([AV1, H264], e);
		expect(asked).toContain('hvc1.1.6.L120.90:prefer-hardware');
		expect(r.decode).toEqual([]);
		expect(loads()).toBe(0);
	});

	test("the browser's own AV1 decoder, even without hardware, beats dav1d-wasm", async () => {
		const { e, loads } = env([], ['av01', 'avc1']);
		const r = await probe([AV1, H264], e);
		expect(r.decode.map((d) => d.implementation)).toEqual([
			'webcodecs',
			'webcodecs',
		]);
		expect(loads()).toBe(0);
	});

	test('dav1d-wasm is advertised only once it initialized', async () => {
		const { e, loads } = env([], [], false);
		const r = await probe([AV1, H264], e);
		expect(r.decode).toEqual([]);
		expect(r.accepted.has('av01.0.08M.08/1920x1080')).toBe(false);
		expect(loads()).toBe(1);
	});

	test('never dav1d-wasm for HEVC or H.264', async () => {
		const { e, loads } = env([], []);
		const r = await probe([HEVC, H264], e);
		expect(r.decode).toEqual([]);
		expect(loads()).toBe(0);
	});

	test('the test hook: no hardware decoders, and no WebCodecs AV1', async () => {
		const { e, asked } = env(['av01', 'avc1'], ['av01', 'avc1']);
		e.noHardware = true;
		const r = await probe([AV1], e);
		expect(
			r.decode.map((d) => [d.codec, d.implementation, d.hardwareAcceleration])
		).toEqual([['av01.0.08M.08', 'dav1d-wasm', undefined]]);
		expect(asked).toEqual([]);
	});

	test('each distinct configuration is tested once', async () => {
		const { e, asked } = env([], ['avc1']);
		const two: VideoOffer = {
			...H264,
			stripes: [H264.stripes[0], { ...H264.stripes[0], i: 1, y: 1080 }],
		};
		await probe([two, H264], e);
		expect(asked.length).toBe(2);
	});
});

test('dav1d.wasm decodes Main 8-bit 4:2:0 only', () => {
	expect(dav1dDecodes('av01.0.08M.08')).toBe(true);
	expect(dav1dDecodes('av01.0.13M.08.0.110.01.13.06.0')).toBe(true);
	expect(dav1dDecodes('av01.0.13M.10')).toBe(false);
	expect(dav1dDecodes('av01.1.13M.08')).toBe(false);
	expect(dav1dDecodes('av01.0.13M.08.1.110.01.01.01.0')).toBe(false);
	expect(dav1dDecodes('av01.2.13M.08.0.100.01.01.01.0')).toBe(false);
	expect(dav1dDecodes('hvc1.1.6.L153.B0')).toBe(false);
});
