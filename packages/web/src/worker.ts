// Owns the WebSocket and decodes the video. Complete frames go to the page's <video>
// stripes (MediaStreamTrackGenerator, where the browser has it), or are painted straight
// onto its OffscreenCanvas the moment they are complete; dav1d-wasm pictures always are.
//
// A protocol 2 server offers codecs in its hello: the worker tests each offered
// decoder configuration, tells the server which it decodes, and configures its
// decoders from the server's `codec` message of each epoch. An older server gets
// protocol 1: H.264, configured from each keyframe's SPS.
//
// Decoders are WebCodecs, or dav1d-wasm for AV1 on browsers without any hardware
// WebCodecs decoder; dav1d.js and dav1d.wasm are fetched only when that is needed.
import { FrameQueue, Percentiles } from './assembly';
import type { Part } from './assembly';
import { avcCodec, familyOf, keyframeCodec } from './codecs';
import type { Dav1d, Dav1dBundle, Picture, StripeDecoder } from './decoders';
import { WebCodecsDecoder } from './decoders';
import { frameBinary, frameText, unframe } from './frame';
import { FellBehind, StreamRate } from './pace';
import { configKey, dav1dDecodes, decoderConfig, probe } from './probe';
import type { Accepted } from './probe';
import {
	Bin,
	cmd,
	isCurrent,
	msg,
	parseVideoHeader,
	VIDEO_HEADER_BYTES,
	VIDEO_V2_HEADER_BYTES,
} from './protocol';
import type {
	CodecMessage,
	StripeFrame,
	FromWorker,
	Hello,
	Implementation,
	PcmChunk,
	ServerMessage,
	Stats,
	Stripe,
	ToWorker,
	VideoOffer,
} from './protocol';

declare const self: DedicatedWorkerGlobalScope & {
	starfoldDav1d?: Dav1dBundle;
};

const query = new URLSearchParams(self.location.search);
const noDraw = query.has('nodraw');
/** Test hook: act as a browser with no hardware WebCodecs decoder and no WebCodecs AV1. */
const noHardware = query.has('nohwdecode');
/** Frames waiting for their stripes before the decoders are taken as stuck. */
const MAX_PENDING = 32;
/** How long the capability hello waits for the page's display rate. */
const HZ_WAIT_MS = 1000;

let presentToMain = false;
let surface: 'video' | 'canvas' | null = null;
let painted = false;
let ws: WebSocket | null = null;
let url = '';
let framed = false;
let canvas: OffscreenCanvas | null = null;
let ctx: OffscreenCanvasRenderingContext2D | null = null;
let audioDecoder: AudioDecoder | null = null;
let audioPort: MessagePort | null = null;
let audioTs = 0;
let wantVideo = true;
let wantAudio = false;
/** 0 until the server's hello says which protocol it speaks. */
let proto = 0;
/** `v,` waits for the capability hello, so the server doesn't take this for an old client. */
let videoSent = false;
let helloSeq = 0;
/** One per socket: async work started on an earlier connection is dropped. */
let conn = 0;
/** Protocol 2: the epoch announced last, and whether the decoders are set up for it. */
let epoch = 0;
let ready = false;
/** Protocol 2: each stripe's decoder in this epoch. */
let plans: StripePlan[] = [];
/** Protocol 2: the server's encoder in this epoch. */
let encoder: string | null = null;
/** How each tested configuration decodes here. */
const accepted = new Map<string, Accepted>();
/** Decoder errors in this epoch; a codec that keeps failing is reported. */
let errors: number[] = [];
let dav1d: Dav1d | null = null;
let dav1dLoad: Promise<Dav1d | null> | null = null;

interface StripePlan {
	implementation: Implementation;
	config: VideoDecoderConfig;
	hardwareAcceleration?: HardwareAcceleration;
}

// The screen arrives as independent stripes, one decoder each.
interface StripeState {
	dec: StripeDecoder | null;
	codec: string | null;
	waitingKey: boolean;
	ts: number;
	/** Decoder timestamp -> frame id. */
	ids: Map<number, number>;
}
/** The display's refresh rate the page measured, once it has. */
let hz: number | undefined;
let hzKnown: () => void = () => {};
const hzWait = new Promise<void>((r) => {
	hzKnown = r;
	setTimeout(r, HZ_WAIT_MS);
});
/** A hello went out on this connection before the display rate was known. */
let hzOwed = false;
let rate = new StreamRate();
let behind = new FellBehind();
let layout: Stripe[] = [];
let width = 0;
const stripes: StripeState[] = [];
const frames = new FrameQueue<Picture>(MAX_PENDING);
let kfRequested = 0;
const times = new Percentiles();
const st = {
	frames: 0,
	bytes: 0,
	decodeMs: 0,
	pipeMs: 0,
	lastReport: performance.now(),
	dropped: 0,
};

