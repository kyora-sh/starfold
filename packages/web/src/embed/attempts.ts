// Embedded: the page's side of the connection lifecycle (life.ts). Every connect() is an
// attempt with an id; the page answers the worker's preamble request only for the current
// attempt and only while live, and decides on its own what the embedder hears:
// - each attempt ends in exactly one of: onClose (a socket was asked for), preamble-failed,
//   or connect-failed, a failed worker included; one replaced by a newer connect() or ended
//   by end() before its socket was asked for ends silently;
// - the page posts at most one preamble reply per attempt, only for the current one, and
//   ignores connect() from then until that attempt's close: the port is FIFO, so a reply
//   never follows a newer attempt's connect;
// - every callback goes through the Dispatcher; end() seals it, so after end() the only
//   callback is the onClose it reports for a socket it was waiting on.
import type { LifeDown, LifeUp } from '../life';
import { Dispatcher } from './dispatch';
import type { Report } from './dispatch';
import { CONNECT_FAILED, callPreamble } from './preamble';
import { UNMOUNTED, WORKER_FAILED } from './types';
import type { ConnectFailure, Preamble, Status } from './types';

export interface LifeOptions {
	preamble?: Preamble;
	onClose?(code: number, reason: string): void;
	onStatus?(status: Status, detail?: ConnectFailure): void;
	/** The client's own reaction to an open socket (not an embedder callback; run at once). */
	opened?(): void;
	/** Where a throwing callback's error goes (default: reportError). */
	report?: Report;
}

export class PageLife {
	private id = 0;
	/** asking: the current attempt waits for its preamble; socket: its socket was asked for. */
	private phase: 'idle' | 'asking' | 'socket' = 'idle';
	private live = true;
	private ended = false;
	/** An onClose is queued and has not run yet. */
	private closeQueued = false;
	private readonly dispatch: Dispatcher;
	private readonly listener = (e: MessageEvent<LifeUp>) => this.onUp(e.data);

	constructor(
		private readonly port: MessagePort,
		private readonly o: LifeOptions
	) {
		this.dispatch = new Dispatcher(o.report);
		port.addEventListener('message', this.listener);
		port.start();
	}

	/** A new attempt, unless a socket is open or being made; it replaces one waiting for its preamble. */
	connect() {
		if (!this.live || this.phase === 'socket') return;
		this.id++;
		this.phase = 'asking';
		this.down({ t: 'connect', id: this.id });
		this.status('connecting');
	}

	/**
	 * The worker failed (it could not be loaded, or it threw) and the caller stopped it: an
	 * attempt whose socket was asked for ends with onClose(WORKER_FAILED), one still waiting for
	 * its preamble with connect-failed, and no attempt follows.
	 */
	workerFailed() {
		if (!this.live) return;
		const was = this.phase;
		this.phase = 'idle';
		this.live = false;
		this.port.removeEventListener('message', this.listener);
		this.port.close();
		if (was === 'socket') this.closed(WORKER_FAILED.code, WORKER_FAILED.reason);
		else if (was === 'asking') this.status('connect-failed', CONNECT_FAILED);
	}

	/**
	 * Ends the lifecycle for good: no more messages are taken or sent, the port is closed,
	 * every queued callback is dropped, and the one callback after this is onClose(UNMOUNTED)
	 * if a socket was asked for. The caller stops the worker first.
	 */
	end() {
		if (this.ended) return;
		this.ended = true;
		const had = (this.live && this.phase === 'socket') || this.closeQueued;
		this.live = false;
		this.phase = 'idle';
		this.port.removeEventListener('message', this.listener);
		this.port.close();
		const onClose = this.o.onClose;
		this.dispatch.seal(
			had && onClose
				? () => onClose(UNMOUNTED.code, UNMOUNTED.reason)
				: undefined
		);
	}

	private onUp(m: LifeUp) {
		if (!this.live || m.id !== this.id) return;
		switch (m.t) {
			case 'preamble':
				if (this.phase === 'asking') void this.answer(m.id);
				break;
			case 'open':
				if (this.phase !== 'socket') return;
				this.o.opened?.();
				this.status('open');
				break;
			case 'closed':
				if (this.phase !== 'socket') return;
				this.phase = 'idle';
				this.closed(m.code, m.reason);
				break;
			case 'connectFailed':
				if (this.phase !== 'socket') return;
				this.phase = 'idle';
				this.status('connect-failed', CONNECT_FAILED);
				break;
		}
	}

	private async answer(id: number) {
		const r = await callPreamble(this.o.preamble);
		if (!this.live || id !== this.id || this.phase !== 'asking') return;
		if (!r.ok) {
			this.phase = 'idle';
			this.down({ t: 'preambleFailed', id });
			this.status('preamble-failed', r.failure);
			return;
		}
		this.phase = 'socket';
		this.down({ t: 'preamble', id, msgs: r.msgs });
	}

	/** onClose, then onStatus('closed'), each queued on its own: one throwing can't skip the other. */
	private closed(code: number, reason: string) {
		const onClose = this.o.onClose;
		if (onClose) {
			this.closeQueued = true;
			this.dispatch.call(() => {
				this.closeQueued = false;
				onClose(code, reason);
			});
		}
		this.status('closed');
	}

	private status(s: Status, detail?: ConnectFailure) {
		const onStatus = this.o.onStatus;
		if (onStatus) this.dispatch.call(() => onStatus(s, detail));
	}

	private down(m: LifeDown) {
		this.port.postMessage(m, []);
	}
}
