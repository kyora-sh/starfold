// The worker's socket to the server.
// On the served page it opens at once and opens again a second after every close.
// Embedded (`mount`), every connect is an attempt with an id: the page is asked for the
// preamble of that attempt, an answer for any other attempt is dropped, and a cancelled or
// superseded attempt never opens. The preamble's messages are sent first on the socket and
// dropped once sent; nothing opens again on its own, and every close of a socket is reported
// with its attempt id.

export interface LinkHandlers {
	/** A socket was made: it is the one to send on from now. */
	opening(sock: WebSocket): void;
	/** It opened, after the preamble (if any) was sent. */
	open(attempt: number): void;
	message(data: string | ArrayBuffer): void;
	closed(attempt: number, code: number, reason: string): void;
	/** Embedded: the page's preamble is asked for, for this attempt. */
	askPreamble(attempt: number): void;
	/** Embedded: the socket couldn't even be made (a bad URL or subprotocol list). */
	connectFailed(attempt: number): void;
}

export type LinkMode = { embed: false } | { embed: true; protocols?: string[] };

/** How long the served page waits before it opens the socket again. */
export const RETRY_MS = 1000;

export class Link {
	private sock: WebSocket | null = null;
	/** Embedded: the attempt whose preamble is awaited. */
	private pending: number | null = null;
	/** Embedded: the preamble's messages, from when its socket is made until they are sent. */
	private outgoing: string[] | null = null;
	/** Embedded: the socket abort() closed, and the close to report for it. */
	private aborted: { sock: WebSocket; code: number; reason: string } | null =
		null;

	constructor(
		private readonly url: string,
		private readonly mode: LinkMode,
		private readonly h: LinkHandlers,
		private readonly Socket: typeof WebSocket = WebSocket,
		private readonly retryMs = RETRY_MS
	) {}

	/** The served page opens at once; embedded, nothing opens until the page asks. */
	start() {
		if (!this.mode.embed) this.open(0, null);
	}

	/**
	 * Embedded: attempt `id` wants a connection; it replaces an attempt still waiting for its
	 * preamble. False (and nothing asked) while a socket is open or being made.
	 */
	request(id: number): boolean {
		if (!this.mode.embed || this.sock) return false;
		this.pending = id;
		this.h.askPreamble(id);
		return true;
	}

	/** Embedded: attempt `id`'s preamble; dropped unless `id` is the attempt waiting for it. */
	preamble(id: number, msgs: string[]) {
		if (!this.mode.embed || this.pending !== id || this.sock) return;
		this.pending = null;
		this.open(id, msgs);
	}

	/** Embedded: attempt `id`'s preamble failed; nothing opens for it. */
	refused(id: number) {
		if (this.pending === id) this.pending = null;
	}

	/** Embedded: no attempt goes on. A waiting one never opens; an open socket is closed. */
	cancel() {
		this.pending = null;
		this.outgoing = null;
		this.sock?.close(1000);
	}

	/**
	 * Embedded: the client can't go on with what the server sent. The socket is closed and
	 * its close is reported as `code` and `reason`, whatever the socket's own close says.
	 */
	abort(code: number, reason: string) {
		const sock = this.sock;
		if (!this.mode.embed || !sock) return;
		this.aborted = { sock, code, reason };
		this.outgoing = null;
		sock.close(code, reason);
	}

	/** Whether preamble messages are still held, unsent. */
	get holdsPreamble(): boolean {
		return this.outgoing !== null;
	}

	private open(id: number, msgs: string[] | null) {
		let sock: WebSocket;
		if (this.mode.embed) {
			try {
				sock = new this.Socket(this.url, this.mode.protocols);
			} catch {
				this.h.connectFailed(id);
				return;
			}
			this.outgoing = msgs;
		} else {
			sock = new this.Socket(this.url);
		}
		this.sock = sock;
		sock.binaryType = 'arraybuffer';
		this.h.opening(sock);
		this.attach(sock, id);
	}

	/** The socket's listeners; the preamble's messages are not in their scope. */
	private attach(sock: WebSocket, id: number) {
		sock.addEventListener('open', () => {
			if (this.sock === sock) this.sendPreamble(sock);
			this.h.open(id);
		});
		sock.addEventListener('close', (e: CloseEvent) => {
			if (this.sock === sock) {
				this.sock = null;
				this.outgoing = null;
			}
			const a = this.aborted?.sock === sock ? this.aborted : null;
			if (a) this.aborted = null;
			this.h.closed(id, a?.code ?? e.code, a?.reason ?? e.reason);
			if (!this.mode.embed) setTimeout(() => this.open(0, null), this.retryMs);
		});
		sock.addEventListener('message', (e: MessageEvent<string | ArrayBuffer>) =>
			this.h.message(e.data)
		);
	}

	private sendPreamble(sock: WebSocket) {
		const msgs = this.outgoing ?? [];
		this.outgoing = null;
		for (let i = 0; i < msgs.length; i++) sock.send(msgs[i]);
	}
}
