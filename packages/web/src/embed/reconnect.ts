// Embedded, opt-in: what happens after a close when `reconnect` is set. It does no more than a
// careful host would: wait, then the same connect() with the same preamble, so a ticket is still
// minted by the host for every attempt. Which closes are worth another attempt is `retryable`;
// the limits are counted per run, a run being the closes since the last connection that stayed
// open for STABLE_MS (or since the host's own connect()). The clock, timers, randomness and the
// page's visibility and network state come in as an `Env`, so tests drive them.
import type { ReconnectOptions, ReconnectWhy } from './types';

/** A connection open this long ends the run: the next close starts counting from zero. */
export const STABLE_MS = 10_000;
/** Closes in a row with 1006 that never reached open before the run gives up. */
export const NEVER_OPENED_MAX = 3;

export const RECONNECT_DEFAULTS = {
	initialMs: 500,
	factor: 2,
	capMs: 30_000,
	maxAttempts: 10,
	maxMs: 300_000,
} as const;

/**
 * Whether a close is worth another attempt, by its code alone. 1006 (no close frame: a network
 * drop, a server that crashed or was stopped, or an upgrade the server refused), 1001 (going
 * away), 1012 (service restart) and 1013 (try again later) are; so is a 44xx code from the host's
 * gateway or the server, except 4406 (a protocol 1 viewer this server's build can't serve). 1000
 * (closed on purpose), the client's own 4000 to 4002, a bad frame (1003, 1007, 1009), 4406 and
 * every other code are not.
 */
export function retryable(code: number): boolean {
	if (code === 1006 || code === 1001 || code === 1012 || code === 1013)
		return true;
	return code >= 4400 && code <= 4499 && code !== 4406;
}

/** Full jitter: a delay in [0, min(cap, initial * factor^(attempt - 1))). */
export function backoff(
	attempt: number,
	o: { initialMs: number; factor: number; capMs: number },
	random: () => number
): number {
	const ceiling = Math.min(o.capMs, o.initialMs * o.factor ** (attempt - 1));
	return Math.floor(random() * ceiling);
}

export interface Env {
	now(): number;
	setTimeout(f: () => void, ms: number): unknown;
	clearTimeout(h: unknown): void;
	random(): number;
	hidden(): boolean;
	online(): boolean;
	/** Calls `f` on every visibility or network change until the returned remover runs. */
	onWake(f: () => void): () => void;
}

/** The page's own: `document.visibilityState`, `navigator.onLine` and their events. */
export function pageEnv(): Env {
	return {
		now: () => performance.now(),
		setTimeout: (f, ms) => setTimeout(f, ms),
		clearTimeout: (h) => clearTimeout(h as ReturnType<typeof setTimeout>),
		random: Math.random,
		hidden: () => document.visibilityState === 'hidden',
		online: () => navigator.onLine !== false,
		onWake(f) {
			document.addEventListener('visibilitychange', f);
			window.addEventListener('online', f);
			return () => {
				document.removeEventListener('visibilitychange', f);
				window.removeEventListener('online', f);
			};
		},
	};
}

export type Decision =
	| { t: 'retry'; attempt: number; delayMs: number }
	| { t: 'give-up'; why: ReconnectWhy };

interface Run {
	start: number;
	attempts: number;
	/** Time spent offline, which does not count toward maxMs. */
	pausedMs: number;
	neverOpened: number;
	/** The 44xx code of the last close if it came right after an open. */
	last44xx: number | null;
}

export class Reconnector {
	private readonly o: Required<Omit<ReconnectOptions, 'shouldReconnect'>> &
		Pick<ReconnectOptions, 'shouldReconnect'>;
	private run: Run | null = null;
	private openedAt: number | null = null;
	private timer: unknown = null;
	private stopWake: (() => void) | null = null;
	private offlineSince: number | null = null;