function send(s: string) {
	if (ws && ws.readyState === WebSocket.OPEN)
		ws.send(framed ? frameText(s) : s);
}
function post(m: FromWorker, transfer: Transferable[] = []) {
	self.postMessage(m, transfer);
}

function connect() {
	const sock = new WebSocket(url);
	ws = sock;
	conn++;
	rate = new StreamRate();
	behind = new FellBehind();
	hzOwed = false;
	sock.binaryType = 'arraybuffer';
	sock.addEventListener('open', () => {
		post({ t: 'status', s: 'open' });
		send(cmd.audio(wantAudio));
	});
	sock.addEventListener('close', () => {
		post({ t: 'status', s: 'closed' });
		proto = 0;
		videoSent = false;
		epoch = 0;
		ready = false;
		encoder = null;
		resetDecoder();
		setTimeout(connect, 1000);
	});
	sock.addEventListener('message', (e: MessageEvent<string | ArrayBuffer>) => {
		if (!framed) return onMessage(e.data);
		try {
			if (typeof e.data === 'string') throw new TypeError('a text message');
			const m = unframe(e.data);
			onMessage(m.t === 'text' ? m.s : m.b);
		} catch (err) {
			post({ t: 'log', s: `framed socket: ${err}` });
			sock.close(4003, 'not a framed message');
		}
	});
}

function onMessage(data: string | ArrayBuffer) {
	if (typeof data === 'string') return onText(data);
	const b = new Uint8Array(data);
	switch (b[0]) {
		case Bin.Video:
		case Bin.VideoV2:
			onVideo(b);
			break;
		case Bin.Audio:
			onAudio(b);
			break;
		case Bin.Cursor:
			post({ t: 'cursor', buf: data }, [data]);
			break;
		case Bin.ClipImage:
			post({ t: 'clipImage', buf: data }, [data]);
			break;
	}
}

function onText(s: string) {
	let m: ServerMessage;
	try {
		m = JSON.parse(s);
	} catch {
		return;
	}
	if (m.t === 'ping') {
		send(cmd.pong(m.s));
		return;
	}
	if (m.t === 'close') {
		try {
			ws?.close();
		} catch {}
		return;
	}
	if (m.t === 'codec') {
		onCodec(m);
		return;
	}
	if (m.t === 'hello') onHello(m);
	post(m);
}

function setLayout(w: number, h: number, list: Stripe[]) {
	if (!canvas) return;
	if (canvas.width !== w || canvas.height !== h) {
		canvas.width = w;
		canvas.height = h;
	}
	ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
	layout = list;
	width = w;
}

function sendVideo() {
	if (videoSent) return;
	videoSent = true;
	send(cmd.video(wantVideo));
}

function onHello(m: Hello) {
	if (m.protocol !== 2 || !Array.isArray(m.videoOffers)) {
		proto = 1;
		if (typeof VideoDecoder === 'undefined')
			post({ t: 'log', s: 'no video: this browser has no WebCodecs' });
		setLayout(m.w, m.h, m.stripes && m.stripes.length ? m.stripes : [[0, m.h]]);
		resetDecoder();
		sendVideo();
		return;
	}
	// Each hello (the first, and one per new screen size) is answered with what decodes
	// at its sizes; the last picture stays up until the server's next `codec`.
	proto = 2;
	void answer(conn, ++helloSeq, m.offer, m.videoOffers);
}

async function supported(c: VideoDecoderConfig): Promise<boolean> {
	if (typeof VideoDecoder === 'undefined') return false;
	try {
		return !!(await VideoDecoder.isConfigSupported(c)).supported;
	} catch {
		return false;
	}
}

