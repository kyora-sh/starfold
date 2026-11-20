// What this browser decodes of the server's offers: WebCodecs, hardware preferred, and
// dav1d-wasm for every stripe of each AV1 offer it decodes whole, once no offer is
// covered stripe for stripe by WebCodecs. The server needs one implementation for all
// stripes of an offer and ranks every WebCodecs choice above dav1d-wasm, so dav1d-wasm
// is only fetched when it would be picked.
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
	const covered = offers.some((o) =>
		o.stripes.every((s) => accepted.has(configKey(s.config)))
	);
	const wasm = new Map<string, DecoderConfig>();
	for (const o of offers)
		if (
			o.codec === 'av1' &&
			o.stripes.every((s) => env.dav1dDecodes(s.config.codec))
		)
			for (const s of o.stripes) wasm.set(configKey(s.config), s.config);
	if (!covered && wasm.size > 0 && (await env.dav1d()))
		for (const [k, c] of wasm) {
			accepted.set(k, { implementation: 'dav1d-wasm' });
			decode.push({ ...c, implementation: 'dav1d-wasm' });
		}
	return { decode, accepted };
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
