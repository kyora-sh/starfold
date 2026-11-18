// Embedded: the page's side of the connection lifecycle (life.ts). Every connect() is an
// attempt with an id; the page answers the worker's preamble request only for the current
// attempt and only while mounted, and decides on its own what the embedder hears:
// - each attempt ends in exactly one of: onClose (a socket was asked for), preamble-failed,
//   or connect-failed; one superseded by a newer connect() or cancelled by unmount() before
//   its socket was asked for ends silently;
// - unmount() reports onClose(UNMOUNTED) itself for a socket it was waiting on, at once, and
//   anything the worker says afterwards is dropped.
import type { LifeDown, LifeUp } from '../life';
import { CONNECT_FAILED, callPreamble } from './preamble';
import { UNMOUNTED } from './types';
import type { ConnectFailure, Preamble, Status } from './types';

export interface LifeOptions {
	preamble?: Preamble;
	onClose?(code: number, reason: string): void;
	onStatus?(status: Status, detail?: ConnectFailure): void;
}

export class PageLife {
	private id = 0;
	/** asking: the current attempt waits for its preamble; socket: its socket was asked for. */
	private phase: 'idle' | 'asking' | 'socket' = 'idle';
	private live = true;
	private readonly listener = (e: MessageEvent<LifeUp>) => this.onUp(e.data);

	constructor(
		private readonly port: MessagePort,
		private readonly o: LifeOptions
	) {
		port.addEventListener('message', this.listener);
		port.start();
	}

	/** A new attempt, unless a socket is open or being made; it replaces one waiting for its preamble. */
	connect() {
		if (!this.live || this.phase === 'socket') return;
		this.id++;
		this.phase = 'asking';
		this.down({ t: 'connect', id: this.id });
		this.o.onStatus?.('connecting');
	}

	unmount() {
		if (!this.live) return;
		this.live = false;
		this.down({ t: 'cancel' });
		this.port.removeEventListener('message', this.listener);
		const had = this.phase === 'socket';
		this.phase = 'idle';
		if (had) this.closed(UNMOUNTED.code, UNMOUNTED.reason);
	}

	private onUp(m: LifeUp) {
		if (!this.live || m.id !== this.id) return;
		switch (m.t) {
			case 'preamble':
				if (this.phase === 'asking') void this.answer(m.id);
				break;
			case 'open':
				if (this.phase === 'socket') this.o.onStatus?.('open');
				break;
			case 'closed':
				if (this.phase !== 'socket') return;
				this.phase = 'idle';
				this.closed(m.code, m.reason);
				break;
			case 'connectFailed':
				if (this.phase !== 'socket') return;
				this.phase = 'idle';
				this.o.onStatus?.('connect-failed', CONNECT_FAILED);
				break;
		}
	}

	private async answer(id: number) {
		const r = await callPreamble(this.o.preamble);
		if (!this.live || id !== this.id || this.phase !== 'asking') return;
		if (!r.ok) {
			this.phase = 'idle';
			this.down({ t: 'preambleFailed', id });
			this.o.onStatus?.('preamble-failed', r.failure);
			return;
		}
		this.phase = 'socket';
		this.down({ t: 'preamble', id, msgs: r.msgs });
	}

	/** onClose first; onStatus('closed') runs even if onClose throws. */
	private closed(code: number, reason: string) {
		try {
			this.o.onClose?.(code, reason);
		} finally {
			this.o.onStatus?.('closed');
		}
	}

	private down(m: LifeDown) {
		this.port.postMessage(m, []);
	}
}
