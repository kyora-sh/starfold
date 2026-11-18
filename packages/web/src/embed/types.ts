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

/** What onClose gets for a socket that unmount() ended: 1001, "going away". */
export const UNMOUNTED = { code: 1001, reason: 'unmounted' } as const;
