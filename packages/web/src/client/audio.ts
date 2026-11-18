// Desktop audio: the worker decodes Opus and posts PCM straight to the player worklet
// over a MessageChannel, so audio never waits on the main thread.
import type { App } from './app';

/** What playback follows: the server's hello on this connection, and the connection's state. */
export type AudioCue =
	| { t: 'hello'; audio?: boolean }
	| { t: 'status'; s: string };

export function createAudio(app: App) {
	/** Whether the last hello on the live connection said the server streams audio. */
	let on = false;
	let ac: AudioContext | null = null;
	/** The worklet being set up on `ac`, true once it is wired. */
	let wiring: Promise<boolean> | null = null;

	async function wire(ctx: AudioContext): Promise<boolean> {
		try {
			await ctx.audioWorklet.addModule('audio.js');
			const node = new AudioWorkletNode(ctx, 'pcm-player', {
				outputChannelCount: [2],
			});
			node.connect(ctx.destination);
			const ch = new MessageChannel();
			node.port.postMessage({ port: ch.port2 }, [ch.port2]);
			app.post({ t: 'audioPort', port: ch.port1 }, [ch.port1]);
			return true;
		} catch (err) {
			console.warn('[starfold] audio unavailable', err);
			return false;
		}
	}

	/** Starts (or resumes) playback if the last hello has audio; browsers only let it run
	 * after a user gesture, so a gesture calls this again. */
	async function start() {
		if (!on || typeof AudioDecoder === 'undefined') return;
		if (!ac) {
			ac = new AudioContext({ latencyHint: 'interactive', sampleRate: 48000 });
			wiring = wire(ac);
		}
		const ctx = ac;
		const wired = await wiring;
		if (wired && on && ctx.state !== 'running') ctx.resume().catch(() => {});
	}

	function stop() {
		if (ac && ac.state === 'running') ac.suspend().catch(() => {});
	}

	/** A hello sets playback to what it says (a new connection sends one, and a screen
	 * resize another); a closed connection stops it until the next hello. */
	function follow(m: AudioCue) {
		if (m.t === 'hello') on = !!m.audio;
		else if (m.s === 'closed') on = false;
		else return;
		if (on) void start();
		else stop();
	}
	return { start, follow };
}
