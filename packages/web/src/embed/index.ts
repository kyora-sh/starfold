// The embeddable client: `mount(container, options)` shows a starfold desktop inside an element of
// another page. The worker opens the socket to `connect.url` exactly as given, sends the
// preamble's messages before anything of starfold's, and never reconnects on its own. The
// connection's lifecycle runs over a port of its own (life.ts, attempts.ts), apart from the
// server's data. v1 keeps the picture, keyboard and pointer input, pasting into the desktop, the
// cursor, read-only display and (opt-in) audio; no toolbar, files, microphone or resizing.
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
import { onStream } from './stream';
import {
	KBD,
	SCREEN,
	STRIPE,
	SURFACE,
	applyStyle,
	assetBaseUrl,
} from './styles';
import type { ConnectFailure, Preamble, Status } from './types.js';

export type { ConnectFailure, Preamble, Status } from './types.js';
export { UNMOUNTED } from './types.js';

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
	 * added if missing); default: this module's directory. It must be on the page's own origin:
	 * browsers start a worker only from a same-origin script.
	 */
	assetBase?: string | URL;
	/** Play the desktop's audio (after the first user gesture). Default off. */
	audio?: boolean;
	/**
	 * Called exactly once for every connection whose socket was asked for, with the close
	 * frame's code and reason as the server sent them (a reason is the server's text: a server
	 * that echoes a ticket there leaks it itself). After `unmount()` it is called at once with
	 * `UNMOUNTED` (1001, "unmounted"), and nothing about that connection follows.
	 * Called before `onStatus('closed')`; if it throws, `onStatus('closed')` still runs.
	 */
	onClose?(code: number, reason: string): void;
	/**
	 * `preamble-failed` and `connect-failed` carry a fixed `ConnectFailure`, never the
	 * preamble's own error. Each connect ends in exactly one of `onClose`, `preamble-failed`
	 * or `connect-failed`, unless a newer `connect()` or `unmount()` came first while it was
	 * still waiting for its preamble.
	 */
	onStatus?(status: Status, detail?: ConnectFailure): void;
}

export interface Handle {
	/**
	 * Connects, calling the preamble first. While the preamble of an earlier connect is
	 * pending, starts over (the earlier answer is dropped); while a socket is open or being
	 * made, does nothing; after `unmount()`, does nothing.
	 */
	connect(): void;
	/** Reports the close (see `onClose`), closes the socket, stops the worker and removes what `mount` added. */
	unmount(): void;
}

/** Shows a starfold desktop in `container` and connects once; see `Handle` for the rest. */
export function mount(container: HTMLElement, options: MountOptions): Handle {
	if (!window.isSecureContext)
		throw new Error(
			'starfold: browsers only decode video on secure pages (HTTPS or localhost)'
		);
	const base = assetBaseUrl(options.assetBase, import.meta.url);
	const surface = document.createElement('div');
	surface.tabIndex = 0;
	applyStyle(surface, SURFACE);
	const canvas = document.createElement('canvas');
	applyStyle(canvas, SCREEN);
	if (!canvas.transferControlToOffscreen)
		throw new Error('starfold: this browser lacks OffscreenCanvas support');
	surface.append(canvas);
	// Receives keyboard, IME and paste input; on-screen so mobile browsers open a keyboard.
	const kbd = document.createElement('textarea');
	kbd.setAttribute('aria-hidden', 'true');
	applyStyle(kbd, KBD);
	container.append(surface, kbd);

	const worker = new Worker(new URL('worker.js', base));
	const post = (m: ToWorker, transfer: Transferable[] = []) =>
		worker.postMessage(m, transfer);
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
		STRIPE
	);
	const clipboard = createClipboard(app);
	const audio = options.audio
		? createAudio(app, new URL('audio.js', base))
		: null;
	const onInteract = () => {
		void audio?.start();
		clipboard.flush();
	};
	setupPointer(app, onInteract);
	const keyboard = setupKeyboard(app, clipboard, onInteract);

	const offscreen = canvas.transferControlToOffscreen();
	const lifeChannel = new MessageChannel();
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

	const life = new PageLife(lifeChannel.port1, {
		preamble: options.connect.preamble,
		onClose: options.onClose,
		onStatus: (s, detail) => {
			if (s === 'open') updateVisibility(true);
			options.onStatus?.(s, detail);
		},
	});

	worker.addEventListener('message', (e: MessageEvent<FromWorker>) =>
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
		})
	);

	let mounted = true;
	const handle: Handle = {
		connect() {
			life.connect();
		},
		unmount() {
			if (!mounted) return;
			mounted = false;
			resized.disconnect();
			document.removeEventListener('visibilitychange', onVisibility);
			keyboard.dispose();
			clipboard.dispose();
			audio?.close();
			surface.remove();
			kbd.remove();
			// Reports the close itself; the worker gets a moment to send its close frame.
			try {
				life.unmount();
			} finally {
				setTimeout(() => worker.terminate(), 250);
			}
		},
	};
	handle.connect();
	return handle;
}
