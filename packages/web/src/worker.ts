// Owns the WebSocket, decodes the video with WebCodecs and paints straight onto an
// OffscreenCanvas the moment each frame is decoded (no rAF queueing), or hands the
// frames to the page when it presents them through <video>.
//
// A protocol 2 server offers codecs in its hello: the worker tests each offered
// decoder configuration, tells the server which it decodes, and configures its
// decoders from the server's `codec` message of each epoch. An older server gets
// protocol 1: H.264, configured from each keyframe's SPS.
import {
	Bin,
	cmd,
	msg,
	parseVideoHeader,
	VIDEO_HEADER_BYTES,
	VIDEO_V2_HEADER_BYTES,
} from './protocol';
import type {
	CodecMessage,
	DecodeEntry,
	DecoderConfig,
	FromWorker,
	Hello,
	PcmChunk,
	ServerMessage,
	Stats,
	Stripe,
	StripeFrame,
	ToWorker,
	VideoOffer,
} from './protocol';

declare const self: DedicatedWorkerGlobalScope;

const noDraw = /nodraw/.test(self.location.search);
let presentToMain = false;
let ws: WebSocket | null = null;
let url = '';
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
/** Protocol 2: each stripe's decoder configuration in this epoch. */
let configs: VideoDecoderConfig[] = [];
/** Protocol 2: the server's encoder in this epoch. */
let encoder: string | null = null;
/** The acceleration preference each tested configuration was accepted with. */
const accepted = new Map<string, HardwareAcceleration>();
/** Decoder errors in this epoch; a codec that keeps failing is reported. */
let errors: number[] = [];

// The screen arrives as independent H.264 stripes, one decoder each. A frame is a
// set of stripe updates sharing an id; frames are presented whole and in order.
interface StripeState {
	dec: VideoDecoder | null;
	codec: string | null;
	waitingKey: boolean;
	ts: number;
	/** Decoder timestamp -> frame id. */
	ids: Map<number, number>;
}
interface PendingFrame {
	need: number;
	got: StripeFrame[];
	t0: number;
}
let layout: Stripe[] = [];
let width = 0;
const stripes: StripeState[] = [];
const frames = new Map<number, PendingFrame>();
/** Frame ids awaiting presentation, oldest first. */
const order: number[] = [];
let kfRequested = 0;
const st = {
	frames: 0,
	bytes: 0,
	decodeMs: 0,
	pipeMs: 0,
	lastReport: performance.now(),
	dropped: 0,
};

function send(s: string) {
	if (ws && ws.readyState === WebSocket.OPEN) ws.send(s);
}
function post(m: FromWorker, transfer: Transferable[] = []) {
	self.postMessage(m, transfer);
}

function connect() {
	const sock = new WebSocket(url);
	ws = sock;
	conn++;
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
		if (typeof e.data === 'string') return onText(e.data);
		const b = new Uint8Array(e.data);
		switch (b[0]) {
			case Bin.Video:
			case Bin.VideoV2:
				onVideo(b);
				break;
			case Bin.Audio:
				onAudio(b);
				break;
			case Bin.Cursor:
				post({ t: 'cursor', buf: e.data }, [e.data]);
				break;
			case Bin.ClipImage:
				post({ t: 'clipImage', buf: e.data }, [e.data]);
				break;
		}
	});
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

async function answer(
	c: number,
	seq: number,
	offer: number | undefined,
	offers: VideoOffer[]
) {
	const decode = await probe(offers);
	if (c !== conn || seq !== helloSeq || proto !== 2) return;
	send(msg.hello(offer, decode));
	sendVideo();
}

const configKey = (c: DecoderConfig) =>
	`${c.codec}/${c.codedWidth}x${c.codedHeight}`;