	constructor(
		options: true | ReconnectOptions,
		private readonly env: Env
	) {
		const o = options === true ? {} : options;
		this.o = {
			initialMs: o.initialMs ?? RECONNECT_DEFAULTS.initialMs,
			factor: o.factor ?? RECONNECT_DEFAULTS.factor,
			capMs: o.capMs ?? RECONNECT_DEFAULTS.capMs,
			maxAttempts: o.maxAttempts ?? RECONNECT_DEFAULTS.maxAttempts,
			maxMs: o.maxMs ?? RECONNECT_DEFAULTS.maxMs,
			shouldReconnect: o.shouldReconnect,
		};
	}

	/** Whether the attempt now under way is one this made (not the host's). */
	get reconnecting(): boolean {
		return this.run !== null && this.run.attempts > 0;
	}

	/** The attempt number of the current run (0 before any). */
	get attempt(): number {
		return this.run?.attempts ?? 0;
	}

	opened() {
		this.openedAt = this.env.now();
		if (this.run) this.run.neverOpened = 0;
	}

	/** What to do after a socket closed with `code`; with `retry`, the attempt is counted. */
	closed(code: number, reason: string): Decision {
		const now = this.env.now();
		const openFor = this.openedAt === null ? null : now - this.openedAt;
		this.openedAt = null;
		if (openFor !== null && openFor >= STABLE_MS) this.run = null;
		const run = (this.run ??= {
			start: now,
			attempts: 0,
			pausedMs: 0,
			neverOpened: 0,
			last44xx: null,
		});
		const wanted = this.o.shouldReconnect
			? this.o.shouldReconnect(code, reason)
			: retryable(code);
		if (!wanted) return { t: 'give-up', why: 'code' };
		if (code >= 4400 && code <= 4499 && openFor !== null) {
			if (run.last44xx === code) return { t: 'give-up', why: 'repeated-close' };
			run.last44xx = code;
		} else run.last44xx = null;
		if (code === 1006 && openFor === null) {
			run.neverOpened++;
			if (run.neverOpened >= NEVER_OPENED_MAX)
				return { t: 'give-up', why: 'never-opened' };
		}
		if (run.attempts >= this.o.maxAttempts)
			return { t: 'give-up', why: 'attempts' };
		if (now - run.start - run.pausedMs >= this.o.maxMs)
			return { t: 'give-up', why: 'time' };
		run.attempts++;
		return {
			t: 'retry',
			attempt: run.attempts,
			delayMs: backoff(run.attempts, this.o, this.env.random),
		};
	}

	/**
	 * Calls `connect` after `delayMs`; then, while the page is hidden or offline, waits until it is
	 * neither, and connects at once.
	 */
	schedule(delayMs: number, connect: () => void) {
		this.cancel();
		this.timer = this.env.setTimeout(() => {
			this.timer = null;
			this.whenAwake(connect);
		}, delayMs);
	}

	private whenAwake(connect: () => void) {
		const awake = () => !this.env.hidden() && this.env.online();
		const offline = () => {
			if (!this.env.online()) this.offlineSince ??= this.env.now();
		};
		if (awake()) {
			connect();
			return;
		}
		offline();
		this.stopWake = this.env.onWake(() => {
			if (this.offlineSince !== null && this.env.online()) {
				if (this.run) this.run.pausedMs += this.env.now() - this.offlineSince;
				this.offlineSince = null;
			}
			offline();
			if (!awake()) return;
			this.cancel();
			connect();
		});
	}

	/** Whether an attempt is waiting: its delay, or the page to wake. */
	get pending(): boolean {
		return this.timer !== null || this.stopWake !== null;
	}

	/** Drops a waiting attempt. */
	cancel() {
		if (this.timer !== null) this.env.clearTimeout(this.timer);
		this.timer = null;
		this.stopWake?.();
		this.stopWake = null;
		if (this.offlineSince !== null && this.run)
			this.run.pausedMs += this.env.now() - this.offlineSince;
		this.offlineSince = null;
	}

	/** The host's own connect(), or the end of a run: counting starts over. */
	reset() {
		this.cancel();
		this.run = null;
	}
}
