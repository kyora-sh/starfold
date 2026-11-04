// dav1d.js: the AV1 fallback, loaded by the worker only when it is selected. Runs
// dav1d.wasm (packages/web/dav1d) single-threaded, one decoder per stripe.
import type {
	Dav1d,
	Dav1dBundle,
	DecoderSink,
	Picture,
	StripeDecoder,
} from '../decoders';
import { YuvRenderer } from './yuv';

declare const self: DedicatedWorkerGlobalScope & { starfoldDav1d: Dav1dBundle };

interface Exports {
	memory: WebAssembly.Memory;
	_initialize(): void;
	sf_alloc(n: number): number;
	sf_open(): number;
	sf_close(ctx: number): void;
	sf_decode(ctx: number, buf: number, len: number, err: number): number;
	sf_release(pic: number): void;
}

/** Fields of shim.c's Picture, as 32-bit words. */
const enum F {
	Width,
	Height,
	StrideY,
	StrideUV,
	Y,
	U,
	V,
	Matrix,
	FullRange,
	Count,
}

class YuvPicture implements Picture {
	constructor(
		private readonly x: Exports,
		private readonly r: YuvRenderer,
		private pic: number,
		readonly timestamp: number
	) {}
	draw(
		ctx: OffscreenCanvasRenderingContext2D,
		y: number,
		w: number,
		h: number
	) {
		if (!this.pic) return;
		const f = new Uint32Array(this.x.memory.buffer, this.pic, F.Count);
		this.r.draw(
			ctx,
			{
				heap: new Uint8Array(this.x.memory.buffer),
				strideY: f[F.StrideY],
				strideUV: f[F.StrideUV],
				y: f[F.Y],
				u: f[F.U],
				v: f[F.V],
				matrix: f[F.Matrix],
				fullRange: f[F.FullRange] !== 0,
			},
			y,
			Math.min(w, f[F.Width]),
			Math.min(h, f[F.Height])
		);
	}
	close() {
		if (this.pic) this.x.sf_release(this.pic);
		this.pic = 0;
	}
}

class Dav1dDecoder implements StripeDecoder {
	readonly implementation = 'dav1d-wasm';
	readonly queued = 0;
	private ctx: number;
	constructor(
		private readonly x: Exports,
		private readonly err: number,
		private readonly r: YuvRenderer,
		private readonly sink: DecoderSink
	) {
		this.ctx = x.sf_open();
		if (!this.ctx) throw new Error('dav1d: no decoder');
	}
	decode(data: Uint8Array, _key: boolean, timestamp: number) {
		if (!this.ctx) throw new Error('dav1d: decoder closed');
		const x = this.x;
		const buf = x.sf_alloc(data.length);
		if (!buf) return this.fail('out of memory');
		new Uint8Array(x.memory.buffer, buf, data.length).set(data);
		const pic = x.sf_decode(this.ctx, buf, data.length, this.err);
		const code = new Int32Array(x.memory.buffer, this.err, 1)[0];
		if (pic) this.sink.output(new YuvPicture(x, this.r, pic, timestamp));
		else if (code) this.fail(`decode error ${code}`);
		else this.sink.none(timestamp);
	}
	private fail(s: string) {
		const sink = this.sink;
		queueMicrotask(() => sink.error(new Error(`dav1d: ${s}`)));
	}
	close() {
		if (this.ctx) this.x.sf_close(this.ctx);
		this.ctx = 0;
	}
}

async function load(wasmUrl: string): Promise<Dav1d> {
	let memory: WebAssembly.Memory | null = null;
	const imports = {
		wasi_snapshot_preview1: {
			clock_time_get: (_id: number, _precision: bigint, out: number) => {
				new DataView(memory!.buffer).setBigUint64(
					out,
					BigInt(Math.round(performance.now() * 1e6)),
					true
				);
				return 0;
			},
		},
	};
	const { instance } = await WebAssembly.instantiateStreaming(
		fetch(wasmUrl, { credentials: 'same-origin' }),
		imports
	);
	const x = instance.exports as unknown as Exports;
	memory = x.memory;
	x['_initialize']();
	const err = x.sf_alloc(4);
	const probe = x.sf_open();
	if (!err || !probe) throw new Error('dav1d: initialization failed');
	x.sf_close(probe);
	const r = new YuvRenderer();
	return {
		renderer: r.backend,
		open: (sink) => new Dav1dDecoder(x, err, r, sink),
	};
}

self.starfoldDav1d = { load };