function decoderConfig(
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

async function supported(c: VideoDecoderConfig): Promise<boolean> {
	try {
		return !!(await VideoDecoder.isConfigSupported(c)).supported;
	} catch {
		return false;
	}
}

/** Every offered configuration this browser decodes, hardware preferred. */
async function probe(offers: VideoOffer[]): Promise<DecodeEntry[]> {
	const all = new Map<string, DecoderConfig>();
	for (const o of offers)
		for (const s of o.stripes) all.set(configKey(s.config), s.config);
	const found = await Promise.all([...all].map(([k, c]) => test(k, c)));
	return found.filter((e): e is DecodeEntry => e !== null);
}

async function test(k: string, c: DecoderConfig): Promise<DecodeEntry | null> {
	const hw: HardwareAcceleration | null = (await supported(
		decoderConfig(c, 'prefer-hardware')
	))
		? 'prefer-hardware'
		: (await supported(decoderConfig(c, 'no-preference')))
			? 'no-preference'
			: null;
	if (!hw) return null;
	accepted.set(k, hw);
	return { ...c, implementation: 'webcodecs', hardwareAcceleration: hw };
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
		return;
	}
	void configure(conn, m);
}

async function configure(c: number, m: Extract<CodecMessage, { w: number }>) {
	const list = m.stripes.map((s) =>
		decoderConfig(
			s.config,
			accepted.get(configKey(s.config)) ?? 'no-preference'
		)
	);
	const ok = await Promise.all(list.map(supported));
	if (c !== conn || m.epoch !== epoch) return;
	if (!ok.every(Boolean)) {
		send(msg.error(m.epoch, 'unsupported'));
		return;
	}
	configs = list;
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

function resetDecoder() {
	for (const sd of stripes)
		if (sd.dec && sd.dec.state !== 'closed') {
			try {
				sd.dec.close();
			} catch {}
		}
	stripes.length = 0;
	for (const f of frames.values()) for (const g of f.got) g.frame.close();
	frames.clear();
	order.length = 0;
}

function requestKeyframe() {
	const now = performance.now();
	if (now - kfRequested > 500) {
		kfRequested = now;
		send(proto === 2 ? msg.keyframe(epoch) : cmd.keyframe);
	}
}

const hex2 = (x: number) => x.toString(16).padStart(2, '0');

/** avc1.PPCCLL from the first SPS in an Annex-B keyframe. */
function codecFromSps(d: Uint8Array): string | null {
	for (let i = 0; i + 6 < d.length; i++) {
		if (
			d[i] === 0 &&
			d[i + 1] === 0 &&
			d[i + 2] === 1 &&
			(d[i + 3] & 0x1f) === 7
		) {
			return 'avc1.' + hex2(d[i + 4]) + hex2(d[i + 5]) + hex2(d[i + 6]);
		}
	}
	return null;
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

/** Encoders may code a stripe larger than it is (odd or tiny sizes); crop to it. */
function fitStripe(frame: VideoFrame, i: number): VideoFrame {
	const w = Math.min(
		width || frame.displayWidth,
		frame.visibleRect?.width ?? 0
	);
	const h = Math.min(
		layout[i] ? layout[i][1] : frame.displayHeight,
		frame.visibleRect?.height ?? 0
	);
	if (
		!frame.visibleRect ||
		(w === frame.displayWidth && h === frame.displayHeight)
	)
		return frame;
	try {
		const { x, y } = frame.visibleRect;
		const c = new VideoFrame(frame, {
			visibleRect: { x, y, width: w, height: h },
		});
		frame.close();
		return c;
	} catch {
		return frame;
	}
}

function makeDecoder(i: number, config: VideoDecoderConfig): VideoDecoder {
	const sd = stripeState(i);
	const e = epoch;
	const dec = new VideoDecoder({
		output: (decoded) => {
			if (stripes[i] !== sd) {
				decoded.close();
				return;
			}
			const frame = fitStripe(decoded, i);
			const id = sd.ids.get(frame.timestamp);
			sd.ids.delete(frame.timestamp);
			const f = id === undefined ? undefined : frames.get(id);
			if (!f) {
				frame.close();
				return;
			}
			f.got.push({ i, y: layout[i] ? layout[i][0] : 0, frame });
			flush();
		},
		error: (err) => {
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
			resetDecoder();
			requestKeyframe();
		},
	});
	dec.configure(config);
	return dec;
}

/** Present every complete frame at the head of the queue. */
function flush() {
	while (order.length) {
		const id = order[0];
		const f = frames.get(id);
		if (f && f.got.length < f.need) return;
		order.shift();
		frames.delete(id);
		if (!f) continue;
		if (f.got.length) {
			if (presentToMain)
				post(
					{ t: 'frames', parts: f.got },
					f.got.map((g) => g.frame)
				);
			else
				for (const g of f.got) {
					if (ctx && !noDraw) ctx.drawImage(g.frame, 0, g.y);
					g.frame.close();
				}
		}
		send(proto === 2 ? msg.ack(epoch, id) : cmd.ack(id));
		st.frames++;
		st.decodeMs += performance.now() - f.t0;
	}
}

function onVideo(b: Uint8Array) {
	const h = parseVideoHeader(b);
	const v2 = h.epoch !== undefined;
	// Another protocol's, another epoch's, or before the decoders are set up.
	if (v2 !== (proto === 2)) return;
	if (v2 && (!ready || h.epoch !== epoch || !configs[h.stripe])) return;
	const data = b.subarray(v2 ? VIDEO_V2_HEADER_BYTES : VIDEO_HEADER_BYTES);
	st.bytes += b.length;
	let f = frames.get(h.frame);
	if (!f) {
		f = { need: h.parts, got: [], t0: performance.now() };
		frames.set(h.frame, f);
		order.push(h.frame);
		st.pipeMs += h.pipe / 10;
	}
	const sd = stripeState(h.stripe);
	if (sd.waitingKey && !h.key) {
		f.need--;
		st.dropped++;
		requestKeyframe();
		flush();
		return;
	}
	if (h.key) {
		const config: VideoDecoderConfig = v2
			? configs[h.stripe]
			: {
					codec: codecFromSps(data) || 'avc1.640034',
					optimizeForLatency: true,
					hardwareAcceleration: 'no-preference',
				};
		const codec = config.codec;
		if (!sd.dec || sd.dec.state === 'closed' || codec !== sd.codec) {
			if (sd.dec && sd.dec.state !== 'closed') {
				try {
					sd.dec.close();
				} catch {}
			}
			sd.codec = codec;
			sd.ids.clear();
			try {
				sd.dec = makeDecoder(h.stripe, config);
			} catch (e) {
				post({ t: 'log', s: 'configure failed ' + e });
				f.need--;
				flush();
				return;
			}
		}
		sd.waitingKey = false;
	}
	const ts = ++sd.ts;
	sd.ids.set(ts, h.frame);
	try {
		sd.dec!.decode(
			new EncodedVideoChunk({
				type: h.key ? 'key' : 'delta',
				timestamp: ts,
				data,
			})
		);
	} catch (e) {
		post({ t: 'log', s: 'decode threw ' + e });
		resetDecoder();
		requestKeyframe();
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
	const stats: Stats = {
		t: 'stats',
		fps: n / dt,
		kbps: (st.bytes * 8) / 1000 / dt,
		decodeMs: n ? st.decodeMs / n : 0,
		pipeMs: n ? st.pipeMs / n : 0,
		codec: stripes[0]?.codec ?? null,
		encoder,
		stripes: layout.length,
		dropped: st.dropped,
		queue: order.length,
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
			presentToMain = m.presentToMain;
			ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
			connect();
			break;
		case 'send':
			send(m.d);
			break;
		case 'sendBin':
			if (ws && ws.readyState === WebSocket.OPEN) ws.send(m.d);
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
		case 'audioPort':
			audioPort = m.port;
			wantAudio = true;
			send(cmd.audio(true));
			break;
	}
});
