// Just enough of a browser for mount() to run in bun test: elements, window and document with
// listener accounting, a Worker the test drives, ResizeObserver, animation frames, and the
// media track generator the stripe <video>s use. install() puts them on globalThis and returns
// what a test inspects; restore() puts back what was there.

type Listener = EventListenerOrEventListenerObject;

/** An EventTarget that keeps the listeners currently added. */
class Tracked extends EventTarget {
	readonly active = new Set<string>();
	private ids = new WeakMap<object, number>();
	private next = 1;
	private key(type: string, l: Listener | null) {
		if (!l) return `${type}:null`;
		let id = this.ids.get(l);
		if (!id) {
			id = this.next++;
			this.ids.set(l, id);
		}
		return `${type}:${id}`;
	}
	override addEventListener(
		type: string,
		l: Listener | null,
		o?: AddEventListenerOptions | boolean
	) {
		this.active.add(this.key(type, l));
		super.addEventListener(type, l, o);
	}
	override removeEventListener(
		type: string,
		l: Listener | null,
		o?: EventListenerOptions | boolean
	) {
		this.active.delete(this.key(type, l));
		super.removeEventListener(type, l, o);
	}
}

const noop2d = new Proxy({}, { get: () => () => {} });

export class FakeEl extends Tracked {
	style: Record<string, string> = {};
	children: FakeEl[] = [];
	parent: FakeEl | null = null;
	className = '';
	tabIndex = -1;
	value = '';
	srcObject: unknown = null;
	muted = false;
	autoplay = false;
	playsInline = false;
	disablePictureInPicture = false;
	clientWidth = 0;
	clientHeight = 0;
	width = 0;
	height = 0;
	constructor(readonly tagName: string) {
		super();
	}
	append(...els: FakeEl[]) {
		for (const e of els) {
			e.parent = this;
			this.children.push(e);
		}
	}
	appendChild(e: FakeEl) {
		this.append(e);
		return e;
	}
	remove() {
		if (!this.parent) return;
		this.parent.children = this.parent.children.filter((c) => c !== this);
		this.parent = null;
	}
	get isConnected() {
		return this.parent !== null;
	}
	setAttribute() {}
	focus() {}
	setPointerCapture() {}
	play() {
		return Promise.resolve();
	}
	getContext() {
		return noop2d;
	}
	getBoundingClientRect() {
		const w = parseFloat(this.style.width ?? '') || this.clientWidth || 100;
		const h = parseFloat(this.style.height ?? '') || this.clientHeight || 100;
		return { left: 0, top: 0, width: w, height: h, right: w, bottom: h };
	}
	transferControlToOffscreen() {
		return {};
	}
}

export class FakeWorker extends Tracked {
	static all: FakeWorker[] = [];
	posted: { t: string }[] = [];
	terminated = false;
	life: MessagePort | null = null;
	constructor(readonly url: string | URL) {
		super();
		FakeWorker.all.push(this);
	}
	postMessage(m: { t: string; embed?: { life: MessagePort } }) {
		if (this.terminated) return;
		this.posted.push(m);
		if (m.t === 'init' && m.embed) this.life = m.embed.life;
	}
	terminate() {
		this.terminated = true;
		this.life?.close();
	}
	/** What the worker would post on its ordinary channel. */
	emit(data: unknown) {
		this.dispatchEvent(new MessageEvent('message', { data }));
	}
}

export class FakeResizeObserver {
	static all: FakeResizeObserver[] = [];
	observing = 0;
	disconnected = false;
	constructor(readonly cb: () => void) {
		FakeResizeObserver.all.push(this);
	}
	observe() {
		this.observing++;
	}
	disconnect() {
		this.disconnected = true;
	}
}

export class FakeGenerator {
	static all: FakeGenerator[] = [];
	stopped = false;
	writable = { getWriter: () => ({ write: () => Promise.resolve() }) };
	constructor() {
		FakeGenerator.all.push(this);
	}
	stop() {
		this.stopped = true;
	}
}

export interface Env {
	window: Tracked & Record<string, unknown>;
	document: Tracked & Record<string, unknown>;
	frames: Map<number, FrameRequestCallback>;
	vibrated: number;
	reported: unknown[];
	portsClosed: number;
	restore(): void;
}

const KEYS = [
	'window',
	'document',
	'Worker',
	'ResizeObserver',
	'requestAnimationFrame',
	'cancelAnimationFrame',
	'MediaStreamTrackGenerator',
	'MediaStream',
	'reportError',
] as const;

export function install(): Env {
	const g = globalThis as unknown as Record<string, unknown>;
	const saved = new Map(KEYS.map((k) => [k, g[k]]));
	const navSaved = Object.getOwnPropertyDescriptor(navigator, 'vibrate');
	const closeSaved = MessagePort.prototype.close;
	FakeWorker.all = [];
	FakeResizeObserver.all = [];
	FakeGenerator.all = [];
	const frames = new Map<number, FrameRequestCallback>();
	let nextFrame = 1;
	const win = Object.assign(new Tracked(), {
		isSecureContext: true,
		devicePixelRatio: 1,
		innerWidth: 1280,
		innerHeight: 720,
	}) as Env['window'];
	win.top = win;
	const doc = Object.assign(new Tracked(), {
		hidden: false,
		hasFocus: () => true,
		createElement: (tag: string) => new FakeEl(tag),
	}) as Env['document'];
	const env: Env = {
		window: win,
		document: doc,
		frames,
		vibrated: 0,
		reported: [],
		portsClosed: 0,
		restore() {
			for (const [k, v] of saved) g[k] = v;
			if (navSaved) Object.defineProperty(navigator, 'vibrate', navSaved);
			else delete (navigator as unknown as Record<string, unknown>).vibrate;
			MessagePort.prototype.close = closeSaved;
		},
	};
	g.window = win;
	g.document = doc;
	g.Worker = FakeWorker;
	g.ResizeObserver = FakeResizeObserver;
	g.requestAnimationFrame = (cb: FrameRequestCallback) => {
		const id = nextFrame++;
		frames.set(id, cb);
		return id;
	};
	g.cancelAnimationFrame = (id: number) => frames.delete(id);
	g.MediaStreamTrackGenerator = FakeGenerator;
	g.MediaStream = class {
		constructor(readonly tracks: unknown[]) {}
	};
	g.reportError = (e: unknown) => env.reported.push(e);
	Object.defineProperty(navigator, 'vibrate', {
		configurable: true,
		value: () => {
			env.vibrated++;
			return true;
		},
	});
	MessagePort.prototype.close = function (this: MessagePort) {
		env.portsClosed++;
		closeSaved.call(this);
	};
	return env;
}

/** A container element with a size. */
export function container(w = 800, h = 450): FakeEl {
	const c = new FakeEl('div');
	c.clientWidth = w;
	c.clientHeight = h;
	return c;
}
