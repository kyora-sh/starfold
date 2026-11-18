// The embeddable client: `mount(container, options)` shows a starfold desktop inside an element of
// another page. The worker opens the socket to `connect.url` exactly as given (no query string
// added), sends the preamble's messages before anything of starfold's, and never reconnects on
// its own: every close goes to `onClose` with its code and reason, and `connect()` opens again,
// asking the preamble again. v1 keeps the picture, keyboard and pointer input, pasting into the
// desktop, the cursor, read-only display and (opt-in) audio; no toolbar, files, microphone or
// resizing.
import type { App } from '../client/app';
import { blocksInput } from '../client/app';
import { createAudio } from '../client/audio';
import { createClipboard } from '../client/clipboard';
import { createCursor } from '../client/cursor';
import { setupKeyboard } from '../client/keyboard';
import { setupPointer } from '../client/pointer';
import { createVideo } from '../client/video';
import { displayHz } from '../pace';
import { cmd } from '../protocol';
import type { FromWorker, ToWorker } from '../protocol';
import { answerPreamble } from './preamble';
import type { Preamble } from './preamble';

export type { Preamble } from './preamble';

export interface ConnectOptions {
	/** The WebSocket URL, used exactly as given. */
	url: string;
	/** WebSocket subprotocols. */
	protocols?: string | string[];
	/** Text message(s) sent first on every connection; called before every connect. */
	preamble?: Preamble;
}

export type Status = 'connecting' | 'open' | 'closed' | 'preamble-failed';

export interface MountOptions {
	connect: ConnectOptions;
	/** Where worker.js, dav1d.js, dav1d.wasm and audio.js are; default: beside this module. */
	assetBase?: string | URL;
	/** Play the desktop's audio (after the first user gesture). Default off. */
	audio?: boolean;
	/** Every close of the socket, with the close frame's code and reason. Nothing reconnects. */
	onClose?(code: number, reason: string): void;
	/** `preamble-failed` carries the preamble's error; nothing opened. */
	onStatus?(status: Status, detail?: unknown): void;
}

export interface Handle {
	/** Connects, calling the preamble first. Ignored while connected or connecting. */
	connect(): void;
	/** Closes the socket, stops the worker and removes what `mount` added. */
	unmount(): void;
}

const css = (e: HTMLElement, s: Partial<CSSStyleDeclaration>) => Object.assign(e.style, s);

