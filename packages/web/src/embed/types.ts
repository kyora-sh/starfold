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
	| 'connect-failed';

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
