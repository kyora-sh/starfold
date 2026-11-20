// The embeddable client: `mount(container, options)` shows a starfold desktop inside an element of
// another page. The worker opens the socket to `connect.url` exactly as given, sends the
// preamble's messages before anything of starfold's, and reconnects only with the opt-in
// `reconnect` option (reconnect.ts). The
// connection's lifecycle runs over a port of its own (life.ts, attempts.ts), apart from the
// server's data; every embedder callback goes through one queued dispatcher (dispatch.ts); and
// everything mount() sets up is undone by one disposer list (disposers.ts) on unmount(), which
// stops the worker at once. v1 keeps the picture, keyboard and pointer input, pasting into the
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
import { PageLife } from './attempts';
import { Disposers } from './disposers';
import { Reconnector, pageEnv } from './reconnect';
import { onStream } from './stream';
import {
	KBD,
	SCREEN,
	STRIPE,
	SURFACE,
	applyStyle,
	assetBaseUrl,
} from './styles';
import type {
	ConnectFailure,
	Preamble,
	ReconnectEvent,
	ReconnectOptions,
	Status,
} from './types.js';

export type {
	ConnectFailure,
	Preamble,
	ReconnectEvent,
	ReconnectOptions,
	ReconnectWhy,
	Status,
} from './types.js';
export {
	NO_COMMON_CODEC,
	PROTOCOL_ERROR,
	UNMOUNTED,
	WORKER_FAILED,
} from './types.js';

export interface ConnectOptions {
	/** The WebSocket URL, used exactly as given (no query string is added). */
	url: string;
	/** WebSocket subprotocols. */
	protocols?: string | string[];
	/**
	 * Text message(s) sent first on every connection, before anything of starfold's; called
	 * before every connect. What it gives is copied, posted to the worker, sent once the socket
	 * opens and then dropped; it is never logged or passed to a callback.
	 */
	preamble?: Preamble;
}

export interface MountOptions {
	connect: ConnectOptions;
	/**
	 * The directory with worker.js, dav1d.js, dav1d.wasm and audio.js (a trailing slash is
	 * added if missing), resolved against this module's URL; default: this module's directory.
	 * It must be on the page's own origin: browsers start a worker only from a same-origin
	 * script, and a worker that fails to load ends the attempt with `connect-failed`.
	 */
	assetBase?: string | URL;
	/** Play the desktop's audio (after the first user gesture). Default off. */
	audio?: boolean;
	/**
	 * Called once for every connection whose socket was asked for, with the close frame's code
	 * and reason as the server sent them (a reason is the server's text: a server that echoes a
	 * ticket there leaks it itself); `PROTOCOL_ERROR` (4000) when the client could not handle
	 * what the server sent; `NO_COMMON_CODEC` (4001) when the server found no codec this browser
	 * decodes, and the client closed the socket; `WORKER_FAILED` (4002) when the worker failed
	 * at run time, and the client stopped it (no connect follows); `UNMOUNTED` (1001) when
	 * `unmount()` ended it. `unmount()` stops the worker at once, so the server sees the socket
	 * drop without a close frame, and a close the server sent at the same moment is reported as
	 * `UNMOUNTED`.
	 */
	onClose?(code: number, reason: string): void;
	/**
	 * `preamble-failed` and `connect-failed` carry a fixed `ConnectFailure`, never the
	 * preamble's own error. Each connect ends in exactly one of `onClose`, `preamble-failed`
	 * or `connect-failed`, unless a newer `connect()` or `unmount()` came first while it was
	 * still waiting for its preamble. After `onClose`, `onStatus('closed')` follows, except
	 * after `unmount()`; with `reconnect`, `reconnecting` follows instead while another attempt
	 * is coming (it then reports `connecting` and `open` as any connect), and `gave-up` then
	 * `closed` when none is. A reconnect attempt that ends in `preamble-failed` or
	 * `connect-failed` is followed by `gave-up` and `closed`. With `reconnect`, a worker that
	 * fails after a socket was asked for, or while waiting to reconnect, also ends in `gave-up`
	 * and `closed`; one that fails while the host's own connect() waits for its preamble ends in
	 * `connect-failed` alone, as without it.
	 */
	onStatus?(status: Status, detail?: ConnectFailure): void;
	/**
	 * Reconnect after a close worth another attempt, as the host would: the same `connect()`,
	 * the preamble called again for each attempt. `true` takes the defaults; an object sets the
	 * limits (`ReconnectOptions`). Default off: every close ends the connection, as before. Which
	 * close codes reconnect is in the README's embedding section. A reconnect is a new viewer to
	 * the server (a new negotiation, then a keyframe).
	 */
	reconnect?: true | ReconnectOptions;
	/**
	 * With `reconnect`: `reconnecting` (the attempt number, its delay and the code of the close
	 * that caused it), `reconnected` (an attempt reached open, with the code of the close it
	 * followed) and `gave-up` (why, and the last close's code). Each comes after the `onStatus` of
	 * the same name (`open` for `reconnected`). No close reason is in them: onClose has it.
	 */
	onReconnect?(e: ReconnectEvent): void;
}

