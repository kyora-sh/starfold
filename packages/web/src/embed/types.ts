// The embed's public types, in a module with no imports so its declarations ship on their own.

/** Text message(s) to send before anything of starfold's: a ticket, say. Called before every connect. */
export type Preamble = () => string | string[] | Promise<string | string[]>;

/**
 * Why a connect failed before any socket existed. Fixed values: the preamble's own error (or
 * the URL) is never passed on, since it may hold a credential.
 */
export type ConnectFailure =
	| {
			kind: 'preamble-failed';
			/** The preamble threw, its promise rejected, or it gave something other than text. */
			cause: 'threw' | 'rejected' | 'not-text';
			message: string;
	  }
	| { kind: 'connect-failed'; message: string };

export type Status =
	| 'connecting'
	| 'open'
	| 'closed'
	| 'preamble-failed'
	| 'connect-failed'
	| 'reconnecting'
	| 'gave-up';

/**
 * The `reconnect` option's limits. The delay before attempt n is a random time below
 * min(capMs, initialMs * factor^(n-1)); a run gives up after maxAttempts attempts or maxMs
 * since its first close (time offline not counted), whichever comes first. A connection that
 * stays open for 10 s ends the run.
 */
export interface ReconnectOptions {
	/** Default 500. */
	initialMs?: number;
	/** Default 2. */
	factor?: number;
	/** Default 30000. */
	capMs?: number;
	/** Default 10. */
	maxAttempts?: number;
	/** Default 300000 (5 min). */
	maxMs?: number;
	/** Whether a close with this code and reason is worth another attempt; replaces the default table. */
	shouldReconnect?(code: number, reason: string): boolean;
}

/**
 * Why reconnecting stopped: `code`, a close not worth another attempt; `repeated-close`, two
 * closes in a row with the same 44xx code, each within 10 s of its open; `never-opened`, three
 * 1006 closes in a row that never reached open (an upgrade the server refuses looks like this);
 * `attempts` or `time`, a limit; `preamble-failed` or `connect-failed`, a reconnect attempt that
 * ended so.
 */
export type ReconnectWhy =
	| 'code'
	| 'repeated-close'
	| 'never-opened'
	| 'attempts'
	| 'time'
	| 'preamble-failed'
	| 'connect-failed';

/** What `onReconnect` gets; `code` and `reason` are the last close's. */
export type ReconnectEvent =
	| {
			kind: 'reconnecting';
			attempt: number;
			delayMs: number;
			code: number;
			reason: string;
	  }
	| { kind: 'reconnected'; attempt: number }
	| { kind: 'gave-up'; why: ReconnectWhy; code: number; reason: string };

/**
 * What onClose gets when unmount() ends a connection whose socket was asked for: 1001, "going
 * away". unmount() stops the worker at once, so the server sees the socket drop without a
 * close frame, and a close the server sent at the same moment is reported as this.
 */
export const UNMOUNTED = { code: 1001, reason: 'unmounted' } as const;

/**
 * What onClose gets when the client could not handle what the server sent (a message that is
 * not an object, a malformed offer): the client closes the socket and reports this, whatever
 * the socket's own close says.
 */
export const PROTOCOL_ERROR = {
	code: 4000,
	reason: 'starfold: protocol error',
} as const;

/**
 * What onClose gets when the server finds no codec this browser decodes (its `codec` message
 * carries an error): nothing could be shown, so the client closes the socket with this code.
 */
export const NO_COMMON_CODEC = {
	code: 4001,
	reason: 'starfold: no common codec',
} as const;

/**
 * What onClose gets when the worker fails at run time after its socket was asked for: the
 * worker is stopped, so the server sees the socket drop without a close frame.
 */
export const WORKER_FAILED = {
	code: 4002,
	reason: 'starfold: worker failed',
} as const;