/** Shows a starfold desktop in `container` and connects once; see `Handle` for the rest. */
export function mount(container: HTMLElement, options: MountOptions): Handle {
	if (!window.isSecureContext)
		throw new Error('starfold: browsers only decode video on secure pages (HTTPS or localhost)');
	const base = new URL(options.assetBase ?? '.', import.meta.url);
	const surface = document.createElement('div');
	surface.tabIndex = 0;
	css(surface, { position: 'relative', outline: 'none', touchAction: 'none' });
	const canvas = document.createElement('canvas');
	css(canvas, {
		position: 'absolute',
		inset: '0',
		width: '100%',
		height: '100%',
		display: 'block',
		pointerEvents: 'none',
	});
	if (!canvas.transferControlToOffscreen)
		throw new Error('starfold: this browser lacks OffscreenCanvas support');
	surface.append(canvas);
	// Receives keyboard, IME and paste input; on-screen so mobile browsers open a keyboard.
	const kbd = document.createElement('textarea');
	kbd.setAttribute('aria-hidden', 'true');
	css(kbd, {
		position: 'fixed',
		left: '0',
		bottom: '0',
		width: '1px',
		height: '1px',
		opacity: '0.01',
		border: '0',
		padding: '0',
		resize: 'none',
		pointerEvents: 'none',
		fontSize: '16px',
	});
	container.append(surface, kbd);

	const worker = new Worker(new URL('worker.js', base));
	const post = (m: ToWorker, transfer: Transferable[] = []) => worker.postMessage(m, transfer);
	const app: App = {
		worker,
		post,
		send: (d) => {
			if (!blocksInput(app, d)) post({ t: 'send', d });
		},
		sendBin: (type, bytes) => {
			if (app.readOnly) return;
			const msg = new Uint8Array(bytes.byteLength + 1);
			msg[0] = type;
			msg.set(bytes, 1);
			post({ t: 'sendBin', d: msg.buffer }, [msg.buffer]);
		},
		surface,
		kbd,
		tools: document.createElement('div'),
		isMac: /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent),
		remoteMac: false,
		readOnly: false,
		inIframe: window !== window.top,
		desk: { w: 0, h: 0 },
		view: { scale: 1 },
	};

	const stamps: number[] = [];
	const tick = (t: number) => {
		stamps.push(t);
		if (t - stamps[0] < 500) return requestAnimationFrame(tick);
		const hz = displayHz(stamps);
		if (hz) post({ t: 'hz', hz });
	};
	requestAnimationFrame(tick);

	const cursor = createCursor(app);
	const useVideo = typeof MediaStreamTrackGenerator !== 'undefined';
	const video = createVideo(app, canvas, useVideo, cursor.apply, () => {}, () => ({
		w: container.clientWidth,
		h: container.clientHeight,
	}));
	const clipboard = createClipboard(app);
	const audio = options.audio ? createAudio(app, new URL('audio.js', base)) : null;
	const onInteract = () => {
		void audio?.start();
		clipboard.flush();
	};
	setupPointer(app, onInteract);
	setupKeyboard(app, clipboard, onInteract);

	const offscreen = canvas.transferControlToOffscreen();
	const protocols = options.connect.protocols;
	post(
		{
			t: 'init',
			canvas: offscreen,
			url: options.connect.url,
			presentToMain: useVideo,
			embed: {
				protocols: protocols === undefined ? undefined : ([] as string[]).concat(protocols),
			},
		},
		[offscreen]
	);

	let videoOn: boolean | null = null;
	const updateVisibility = (force = false) => {
		const on = !document.hidden && container.clientWidth > 0 && container.clientHeight > 0;
		if (on !== videoOn || force) {
			videoOn = on;
			post({ t: 'video', on });
		}
	};
	const onVisibility = () => {
		updateVisibility();
		if (document.hidden) app.send(cmd.releaseAll);
	};
	document.addEventListener('visibilitychange', onVisibility);
	const resized = new ResizeObserver(() => {
		video.layout();
		updateVisibility();
	});
	resized.observe(container);
	updateVisibility();

	const status = (s: Status, detail?: unknown) => options.onStatus?.(s, detail);
	let mounted = true;
	worker.addEventListener('message', (e: MessageEvent<FromWorker>) => {
		const m = e.data;
		switch (m.t) {
			case 'frames':
				video.present(m.parts);
				break;
			case 'surface':
				video.setSurface(m.s);
				break;
			case 'hello':
				app.desk.w = m.w;
				app.desk.h = m.h;
				app.remoteMac = m.os === 'macos';
				app.readOnly = !!m.readOnly;
				video.build(m.stripes && m.stripes.length ? m.stripes : [[0, m.h]]);
				video.layout();
				break;
			case 'codec':
				if (m.error === undefined) {
					app.desk.w = m.w;
					app.desk.h = m.h;
					video.build(m.stripes.map((s): [number, number] => [s.y, s.h]));
					video.layout();
				}
				break;
			case 'preamble':
				status('connecting');
				void answerPreamble(options.connect.preamble, post, (err) =>
					status('preamble-failed', err)
				);
				break;
			case 'status':
				if (m.s === 'open') {
					updateVisibility(true);
					status('open');
				} else {
					status('closed');
					options.onClose?.(m.code, m.reason);
				}
				break;
			case 'cursor':
				cursor.set(m.buf);
				break;
			case 'log':
				console.warn('[starfold]', m.s);
				break;
		}
	});

	const handle: Handle = {
		connect() {
			if (mounted) post({ t: 'connect' });
		},
		unmount() {
			if (!mounted) return;
			mounted = false;
			post({ t: 'disconnect' });
			resized.disconnect();
			document.removeEventListener('visibilitychange', onVisibility);
			// Let the worker send its close frame before it stops.
			setTimeout(() => worker.terminate(), 250);
			surface.remove();
			kbd.remove();
		},
	};
	handle.connect();
	return handle;
}