export interface Handle {
	/**
	 * Connects, calling the preamble first. While the preamble of an earlier connect is
	 * pending, starts over (the earlier answer is dropped); while waiting to reconnect,
	 * connects now and the reconnect count starts over; while a socket is open or being made,
	 * does nothing; after `unmount()`, does nothing.
	 */
	connect(): void;
	/**
	 * Stops the worker at once (its socket drops), removes everything `mount` added, and
	 * then, if a socket had been asked for, reports `onClose(UNMOUNTED)`. After it returns no
	 * socket is opened, no message is sent, and no callback other than that one runs.
	 */
	unmount(): void;
}

/**
 * Shows a starfold desktop in `container` and connects once; see `Handle` for the rest.
 * Callbacks always run after the call that caused them has returned, from a queued task; a
 * throwing callback is passed to `reportError` and changes nothing.
 */
export function mount(container: HTMLElement, options: MountOptions): Handle {
	if (!window.isSecureContext)
		throw new Error(
			'starfold: browsers only decode video on secure pages (HTTPS or localhost)'
		);
	const base = assetBaseUrl(options.assetBase, import.meta.url);
	// Before anything exists: limits that are not numbers throw here.
	const reconnect = options.reconnect
		? new Reconnector(options.reconnect, pageEnv())
		: undefined;
	const canvas = document.createElement('canvas');
	if (!canvas.transferControlToOffscreen)
		throw new Error('starfold: this browser lacks OffscreenCanvas support');
	// Before anything is added to the page: a worker that can't be created leaves nothing behind.
	const worker = new Worker(new URL('worker.js', base));

	/** Set first thing in unmount(); every later continuation checks it before acting. */
	let disposed = false;
	const disposers = new Disposers();
	disposers.add('worker', () => worker.terminate());
	const lifeChannel = new MessageChannel();
	disposers.add('life port', () => lifeChannel.port1.close());

	const surface = document.createElement('div');
	surface.tabIndex = 0;
	applyStyle(surface, SURFACE);
	applyStyle(canvas, SCREEN);
	surface.append(canvas);
	// Receives keyboard, IME and paste input; on-screen so mobile browsers open a keyboard.
	const kbd = document.createElement('textarea');
	kbd.setAttribute('aria-hidden', 'true');
	applyStyle(kbd, KBD);
	container.append(surface, kbd);
	disposers.add('elements', () => {
		surface.remove();
		kbd.remove();
	});

	const post = (m: ToWorker, transfer: Transferable[] = []) => {
		if (!disposed) worker.postMessage(m, transfer);
	};
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
	let tickFrame = 0;
	const tick = (t: number) => {
		if (disposed) return;
		stamps.push(t);
		if (t - stamps[0] < 500) {
			tickFrame = requestAnimationFrame(tick);
			return;
		}
		tickFrame = 0;
		const hz = displayHz(stamps);
		if (hz) post({ t: 'hz', hz });
	};
	tickFrame = requestAnimationFrame(tick);
	disposers.add('refresh-rate frame', () => {
		if (tickFrame) cancelAnimationFrame(tickFrame);
	});

	const cursor = createCursor(app);
	const useVideo = typeof MediaStreamTrackGenerator !== 'undefined';
	const video = createVideo(
		app,
		canvas,
		useVideo,
		cursor.apply,
		() => {},
		() => ({
			w: container.clientWidth,
			h: container.clientHeight,
		}),
		STRIPE,
		true
	);
	disposers.add('video', () => video.dispose());
	const clipboard = createClipboard(app);
	disposers.add('clipboard', () => clipboard.dispose());
	const audio = options.audio
		? createAudio(app, new URL('audio.js', base))
		: null;
	if (audio) disposers.add('audio', () => audio.close());
	const onInteract = () => {
		void audio?.start();
		clipboard.flush();
	};
	const pointer = setupPointer(app, onInteract, () =>
		kbd.focus({ preventScroll: true })
	);
	disposers.add('pointer', () => pointer.dispose());
	const keyboard = setupKeyboard(app, clipboard, onInteract, true);
	disposers.add('keyboard', () => keyboard.dispose());

	let videoOn: boolean | null = null;
	const updateVisibility = (force = false) => {
		const on =
			!document.hidden &&
			container.clientWidth > 0 &&
			container.clientHeight > 0;
		if (on !== videoOn || force) {
			videoOn = on;
			post({ t: 'video', on });
		}
	};
	const onVisibility = () => {
		if (disposed) return;
		updateVisibility();
		if (document.hidden) app.send(cmd.releaseAll);
	};
	document.addEventListener('visibilitychange', onVisibility);
	disposers.add('visibilitychange', () =>
		document.removeEventListener('visibilitychange', onVisibility)
	);
	const resized = new ResizeObserver(() => {
		if (disposed) return;
		video.layout();
		updateVisibility();
	});
	resized.observe(container);
	disposers.add('resize observer', () => resized.disconnect());

	const life = new PageLife(lifeChannel.port1, {
		preamble: options.connect.preamble,
		onClose: options.onClose,
		onStatus: options.onStatus,
		onReconnect: options.onReconnect,
		reconnect,
		opened: () => updateVisibility(true),
	});
	const onWorkerError = (e: Event) => {
		if (disposed) return;
		e.preventDefault();
		worker.terminate();
		life.workerFailed();
	};
	worker.addEventListener('error', onWorkerError);
	disposers.add('worker error listener', () =>
		worker.removeEventListener('error', onWorkerError)
	);
	const onWorkerMessage = (e: MessageEvent<FromWorker>) => {
		if (disposed) return;
		onStream(e.data, {
			frames: (m) => video.present(m.parts),
			surface: (m) => video.setSurface(m.s),
			hello: (m) => {
				app.desk.w = m.w;
				app.desk.h = m.h;
				app.remoteMac = m.os === 'macos';
				app.readOnly = !!m.readOnly;
				video.build(m.stripes && m.stripes.length ? m.stripes : [[0, m.h]]);
				video.layout();
			},
			codec: (m) => {
				if (m.error !== undefined) return;
				app.desk.w = m.w;
				app.desk.h = m.h;
				video.build(m.stripes.map((s): [number, number] => [s.y, s.h]));
				video.layout();
			},
			cursor: (m) => cursor.set(m.buf),
		});
	};
	worker.addEventListener('message', onWorkerMessage);
	disposers.add('worker message listener', () =>
		worker.removeEventListener('message', onWorkerMessage)
	);

	const offscreen = canvas.transferControlToOffscreen();
	const protocols = options.connect.protocols;
	post(
		{
			t: 'init',
			canvas: offscreen,
			url: options.connect.url,
			presentToMain: useVideo,
			embed: {
				protocols:
					protocols === undefined
						? undefined
						: ([] as string[]).concat(protocols),
				life: lifeChannel.port2,
			},
		},
		[offscreen, lifeChannel.port2]
	);
	updateVisibility();

	const handle: Handle = {
		connect() {
			if (!disposed) life.connect();
		},
		unmount() {
			if (disposed) return;
			disposed = true;
			// The worker goes first: once terminated it processes nothing more, so a preamble
			// reply already posted never opens a socket. Then the lifecycle ends, which drops
			// every queued callback but the unmounted close.
			try {
				disposers.run();
			} finally {
				life.end();
			}
		},
	};
	handle.connect();
	return handle;
}
