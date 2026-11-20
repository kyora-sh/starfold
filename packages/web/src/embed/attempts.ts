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
//   callback is the onClose it reports for a socket it was waiting on;
// - with a Reconnector, a close is followed by 'reconnecting' and a later attempt of the
//   lifecycle's own, or by 'gave-up' and then 'closed', and so is a reconnect attempt that
//   ended in preamble-failed or connect-failed; the host's connect() starts counting over.
import type { LifeDown, LifeUp } from '../life';
import { Dispatcher } from './dispatch';
import type { Report } from './dispatch';
import { CONNECT_FAILED, callPreamble } from './preamble';
import type { Decision, Reconnector } from './reconnect';
import { UNMOUNTED, WORKER_FAILED } from './types';
import type {
	ConnectFailure,
	Preamble,
	ReconnectEvent,
	ReconnectWhy,
	Status,
} from './types';

export interface LifeOptions {
	preamble?: Preamble;
	onClose?(code: number, reason: string): void;
	onStatus?(status: Status, detail?: ConnectFailure): void;
	onReconnect?(e: ReconnectEvent): void;
	/** Set when the embedder asked for `reconnect`; absent, a close ends it as before. */
	reconnect?: Reconnector;
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

	/** The last close, for a gave-up that follows a later attempt's failure. */
	private lastCode = 0;

	/**
	 * A new attempt, unless a socket is open or being made; it replaces one waiting for its
	 * preamble, and one waiting to reconnect, whose count starts over.
	 */
	connect() {
		if (!this.live || this.phase === 'socket') return;
		this.o.reconnect?.reset();
		this.attempt();
	}

	private attempt() {
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
		const waiting = this.o.reconnect?.pending ?? false;
		this.phase = 'idle';
		this.live = false;
		this.port.removeEventListener('message', this.listener);
		this.port.close();
		if (was === 'socket') this.closed(WORKER_FAILED.code, WORKER_FAILED.reason);
		else if (was === 'asking') this.failed('connect-failed', CONNECT_FAILED);
		else if (waiting) this.giveUp('connect-failed');
		this.o.reconnect?.reset();
	}

	/**
	 * Ends the lifecycle for good: no more messages are taken or sent, the port is closed,
	 * every queued callback is dropped, and the one callback after this is onClose(UNMOUNTED)
	 * if a socket was asked for. The caller stops the worker first.
	 */
	end() {
		if (this.ended) return;
		this.ended = true;
		this.o.reconnect?.reset();
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
				if (this.o.reconnect) {
					const r = this.o.reconnect;
					if (r.reconnecting)
						this.event({
							kind: 'reconnected',
							attempt: r.attempt,
							code: this.lastCode,
						});
					r.opened();
				}
				break;
			case 'closed':
				if (this.phase !== 'socket') return;
				this.phase = 'idle';
				this.closed(m.code, m.reason, m.openMs);
				break;
			case 'connectFailed':
				if (this.phase !== 'socket') return;
				this.phase = 'idle';
				this.failed('connect-failed', CONNECT_FAILED);
				break;
		}
	}

	private async answer(id: number) {
		const r = await callPreamble(this.o.preamble);
		if (!this.live || id !== this.id || this.phase !== 'asking') return;
		if (!r.ok) {
			this.phase = 'idle';
			this.down({ t: 'preambleFailed', id });
			this.failed('preamble-failed', r.failure);
			return;
		}
		const send = () => {
			if (!this.live || id !== this.id || this.phase !== 'asking') return;
			this.phase = 'socket';
			this.down({ t: 'preamble', id, msgs: r.msgs });
		};
		// A reconnect attempt sends no reply while the page is hidden or offline, even if the
		// page went so while its preamble was pending, nor past the run's deadline.
		const re = this.o.reconnect;
		if (re?.reconnecting) re.hold(send, () => this.expireAsking(id));
		else send();
	}

	/** A reconnect attempt still before its socket when the run's deadline passes. */
	private expireAsking(id: number) {
		if (!this.live || id !== this.id || this.phase !== 'asking') return;
		this.phase = 'idle';
		this.down({ t: 'preambleFailed', id });
		this.giveUp('time');
	}

	/** An attempt that ended before a socket; one made to reconnect ends the run. */
	private failed(
		s: 'preamble-failed' | 'connect-failed',
		detail: ConnectFailure
	) {
		const r = this.o.reconnect;
		const run = r?.reconnecting ?? false;
		this.status(s, detail);
		if (run) this.giveUp(s);
		r?.reset();
	}

	/**
	 * onClose, then what follows it: onStatus('closed'), or with reconnect 'reconnecting' and an
	 * attempt later, or 'gave-up' and 'closed'. Each is queued on its own: one throwing can't skip
	 * another.
	 */
	private closed(code: number, reason: string, openMs?: number | null) {
		const onClose = this.o.onClose;
		if (onClose) {
			this.closeQueued = true;
			this.dispatch.call(() => {
				this.closeQueued = false;
				onClose(code, reason);
			});
		}
		this.lastCode = code;
		const r = this.o.reconnect;
		if (!r) {
			this.status('closed');
			return;
		}
		// A failed worker: nothing can follow.
		if (!this.live) {
			this.giveUp('code');
			return;
		}
		const id = this.id;
		let d: Decision;
		try {
			d = r.closed(code, reason, openMs);
		} catch (err) {
			this.dispatch.call(() => {
				throw err;
			});
			d = { t: 'give-up', why: 'code' };
		}
		// shouldReconnect called unmount() or connect(): that call decides what follows.
		if (!this.live || this.id !== id || this.phase !== 'idle') return;
		if (d.t === 'give-up') {
			this.giveUp(d.why);
			return;
		}
		this.status('reconnecting');
		this.event({
			kind: 'reconnecting',
			attempt: d.attempt,
			delayMs: d.delayMs,
			code,
		});
		r.schedule(
			d.delayMs,
			() => {
				if (!this.live || this.phase !== 'idle') return;
				this.attempt();
				const id = this.id;
				r.guard(() => this.expireAsking(id));
			},
			() => {
				if (this.live && this.phase === 'idle') this.giveUp('time');
			}
		);
	}

	/** 'gave-up' with the last close's code, then 'closed': the run is over. */
	private giveUp(why: ReconnectWhy) {
		this.o.reconnect?.reset();
		this.status('gave-up');
		this.event({ kind: 'gave-up', why, code: this.lastCode });
		this.status('closed');
	}

	private event(e: ReconnectEvent) {
		const onReconnect = this.o.onReconnect;
		if (onReconnect) this.dispatch.call(() => onReconnect(e));
	}

	private status(s: Status, detail?: ConnectFailure) {
		const onStatus = this.o.onStatus;
		if (onStatus) this.dispatch.call(() => onStatus(s, detail));
	}

	private down(m: LifeDown) {
		this.port.postMessage(m, []);
	}
}