function loadDav1d(): Promise<Dav1d | null> {
	dav1dLoad ??= (async () => {
		try {
			importScripts(new URL('dav1d.js', self.location.href).href);
			dav1d = await self.starfoldDav1d!.load(
				new URL('dav1d.wasm', self.location.href).href
			);
			post({ t: 'log', s: `dav1d-wasm ready (${dav1d.renderer})` });
		} catch (e) {
			post({ t: 'log', s: `dav1d-wasm unavailable: ${e}` });
		}
		return dav1d;
	})();
	return dav1dLoad;
}

async function answer(
	c: number,
	seq: number,
	offer: number | undefined,
	offers: VideoOffer[]
) {
	const r = await probe(offers, {
		supported,
		dav1d: async () => !!(await loadDav1d()),
		dav1dDecodes,
		noHardware,
	});
	await hzWait;
	if (c !== conn || seq !== helloSeq || proto !== 2) return;
	for (const [k, a] of r.accepted) accepted.set(k, a);
	send(msg.hello(offer, r.decode, hz));
	hzOwed = hz === undefined;
	sendVideo();
}

/** A new epoch: everything of the old one is dropped, and once every stripe's
 * decoder configuration checks out the server is told it can send. */
function onCodec(m: CodecMessage) {
	if (proto !== 2 || m.epoch <= epoch) return;
	epoch = m.epoch;
	ready = false;
	encoder = null;
	errors = [];
	resetDecoder();
	if (m.error !== undefined) {
		post({ t: 'log', s: `no video: ${m.error}` });
		post(m);
		return;
	}
	void configure(conn, m);
}

async function plan(m: Extract<CodecMessage, { w: number }>) {
	return Promise.all(
		m.stripes.map(async (s): Promise<StripePlan | null> => {
			const a = accepted.get(configKey(s.config));
			if (a?.implementation === 'dav1d-wasm')
				return dav1d && m.codec === 'av1' && dav1dDecodes(s.config.codec)
					? {
							implementation: 'dav1d-wasm',
							config: decoderConfig(s.config, 'no-preference'),
						}
					: null;
			const hw = a?.hardwareAcceleration ?? 'no-preference';
			const config = decoderConfig(s.config, hw);
			if (noHardware && m.codec === 'av1') return null;
			return (await supported(config))
				? { implementation: 'webcodecs', config, hardwareAcceleration: hw }
				: null;
		})
	);
}

async function configure(c: number, m: Extract<CodecMessage, { w: number }>) {
	const list = await plan(m);
	if (c !== conn || m.epoch !== epoch) return;
	if (!list.every((p) => p !== null)) {
		send(msg.error(m.epoch, 'unsupported'));
		return;
	}
	plans = list as StripePlan[];
	encoder = m.backend ? `${m.backend} (${m.hardware ? 'hw' : 'sw'})` : null;
	setLayout(
		m.w,
		m.h,
		m.stripes.map((s): Stripe => [s.y, s.h])
	);
	ready = true;
	send(msg.ready(m.epoch));
	post(m);
}

function closeDecoders() {
	for (const sd of stripes) sd.dec?.close();
	stripes.length = 0;
}

function resetDecoder() {
	frames.reset();
	closeDecoders();
}

/** Drops everything so far and starts again from the next keyframe. Acks are cumulative,
 * so acking the newest frame keeps the dropped ones from holding up flow control. */
function restart() {
	const id = frames.cut();
	closeDecoders();
	if (id !== null) send(proto === 2 ? msg.ack(epoch, id) : cmd.ack(id));
	requestKeyframe();
}

function requestKeyframe() {
	const now = performance.now();
	if (now - kfRequested > 500) {
		kfRequested = now;
		send(proto === 2 ? msg.keyframe(epoch) : cmd.keyframe);
	}
}

function stripeState(i: number): StripeState {
	return (
		stripes[i] ||
		(stripes[i] = {
			dec: null,
			codec: null,
			waitingKey: true,
			ts: 0,
			ids: new Map(),
		})
	);
}

function onDecodeError(i: number, sd: StripeState, e: number, err: Error) {
	post({ t: 'log', s: `stripe ${i} decoder error: ${err.message}` });
	if (stripes[i] !== sd) return;
	if (proto === 2 && e === epoch && ready) {
		const now = performance.now();
		errors = errors.filter((t) => now - t < 10000);
		errors.push(now);
		if (errors.length >= 3) {
			// It keeps failing here: the server moves on to its next codec.
			ready = false;
			resetDecoder();
			send(msg.error(e, 'decode-error'));
			return;
		}
	}
	restart();
}

