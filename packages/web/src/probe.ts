// What this browser decodes of the server's offers: WebCodecs, hardware preferred, and
// dav1d-wasm for AV1 only when the browser has no hardware-backed WebCodecs decoder for
// AV1, HEVC or H.264 and none of its own for AV1.
import type {
	DecodeEntry,
	DecoderConfig,
	Implementation,
	VideoOffer,
} from './protocol';

/** How a tested configuration is decoded. */
export interface Accepted {
	implementation: Implementation;
	hardwareAcceleration?: HardwareAcceleration;
}

export interface ProbeEnv {
	/** VideoDecoder.isConfigSupported, false when it throws or WebCodecs is missing. */
	supported(c: VideoDecoderConfig): Promise<boolean>;
	/** Loads and initializes dav1d-wasm; whether it is ready. */
	dav1d(): Promise<boolean>;
	/** dav1d-wasm decodes this AV1 codec string (8-bit 4:2:0 only). */
	dav1dDecodes(codec: string): boolean;
	/** Test hook: no hardware-backed WebCodecs decoder, and no WebCodecs AV1. */
	noHardware?: boolean;
}

export const configKey = (c: DecoderConfig) =>
	`${c.codec}/${c.codedWidth}x${c.codedHeight}`;

export function decoderConfig(
	c: DecoderConfig,
	hardwareAcceleration: HardwareAcceleration
): VideoDecoderConfig {
	return {
		codec: c.codec,
		codedWidth: c.codedWidth,
		codedHeight: c.codedHeight,
		optimizeForLatency: true,
		hardwareAcceleration,
	};
}

export interface ProbeResult {
	decode: DecodeEntry[];
	/** By configKey: WebCodecs when it decodes the configuration, else dav1d-wasm. */
	accepted: Map<string, Accepted>;
}

export async function probe(
	offers: VideoOffer[],
	env: ProbeEnv
): Promise<ProbeResult> {
	const all = new Map<string, { c: DecoderConfig; av1: boolean }>();
	for (const o of offers)
		for (const s of o.stripes)
			all.set(configKey(s.config), { c: s.config, av1: o.codec === 'av1' });
	const tested = await Promise.all(
		[...all].map(async ([k, { c, av1 }]) => {
			const hw = await webcodecs(c, av1, env);
			return { k, c, av1, hw };
		})
	);
	const accepted = new Map<string, Accepted>();
	const decode: DecodeEntry[] = [];
	for (const t of tested)
		if (t.hw) {
			accepted.set(t.k, {
				implementation: 'webcodecs',
				hardwareAcceleration: t.hw,
			});
			decode.push({
				...t.c,
				implementation: 'webcodecs',
				hardwareAcceleration: t.hw,
			});
		}
	const wasm = tested.filter(
		(t) => t.av1 && !t.hw && env.dav1dDecodes(t.c.codec)
	);
	const noHardware =
		!tested.some((t) => t.hw === 'prefer-hardware') &&
		wasm.length > 0 &&
		!(await hardwareElsewhere(wasm[0].c, env));
	if (noHardware && (await env.dav1d()))
		for (const t of wasm) {
			accepted.set(t.k, { implementation: 'dav1d-wasm' });
			decode.push({ ...t.c, implementation: 'dav1d-wasm' });
		}
	return { decode, accepted };
}

/** Codecs the server may not offer still count: a hardware decoder for any of them
 * means this browser has a native path, and dav1d-wasm stays out. */
const REPRESENTATIVE = ['av01.0.08M.08', 'hvc1.1.6.L120.90', 'avc1.640028'];

async function hardwareElsewhere(
	size: DecoderConfig,
	env: ProbeEnv
): Promise<boolean> {
	if (env.noHardware) return false;
	const found = await Promise.all(
		REPRESENTATIVE.map((codec) =>
			env.supported(decoderConfig({ ...size, codec }, 'prefer-hardware'))
		)
	);
	return found.includes(true);
}

async function webcodecs(
	c: DecoderConfig,
	av1: boolean,
	env: ProbeEnv
): Promise<HardwareAcceleration | null> {
	if (env.noHardware && av1) return null;
	if (
		!env.noHardware &&
		(await env.supported(decoderConfig(c, 'prefer-hardware')))
	)
		return 'prefer-hardware';
	if (await env.supported(decoderConfig(c, 'no-preference')))
		return 'no-preference';
	return null;
}

/** dav1d.wasm is built for profile 0 (Main), 8-bit, 4:2:0. */
export function dav1dDecodes(codec: string): boolean {
	const p = codec.split('.');
	return (
		p[0] === 'av01' &&
		p[1] === '0' &&
		p[3] === '08' &&
		(p[4] === undefined || p[4] === '0') &&
		(p[5] === undefined || p[5].startsWith('11'))
	);
}
