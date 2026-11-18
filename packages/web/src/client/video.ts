// Presenting decoded frames and fitting the desktop to the window.
import type { Stripe, StripeFrame } from '../protocol';
import type { App } from './app';

interface StripeOut {
	video: HTMLVideoElement;
	gen: MediaStreamTrackGenerator;
	writer: WritableStreamDefaultWriter<VideoFrame>;
	writing: boolean;
	next: VideoFrame | null;
}

/**
 * Prefer handing decoded frames to a <video> via MediaStreamTrackGenerator: the
 * compositor scans YUV out directly instead of rasterizing into a 2D canvas. One
 * <video> per encoded stripe, stacked; the worker hands over every stripe of a frame
 * in one message so they are written to their tracks together.
 */
// At most one write in flight per track; a newer frame replaces a waiting one.
function write(o: StripeOut, frame: VideoFrame) {
	if (o.writing) {
		o.next?.close();
		o.next = frame;
		return;
	}
	o.writing = true;
	o.writer
		.write(frame)
		.catch(() => {})
		.finally(() => {
			o.writing = false;
			const next = o.next;
			if (next) {
				o.next = null;
				write(o, next);
			}
		});
}

const gcd = (a: number, b: number): number => (b ? gcd(b, a % b) : a);

export function createVideo(
	app: App,
	canvas: HTMLCanvasElement,
	useVideo: boolean,
	onLayout: () => void,
	onFirstPaint: () => void,
	/** The CSS pixels the desktop is fitted into: the window, unless embedded. */
	viewport: () => { w: number; h: number } = () => ({
		w: window.innerWidth,
		h: window.innerHeight,
	}),
	/** Inline styles for each stripe <video>; the served page styles `.stripe` in its CSS. */
	stripeStyle: Partial<Record<keyof CSSStyleDeclaration, string>> = {},
	/**
	 * Never larger than the viewport (embedded): when even the smallest seam-free height
	 * doesn't fit, the picture fits exactly instead.
	 */
	contain = false
) {
	let outs: StripeOut[] = [];
	let stripes: Stripe[] = [[0, 1]];
	/** Waiting for the first presented frame of a stripe <video> (re-armed if it is removed first). */
	let armed = false;
	let painted = false;
	/** The pending first-paint callback, for dispose(). */
	let paintWait: { v: HTMLVideoElement; id: number; frame: boolean } | null =
		null;
	/** Where the worker presents: <video> stripes, or the canvas (always for dav1d-wasm). */
	let surface: 'video' | 'canvas' = useVideo ? 'video' : 'canvas';
	const show = () => {
		canvas.style.display = surface === 'canvas' ? '' : 'none';
		for (const o of outs)
			o.video.style.display = surface === 'video' ? '' : 'none';
	};
	show();

	/** Drops the stripes. A <video> without data delays the page's load event until it has
	 * some, even once removed, so each source is detached first. */
	function clear() {
		for (const o of outs) {
			o.video.srcObject = null;
			o.gen.stop();
			o.next?.close();
			o.next = null;
			o.video.remove();
		}
		outs = [];
		armed = false;
	}

	function build(list: Stripe[]) {
		stripes = list;
		clear();
	}

	/** The stripes' <video>s, made with their first frames so none ever waits for data. */
	function open() {
		for (const [y, h] of stripes) {
			const gen = new MediaStreamTrackGenerator({ kind: 'video' });
			const video = document.createElement('video');
			video.className = 'stripe';
			Object.assign(video.style, stripeStyle);
			video.muted = true;
			video.autoplay = true;
			video.playsInline = true;
			video.disablePictureInPicture = true;
			video.style.top = `${(y / app.desk.h) * 100}%`;
			video.style.height = `${(h / app.desk.h) * 100}%`;
			video.srcObject = new MediaStream([gen]);
			app.surface.appendChild(video);
			video.play().catch(() => {});
			outs.push({
				video,
				gen,
				writer: gen.writable.getWriter(),
				writing: false,
				next: null,
			});
		}
	}

	function present(parts: StripeFrame[]) {
		if (!useVideo) {
			for (const p of parts) p.frame.close();
			return;
		}
		if (!outs.length) open();
		if (!painted && !armed && outs[0] && parts.length) {
			armed = true;
			const v = outs[0].video;
			const done = () => {
				if (!v.isConnected) return;
				painted = true;
				onFirstPaint();
			};
			if ('requestVideoFrameCallback' in v)
				paintWait = { v, id: v.requestVideoFrameCallback(done), frame: false };
			else paintWait = { v, id: requestAnimationFrame(done), frame: true };
		}
		for (const p of parts) {
			const o = outs[p.i];
			if (o) write(o, p.frame);
			else p.frame.close();
		}
	}

	function setSurface(s: 'video' | 'canvas') {
		surface = useVideo ? s : 'canvas';
		if (surface === 'canvas') clear();
		show();
	}

	function layout() {
		const { w: dw, h: dh } = app.desk;
		if (!dw) return;
		const { w: vw, h: vh } = viewport();
		const dpr = window.devicePixelRatio || 1;
		// Snap the height so every stripe boundary lands on a whole device pixel;
		// otherwise the stacked stripe layers show hairline seams.
		const q = dh / stripes.reduce((g, [y]) => gcd(g, y), dh);
		const fit = (Math.min(vw / dw, vh / dh) || 1) * dh * dpr;
		const snapped = Math.floor(fit / q) * q;
		const devH = contain && snapped < q ? fit : Math.max(q, snapped);
		app.view.scale = devH / dpr / dh;
		app.surface.style.width = `${dw * app.view.scale}px`;
		app.surface.style.height = `${dh * app.view.scale}px`;
		onLayout();
	}

	/** Drops the stripes and a pending first-paint callback (embedded: on unmount). */
	function dispose() {
		if (paintWait?.frame) cancelAnimationFrame(paintWait.id);
		else if (paintWait) paintWait.v.cancelVideoFrameCallback(paintWait.id);
		paintWait = null;
		clear();
	}

	return { build, present, layout, setSurface, dispose };
}