function makeDecoder(i: number, p: StripePlan): StripeDecoder {
	const sd = stripeState(i);
	const e = epoch;
	const sink = {
		output: (pic: Picture) => {
			const id = sd.ids.get(pic.timestamp);
			sd.ids.delete(pic.timestamp);
			if (
				stripes[i] !== sd ||
				id === undefined ||
				!frames.add(id, { i, y: layout[i] ? layout[i][0] : 0, pic })
			) {
				pic.close();
				return;
			}
			flush();
		},
		none: (ts: number) => {
			const id = sd.ids.get(ts);
			sd.ids.delete(ts);
			if (stripes[i] !== sd || id === undefined) return;
			frames.skip(id);
			flush();
		},
		error: (err: Error) => onDecodeError(i, sd, e, err),
	};
	if (p.implementation === 'dav1d-wasm') {
		if (!dav1d) throw new Error('dav1d-wasm is not loaded');
		return dav1d.open(sink);
	}
	return new WebCodecsDecoder(p.config, sink);
}

function setSurface(s: 'video' | 'canvas') {
	if (s === surface) return;
	surface = s;
	post({ t: 'surface', s });
}

function present(id: number, parts: Part<Picture>[], t0: number) {
	if (presentToMain && parts.length && parts.every((g) => g.pic.take)) {
		setSurface('video');
		const out: StripeFrame[] = parts.map((g) => ({
			i: g.i,
			y: g.y,
			frame: g.pic.take!(
				width || Infinity,
				layout[g.i] ? layout[g.i][1] : Infinity
			),
		}));
		post(
			{ t: 'frames', parts: out },
			out.map((g) => g.frame)
		);
	} else if (parts.length) {
		setSurface('canvas');
		for (const g of parts) {
			const h = layout[g.i] ? layout[g.i][1] : Infinity;
			if (ctx && !noDraw) g.pic.draw(ctx, g.y, width || Infinity, h);
			g.pic.close();
		}
		if (!painted) {
			painted = true;
			post({ t: 'painted' });
		}
	}
	send(proto === 2 ? msg.ack(epoch, id) : cmd.ack(id));
	const ms = performance.now() - t0;
	st.frames++;
	st.decodeMs += ms;
	times.add(ms);
}

function flush() {
	frames.flush(present);
}

function dropAll() {
	st.dropped += frames.size;
	restart();
}

/** The stripe's decoder configuration for a keyframe: protocol 2's for this epoch, with
 * the codec string the keyframe's own parameter sets give; protocol 1's from its SPS. */
function keyConfig(i: number, data: Uint8Array): StripePlan {
	if (proto !== 2)
		return {
			implementation: 'webcodecs',
			config: {
				codec: avcCodec(data) || 'avc1.640034',
				optimizeForLatency: true,
				hardwareAcceleration: 'no-preference',
			},
		};
	const p = plans[i];
	const family = familyOf(p.config.codec);
	const actual = family && keyframeCodec(family, data);
	return actual && actual !== p.config.codec
		? { ...p, config: { ...p.config, codec: actual } }
		: p;
}

function onVideo(b: Uint8Array) {
	const h = parseVideoHeader(b);
	if (!isCurrent(h, proto, epoch, ready) || (proto === 2 && !plans[h.stripe]))
		return;
	if (proto !== 2 && typeof VideoDecoder === 'undefined') return;
	if (!frames.seen(h.frame)) return;
	const v2 = proto === 2;
	const data = b.subarray(v2 ? VIDEO_V2_HEADER_BYTES : VIDEO_HEADER_BYTES);
	st.bytes += b.length;
	const first = !frames.has(h.frame);
	if (!frames.open(h.frame, h.parts, performance.now())) {
		if (behind.note(performance.now())) send(msg.slow());
		dropAll();
		return;
	}
	if (first) {
		st.pipeMs += h.pipe / 10;
		rate.note(performance.now());
	}
	const sd = stripeState(h.stripe);
	if (sd.waitingKey && !h.key) {
		frames.skip(h.frame);
		st.dropped++;
		requestKeyframe();
		flush();
		return;
	}
	if (h.key) {
		const p = keyConfig(h.stripe, data);
		const codec = `${p.implementation}/${p.config.codec}`;
		if (!sd.dec || codec !== sd.codec) {
			sd.dec?.close();
			sd.dec = null;
			sd.codec = codec;
			sd.ids.clear();
			try {
				sd.dec = makeDecoder(h.stripe, p);
			} catch (e) {
				onDecodeError(h.stripe, sd, epoch, new Error(`configure failed: ${e}`));
				return;
			}
		}
		sd.waitingKey = false;
	}
	const dec = sd.dec!;
	if (dec.queued >= rate.maxQueued()) {
		post({ t: 'log', s: `stripe ${h.stripe} decoder fell behind` });
		if (behind.note(performance.now())) send(msg.slow());
		dropAll();
		return;
	}
	const ts = ++sd.ts;
	sd.ids.set(ts, h.frame);
	try {
		dec.decode(data, h.key, ts);
	} catch (e) {
		post({ t: 'log', s: 'decode threw ' + e });
		restart();
	}
}

