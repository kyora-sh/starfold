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
 * gateway or the server, except 4406 (a protocol 1 viewer this server can't serve). 1000 (closed
 * on purpose), the client's own 4000 to 4002, a bad frame (1003, 1007, 1009), 4406 and every
 * other code are not.
 */
export function retryable(code: number): boolean {
	if (code === 1006 || code === 1001 || code === 1012 || code === 1013)
		return true;
	return is44xx(code) && code !== 4406;
}

const is44xx = (code: number) => code >= 4400 && code <= 4499;

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
			window.addEventListener('offline', f);
			return () => {
				document.removeEventListener('visibilitychange', f);
				window.removeEventListener('online', f);
				window.removeEventListener('offline', f);
			};
		},
	};
}

export type Decision =
	| { t: 'retry'; attempt: number; delayMs: number }
	| { t: 'give-up'; why: ReconnectWhy };

type Limits = Required<Omit<ReconnectOptions, 'shouldReconnect'>>;

/** The limits with their defaults; a limit that is not a positive number (factor: >= 1) throws. */
export function limits(options: true | ReconnectOptions): Limits {
	const o = options === true ? {} : options;
	const out: Limits = {
		initialMs: o.initialMs ?? RECONNECT_DEFAULTS.initialMs,
		factor: o.factor ?? RECONNECT_DEFAULTS.factor,
		capMs: o.capMs ?? RECONNECT_DEFAULTS.capMs,
		maxAttempts: o.maxAttempts ?? RECONNECT_DEFAULTS.maxAttempts,
		maxMs: o.maxMs ?? RECONNECT_DEFAULTS.maxMs,
	};
	for (const [k, v] of Object.entries(out)) {
		const min = k === 'factor' ? 1 : 0;
		if (
			typeof v !== 'number' ||
			!Number.isFinite(v) ||
			v < min ||
			(min === 0 && v === 0)
		)
			throw new TypeError(`starfold: reconnect.${k} must be a positive number`);
	}
	return out;
}

interface Run {
	start: number;
	attempts: number;
	/** Time spent offline while waiting, which does not count toward maxMs. */
	pausedMs: number;
	neverOpened: number;
	/** The 44xx code of the last close if it came right after an open. */
	last44xx: number | null;
}

/** An attempt waiting for its delay and for the page to be visible and online. */
interface Wait {
	timer: unknown;
	due: boolean;
	stop: () => void;
	go: () => void;
}

export class Reconnector {
	private readonly o: Limits;
	private readonly should?: ReconnectOptions['shouldReconnect'];
	private run: Run | null = null;
	private openedAt: number | null = null;
	private wait: Wait | null = null;
	private offlineSince: number | null = null;

	constructor(
		options: true | ReconnectOptions,
		private readonly env: Env
	) {
		this.o = limits(options);
		this.should = options === true ? undefined : options.shouldReconnect;
	}

	/** Whether the attempt now under way is one this made (not the host's). */
	get reconnecting(): boolean {
		return this.run !== null && this.run.attempts > 0;
	}

	/** The attempt number of the current run (0 before any). */
	get attempt(): number {
		return this.run?.attempts ?? 0;
	}

	/** Whether an attempt is waiting: its delay, or the page to wake. */
	get pending(): boolean {
		return this.wait !== null;
	}

	opened() {
		this.openedAt = this.env.now();
		if (this.run) this.run.neverOpened = 0;
	}

	/**
	 * What to do after a socket closed with `code`; with `retry`, the attempt is counted. Calls
	 * the embedder's `shouldReconnect`, which may throw or call back into the client: the caller
	 * handles both.
	 */
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
		const short = openFor !== null && openFor < STABLE_MS;
		const repeat = short && is44xx(code) && run.last44xx === code;
		run.last44xx = short && is44xx(code) ? code : null;
		run.neverOpened =
			code === 1006 && openFor === null ? run.neverOpened + 1 : 0;
		const wanted = this.should ? this.should(code, reason) : retryable(code);
		if (!wanted) return { t: 'give-up', why: 'code' };
		if (repeat) return { t: 'give-up', why: 'repeated-close' };
		if (run.neverOpened >= NEVER_OPENED_MAX)
			return { t: 'give-up', why: 'never-opened' };
		if (run.attempts >= this.o.maxAttempts)
			return { t: 'give-up', why: 'attempts' };
		if (this.left() <= 0) return { t: 'give-up', why: 'time' };
		run.attempts++;
		return {
			t: 'retry',
			attempt: run.attempts,
			delayMs: backoff(run.attempts, this.o, this.env.random),
		};
	}

	/** What is left of the run's maxMs, time offline not counted. */
	private left(): number {
		if (!this.run) return this.o.maxMs;
		const now = this.env.now();
		const offline = this.offlineSince === null ? 0 : now - this.offlineSince;
		return this.o.maxMs - (now - this.run.start - this.run.pausedMs - offline);
	}

	/** Starts or ends a stretch offline as `navigator.onLine` says; the stretch is not counted. */
	private network() {
		const now = this.env.now();
		if (!this.env.online()) this.offlineSince ??= now;
		else if (this.offlineSince !== null) {
			if (this.run) this.run.pausedMs += now - this.offlineSince;
			this.offlineSince = null;
		}
	}

	private awake(): boolean {
		return !this.env.hidden() && this.env.online();
	}

	/**
	 * Calls `connect` once `delayMs` is up and the page is visible and online; coming back online
	 * connects at once, delay or not. If maxMs runs out first, calls `expire` instead (at the
	 * deadline, or on waking past it).
	 */
	schedule(delayMs: number, connect: () => void, expire: () => void) {
		this.cancel();
		this.network();
		const w: Wait = {
			timer: null,
			due: false,
			stop: () => {},
			go: () => {
				this.cancel();
				if (this.left() <= 0) expire();
				else connect();
			},
		};
		const left = this.left();
		w.timer = this.env.setTimeout(
			() => {
				w.timer = null;
				w.due = true;
				if (this.left() <= 0 && this.env.online()) w.go();
				else if (this.awake()) w.go();
			},
			Math.max(0, Math.min(delayMs, left))
		);
		w.stop = this.env.onWake(() => {
			const wasOffline = this.offlineSince !== null;
			this.network();
			const back = wasOffline && this.offlineSince === null;
			if ((w.due || back) && this.awake()) w.go();
		});
		this.wait = w;
	}

	/** Calls `f` now if the page is visible and online, else once it is (dropped by cancel/reset). */
	hold(f: () => void) {
		if (this.awake()) {
			f();
			return;
		}
		this.cancel();
		this.network();
		const w: Wait = {
			timer: null,
			due: true,
			stop: () => {},
			go: () => {
				this.cancel();
				f();
			},
		};
		w.stop = this.env.onWake(() => {
			this.network();
			if (this.awake()) w.go();
		});
		this.wait = w;
	}

	/** Drops a waiting attempt. */
	cancel() {
		const w = this.wait;
		this.wait = null;
		if (w) {
			if (w.timer !== null) this.env.clearTimeout(w.timer);
			w.stop();
		}
		if (this.offlineSince !== null && this.run)
			this.run.pausedMs += this.env.now() - this.offlineSince;
		this.offlineSince = null;
	}

	/** The host's own connect(), or the end of a run: counting starts over. */
	reset() {
		this.cancel();
		this.run = null;
		this.openedAt = null;
	}
}
