// WebCodecs codec strings read from a keyframe's own parameter sets: avc1 from the H.264
// SPS, hvc1 from the HEVC SPS (ISO/IEC 14496-15 annex E), av01 from the AV1 sequence
// header (AV1 ISOBMFF codecs parameter).

export type CodecFamily = 'h264' | 'hevc' | 'av1';

/** The NAL units of an Annex-B buffer, header byte first, start codes stripped. */
export function nalUnits(d: Uint8Array): Uint8Array[] {
	const starts: number[] = [];
	for (let i = 0; i + 3 <= d.length; i++)
		if (d[i] === 0 && d[i + 1] === 0 && d[i + 2] === 1) {
			starts.push(i + 3);
			i += 2;
		}
	return starts
		.map((s, k) => {
			let e = k + 1 < starts.length ? starts[k + 1] - 3 : d.length;
			while (k + 1 < starts.length && e > s && d[e - 1] === 0) e--;
			return d.subarray(s, e);
		})
		.filter((n) => n.length > 0);
}

const hex2 = (x: number) => x.toString(16).padStart(2, '0');

export function avcCodec(d: Uint8Array): string | null {
	const sps = nalUnits(d).find((n) => (n[0] & 0x1f) === 7 && n.length >= 4);
	return sps ? `avc1.${hex2(sps[1])}${hex2(sps[2])}${hex2(sps[3])}` : null;
}

/** A NAL unit's payload without its header and emulation-prevention bytes. */
function rbsp(nal: Uint8Array, header: number): Uint8Array {
	const out: number[] = [];
	let zeros = 0;
	for (let i = header; i < nal.length; i++) {
		const b = nal[i];
		if (zeros >= 2 && b === 3) {
			zeros = 0;
			continue;
		}
		zeros = b === 0 ? zeros + 1 : 0;
		out.push(b);
	}
	return Uint8Array.from(out);
}

function reverseBits32(x: number): number {
	let r = 0;
	for (let i = 0; i < 32; i++) r = (r << 1) | ((x >>> i) & 1);
	return r >>> 0;
}

export function hevcCodec(d: Uint8Array): string | null {
	const sps = nalUnits(d).find((n) => ((n[0] >> 1) & 0x3f) === 33);
	if (!sps) return null;
	const r = rbsp(sps, 2);
	if (r.length < 13) return null;
	const p = r.subarray(1, 13);
	const space = ['', 'A', 'B', 'C'][p[0] >> 6];
	const tier = p[0] & 0x20 ? 'H' : 'L';
	const compat = reverseBits32(
		((p[1] << 24) | (p[2] << 16) | (p[3] << 8) | p[4]) >>> 0
	);
	const constraints = [...p.subarray(5, 11)];
	while (constraints.length && constraints[constraints.length - 1] === 0)
		constraints.pop();
	return (
		`hvc1.${space}${p[0] & 0x1f}.${compat.toString(16).toUpperCase()}.${tier}${p[11]}` +
		constraints.map((b) => '.' + b.toString(16).toUpperCase()).join('')
	);
}

class Bits {
	private pos = 0;
	constructor(private readonly d: Uint8Array) {}
	bit(): number {
		const i = this.pos >> 3;
		if (i >= this.d.length) throw new RangeError('sequence header ends early');
		const b = (this.d[i] >> (7 - (this.pos & 7))) & 1;
		this.pos++;
		return b;
	}
	f(n: number): number {
		let v = 0;
		for (let i = 0; i < n; i++) v = v * 2 + this.bit();
		return v;
	}
	uvlc(): number {
		let zeros = 0;
		while (!this.bit()) zeros++;
		return zeros >= 32 ? 2 ** 32 - 1 : 2 ** zeros - 1 + this.f(zeros);
	}
}

function leb128(d: Uint8Array, i: number): [value: number, next: number] {
	let v = 0;
	for (let k = 0; k < 8 && i < d.length; k++) {
		const b = d[i++];
		v += (b & 0x7f) * 2 ** (7 * k);
		if (!(b & 0x80)) return [v, i];
	}
	throw new RangeError('bad leb128');
}