function onAudio(b: Uint8Array) {
	const port = audioPort;
	if (!port || typeof AudioDecoder === 'undefined') return;
	if (!audioDecoder) {
		audioDecoder = new AudioDecoder({
			output: (ad) => {
				const n = ad.numberOfFrames;
				const chunk: PcmChunk = {
					l: new Float32Array(n),
					r: new Float32Array(n),
				};
				ad.copyTo(chunk.l, { planeIndex: 0, format: 'f32-planar' });
				if (ad.numberOfChannels > 1)
					ad.copyTo(chunk.r, { planeIndex: 1, format: 'f32-planar' });
				else chunk.r.set(chunk.l);
				ad.close();
				port.postMessage(chunk, [chunk.l.buffer, chunk.r.buffer]);
			},
			error: () => {
				audioDecoder = null;
			},
		});
		audioDecoder.configure({
			codec: 'opus',
			sampleRate: 48000,
			numberOfChannels: 2,
		});
	}
	audioTs += 10000;
	try {
		audioDecoder.decode(
			new EncodedAudioChunk({
				type: 'key',
				timestamp: audioTs,
				data: b.subarray(1),
			})
		);
	} catch {}
}

setInterval(() => {
	const now = performance.now();
	const dt = (now - st.lastReport) / 1000;
	const n = st.frames;
	const { p50, p95 } = times.take();
	const s0 = stripes[0];
	const p0 = proto === 2 ? plans[0] : undefined;
	const stats: Stats = {
		t: 'stats',
		fps: n / dt,
		kbps: (st.bytes * 8) / 1000 / dt,
		decodeMs: n ? st.decodeMs / n : 0,
		decodeP50: p50,
		decodeP95: p95,
		pipeMs: n ? st.pipeMs / n : 0,
		codec: s0?.codec ? s0.codec.slice(s0.codec.indexOf('/') + 1) : null,
		encoder,
		implementation: s0?.dec?.implementation ?? null,
		hw:
			s0?.dec?.implementation === 'webcodecs'
				? (p0?.hardwareAcceleration ?? 'no-preference')
				: null,
		stripes: layout.length,
		dropped: st.dropped,
		queue: frames.size,
	};
	post(stats);
	st.frames = 0;
	st.bytes = 0;
	st.decodeMs = 0;
	st.pipeMs = 0;
	st.lastReport = now;
}, 1000);

self.addEventListener('message', (e: MessageEvent<ToWorker>) => {
	const m = e.data;
	switch (m.t) {
		case 'init':
			canvas = m.canvas;
			url = m.url;
			framed = m.framed === true;
			presentToMain = m.presentToMain;
			ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
			connect();
			break;
		case 'send':
			send(m.d);
			break;
		case 'sendBin':
			if (ws && ws.readyState === WebSocket.OPEN)
				ws.send(framed ? frameBinary(m.d) : m.d);
			break;
		case 'video':
			if (wantVideo !== m.on) {
				wantVideo = m.on;
				if (m.on) {
					resetDecoder();
					// Protocol 2: the server announces a new epoch.
					if (proto === 2) ready = false;
				}
				if (videoSent) send(cmd.video(m.on));
			}
			break;
		case 'hz':
			hz = m.hz;
			hzKnown();
			if (hzOwed) send(msg.hz(m.hz));
			hzOwed = false;
			break;
		case 'audioPort':
			audioPort = m.port;
			wantAudio = true;
			send(cmd.audio(true));
			break;
	}
});
