// The worker's decoder adapter: one decoder per stripe, WebCodecs or dav1d-wasm behind
// the same interface, each output a picture the worker draws once and closes.
import type { Implementation } from './protocol';

export interface Picture {
	readonly timestamp: number;
	/** Draws the picture's top-left `w` x `h` at (0, `y`). */
	draw(
		ctx: OffscreenCanvasRenderingContext2D,
		y: number,
		w: number,
		h: number
	): void;
	close(): void;
}

export interface DecoderSink {
	output(p: Picture): void;
	/** The chunk with this timestamp decoded to no picture. */
	none(timestamp: number): void;
	error(e: Error): void;
}

export interface StripeDecoder {
	readonly implementation: Implementation;
	/** Chunks submitted and not yet decoded. */
	readonly queued: number;
	decode(data: Uint8Array, key: boolean, timestamp: number): void;
	close(): void;
}

class FramePicture implements Picture {
	constructor(private readonly frame: VideoFrame) {}
	get timestamp() {
		return this.frame.timestamp;
	}
	draw(
		ctx: OffscreenCanvasRenderingContext2D,
		y: number,
		w: number,
		h: number
	) {
		const sw = Math.min(w, this.frame.displayWidth);
		const sh = Math.min(h, this.frame.displayHeight);
		ctx.drawImage(this.frame, 0, 0, sw, sh, 0, y, sw, sh);
	}
	close() {
		this.frame.close();
	}
}

export class WebCodecsDecoder implements StripeDecoder {
	readonly implementation = 'webcodecs';
	private readonly dec: VideoDecoder;
	private pending = 0;
	constructor(config: VideoDecoderConfig, sink: DecoderSink) {
		this.dec = new VideoDecoder({
			output: (frame) => {
				this.pending = Math.max(0, this.pending - 1);
				sink.output(new FramePicture(frame));
			},
			error: (e) => sink.error(e),
		});
		this.dec.configure(config);
	}
	get queued() {
		return this.pending;
	}
	decode(data: Uint8Array, key: boolean, timestamp: number) {
		this.dec.decode(
			new EncodedVideoChunk({ type: key ? 'key' : 'delta', timestamp, data })
		);
		this.pending++;
	}
	close() {
		if (this.dec.state !== 'closed')
			try {
				this.dec.close();
			} catch {}
	}
}

/** What the lazily loaded dav1d.js bundle provides once its wasm is initialized. */
export interface Dav1d {
	/** How its pictures are drawn. */
	renderer: 'webgl2' | '2d';
	open(sink: DecoderSink): StripeDecoder;
}

/** dav1d.js sets this on the worker's global scope. */
export interface Dav1dBundle {
	load(wasmUrl: string): Promise<Dav1d>;
}