/** The payload of the first sequence header OBU in a temporal unit. */
export function sequenceHeader(d: Uint8Array): Uint8Array | null {
	let i = 0;
	while (i < d.length) {
		const h = d[i];
		const type = (h >> 3) & 0xf;
		let j = i + 1 + ((h >> 2) & 1);
		let size = d.length - j;
		if (h & 2) [size, j] = leb128(d, j);
		if (j + size > d.length) return null;
		if (type === 1) return d.subarray(j, j + size);
		i = j + size;
	}
	return null;
}

const dec2 = (x: number) => String(x).padStart(2, '0');

export function av1Codec(d: Uint8Array): string | null {
	const sh = sequenceHeader(d);
	if (!sh) return null;
	try {
		return av1FromSequenceHeader(sh);
	} catch {
		return null;
	}
}

function av1FromSequenceHeader(sh: Uint8Array): string {
	const b = new Bits(sh);
	const profile = b.f(3);
	b.f(1);
	const reduced = b.f(1);
	let level = 0;
	let tier = 0;
	if (reduced) level = b.f(5);
	else {
		let decoderModel = 0;
		let bufferDelayBits = 0;
		if (b.f(1)) {
			b.f(32);
			b.f(32);
			if (b.f(1)) b.uvlc();
			decoderModel = b.f(1);
			if (decoderModel) {
				bufferDelayBits = b.f(5) + 1;
				b.f(32);
				b.f(5);
				b.f(5);
			}
		}
		const displayDelay = b.f(1);
		const points = b.f(5) + 1;
		for (let op = 0; op < points; op++) {
			b.f(12);
			const l = b.f(5);
			const t = l > 7 ? b.f(1) : 0;
			if (op === 0) {
				level = l;
				tier = t;
			}
			if (decoderModel && b.f(1)) {
				b.f(bufferDelayBits);
				b.f(bufferDelayBits);
				b.f(1);
			}
			if (displayDelay && b.f(1)) b.f(4);
		}
	}
	const wBits = b.f(4) + 1;
	const hBits = b.f(4) + 1;
	b.f(wBits);
	b.f(hBits);
	if (!reduced && b.f(1)) {
		b.f(4);
		b.f(3);
	}
	b.f(3);
	if (!reduced) {
		b.f(4);
		const orderHint = b.f(1);
		if (orderHint) b.f(2);
		const sct = b.f(1) ? 2 : b.f(1);
		if (sct > 0 && !b.f(1)) b.f(1);
		if (orderHint) b.f(3);
	}
	b.f(3);
	const high = b.f(1);
	const depth = profile === 2 && high ? (b.f(1) ? 12 : 10) : high ? 10 : 8;
	const mono = profile === 1 ? 0 : b.f(1);
	let [cp, tc, mc] = [2, 2, 2];
	if (b.f(1)) [cp, tc, mc] = [b.f(8), b.f(8), b.f(8)];
	let range = 0;
	let [ssx, ssy, csp] = [1, 1, 0];
	if (mono) range = b.f(1);
	else if (cp === 1 && tc === 13 && mc === 0) {
		range = 1;
		[ssx, ssy] = [0, 0];
	} else {
		range = b.f(1);
		if (profile === 1) [ssx, ssy] = [0, 0];
		else if (profile === 2) {
			if (depth === 12) {
				ssx = b.f(1);
				ssy = ssx ? b.f(1) : 0;
			} else [ssx, ssy] = [1, 0];
		}
		if (ssx && ssy) csp = b.f(2);
	}
	const base = `av01.${profile}.${dec2(level)}${tier ? 'H' : 'M'}.${dec2(depth)}`;
	const rest = `${mono}.${ssx}${ssy}${csp}.${dec2(cp)}.${dec2(tc)}.${dec2(mc)}.${range}`;
	return rest === '0.110.01.01.01.0' ? base : `${base}.${rest}`;
}

export function keyframeCodec(
	family: CodecFamily,
	d: Uint8Array
): string | null {
	if (family === 'h264') return avcCodec(d);
	if (family === 'hevc') return hevcCodec(d);
	return av1Codec(d);
}

export function familyOf(codec: string): CodecFamily | null {
	if (codec.startsWith('avc1.') || codec.startsWith('avc3.')) return 'h264';
	if (codec.startsWith('hvc1.') || codec.startsWith('hev1.')) return 'hevc';
	if (codec.startsWith('av01.')) return 'av1';
	return null;
}
