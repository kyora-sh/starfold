import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import {
	av1Codec,
	avcCodec,
	familyOf,
	hevcCodec,
	nalUnits,
	sequenceHeader,
} from '../src/codecs';

const bytes = (...h: string[]) =>
	Uint8Array.from(
		h
			.join(' ')
			.split(/\s+/)
			.filter(Boolean)
			.map((x) => parseInt(x, 16))
	);

class BitWriter {
	private bits: number[] = [];
	f(n: number, v: number) {
		for (let i = n - 1; i >= 0; i--) this.bits.push(Math.floor(v / 2 ** i) % 2);
		return this;
	}
	bytes(): Uint8Array {
		while (this.bits.length % 8) this.bits.push(0);
		const out = new Uint8Array(this.bits.length / 8);
		this.bits.forEach((b, i) => (out[i >> 3] |= b << (7 - (i & 7))));
		return out;
	}
}

/** A temporal delimiter and a sequence header OBU, both with size fields. */
function temporalUnit(seq: Uint8Array): Uint8Array {
	return Uint8Array.from([0x12, 0x00, 0x0a, seq.length, ...seq, 0x32, 0x00]);
}

describe('avc1', () => {
	test('from the SPS of an Annex-B keyframe, after an AUD', () => {
		const key = bytes(
			'00 00 00 01 09 10',
			'00 00 00 01 67 64 00 1f ac d9 40 50 05 bb 01 10',
			'00 00 00 01 68 eb e3 cb 22 c0',
			'00 00 01 65 88 84 00 33'
		);
		expect(avcCodec(key)).toBe('avc1.64001f');
	});
	test('none without an SPS', () => {
		expect(avcCodec(bytes('00 00 01 41 9a 00'))).toBeNull();
	});
});

describe('hvc1', () => {
	// General profile_tier_level: Main, compatible with Main and Main10, progressive
	// and frame-only, level 3.1; the zero runs carry emulation-prevention bytes.
	const sps =
		'42 01 01 01 60 00 00 03 00 90 00 00 03 00 00 03 00 5d a0 02 80 80 2d';
	test('from the SPS, with emulation prevention removed', () => {
		const key = bytes(
			'00 00 00 01 40 01 0c 01 ff ff 01 60 00 00 03 00 90',
			'00 00 00 01',
			sps,
			'00 00 00 01 44 01 c1 72 b4 62 40',
			'00 00 01 26 01 af 06 b8'
		);
		expect(hevcCodec(key)).toBe('hvc1.1.6.L93.90');
	});
	test('high tier and a profile space', () => {
		const key = bytes('00 00 01 42 01 01 a2 20 00 00 00 00 00 00 00 00 00 99');
		expect(hevcCodec(key)).toBe('hvc1.B2.4.H153');
	});
});

describe('av01', () => {
	test('the 8-bit example of the design: BT.709 primaries, sRGB transfer, BT.601', () => {
		const seq = new BitWriter()
			.f(3, 0)
			.f(1, 0)
			.f(1, 0)
			.f(1, 0)
			.f(1, 0)
			.f(5, 0)
			.f(12, 0)
			.f(5, 13)
			.f(1, 0)
			.f(4, 11)
			.f(4, 11)
			.f(12, 2559)
			.f(12, 1599)
			.f(1, 0)
			.f(3, 0b011)
			.f(4, 0)
			.f(1, 0)
			.f(1, 1)
			.f(1, 1)
			.f(3, 0b011)
			.f(1, 0)
			.f(1, 0)
			.f(1, 1)
			.f(8, 1)
			.f(8, 13)
			.f(8, 6)
			.f(1, 0)
			.f(2, 0)
			.f(1, 0)
			.f(1, 0)
			.bytes();
		expect(av1Codec(temporalUnit(seq))).toBe('av01.0.13M.08.0.110.01.13.06.0');
	});

	test('timing and decoder model info, two operating points, high tier, 10-bit full range', () => {
		const seq = new BitWriter()
			.f(3, 0)
			.f(1, 0)
			.f(1, 0)
			.f(1, 1)
			.f(32, 1001)
			.f(32, 60000)
			.f(1, 1)
			.f(1, 1)
			.f(1, 1)
			.f(5, 9)
			.f(32, 1)
			.f(5, 4)
			.f(5, 4)
			.f(1, 1)
			.f(5, 1)
			.f(12, 0x103)
			.f(5, 16)
			.f(1, 1)
			.f(1, 1)
			.f(10, 300)
			.f(10, 200)
			.f(1, 0)
			.f(1, 1)
			.f(4, 3)
			.f(12, 0x101)
			.f(5, 5)
			.f(1, 0)
			.f(1, 0)
			.f(4, 15)
			.f(4, 15)
			.f(16, 3839)
			.f(16, 2159)
			.f(1, 1)
			.f(4, 2)
			.f(3, 1)
			.f(3, 0b111)
			.f(4, 0)
			.f(1, 1)
			.f(2, 0b11)
			.f(1, 0)
			.f(1, 1)
			.f(1, 0)
			.f(1, 1)
			.f(3, 6)
			.f(3, 0b011)
			.f(1, 1)
			.f(1, 0)
			.f(1, 1)
			.f(8, 9)
			.f(8, 16)
			.f(8, 9)
			.f(1, 1)
			.f(2, 2)
			.f(1, 0)
			.f(1, 0)
			.bytes();
		expect(av1Codec(temporalUnit(seq))).toBe('av01.0.16H.10.0.112.09.16.09.1');
	});

	test('the committed fixture: Main, level 2.1, BT.709 limited range', () => {
		const ivf = readFileSync(
			new URL('../../../bench/tests/fixtures/av1-640x360.ivf', import.meta.url)
		);
		const size = ivf.readUInt32LE(32);
		const first = new Uint8Array(ivf.subarray(44, 44 + size));
		expect(sequenceHeader(first)).not.toBeNull();
		expect(av1Codec(first)).toBe('av01.0.01M.08');
	});

	test('none without a sequence header, or from a truncated one', () => {
		expect(
			av1Codec(Uint8Array.from([0x12, 0x00, 0x32, 0x01, 0x00]))
		).toBeNull();
		expect(av1Codec(Uint8Array.from([0x0a, 0x02, 0x00, 0x00]))).toBeNull();
	});
});

test('Annex-B NAL units drop trailing zeros of four-byte start codes', () => {
	const n = nalUnits(
		bytes('00 00 00 01 67 64 00 00 00 01 68 ee 00 00 01 65 00')
	);
	expect(n.map((u) => Array.from(u))).toEqual([
		[0x67, 0x64],
		[0x68, 0xee],
		[0x65, 0x00],
	]);
});

test('codec families', () => {
	expect(familyOf('avc1.640034')).toBe('h264');
	expect(familyOf('hvc1.1.6.L153.B0')).toBe('hevc');
	expect(familyOf('av01.0.13M.08')).toBe('av1');
	expect(familyOf('vp09.00.10.08')).toBeNull();
});
