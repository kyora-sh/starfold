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
	/** Hands over the picture's top-left `w` x `h` as a VideoFrame, if it is one; closing
	 * that is then the taker's. */
	take?(w: number, h: number): VideoFrame;
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
	private taken = false;
	constructor(private readonly frame: VideoFrame) {}
	take(w: number, h: number): VideoFrame {
		this.taken = true;
		const f = this.frame;
		const r = f.visibleRect;
		const cw = Math.min(w, f.displayWidth);
		const ch = Math.min(h, f.displayHeight);
		if (!r || (cw === f.displayWidth && ch === f.displayHeight)) return f;
		try {
			const c = new VideoFrame(f, {
				visibleRect: { x: r.x, y: r.y, width: cw, height: ch },
			});
			f.close();
			return c;
		} catch {
			return f;
		}
	}
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
		if (!this.taken) this.frame.close();
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
