// The worker's socket to the server.
// On the served page it opens at once and opens again a second after every close.
// Embedded (`mount`), it opens only when the page asks, sends the messages the page's
// preamble gave it before anything else, and never opens again on its own: every close goes
// to the embedder with its code and reason.

export interface LinkHandlers {
	/** A socket was made: it is the one to send on from now. */
	opening(sock: WebSocket): void;
	/** It opened, after the preamble (if any) was sent. */
	open(): void;
	message(data: string | ArrayBuffer): void;
	closed(code: number, reason: string): void;
	/** Embedded: before every connect, the page's preamble is asked for. */
	askPreamble(): void;
}

export type LinkMode = { embed: false } | { embed: true; protocols?: string[] };

/** How long the served page waits before it opens the socket again. */
export const RETRY_MS = 1000;

export class Link {
	private sock: WebSocket | null = null;
	private asked = false;

	constructor(
		private readonly url: string,
		private readonly mode: LinkMode,
		private readonly h: LinkHandlers,
		private readonly Socket: typeof WebSocket = WebSocket,
		private readonly retryMs = RETRY_MS
	) {}

	/** The served page opens at once; embedded, nothing opens until the page asks. */
	start() {
		if (!this.mode.embed) this.open([]);
	}

	/** Embedded: the page wants a connection. False while one is open or being made. */
	request(): boolean {
		if (!this.mode.embed || this.sock || this.asked) return false;
		this.asked = true;
		this.h.askPreamble();
		return true;
	}

	/** Embedded: the page's preamble arrived; open and send it first. */
	preamble(msgs: string[]) {
		if (!this.mode.embed || !this.asked || this.sock) return;
		this.asked = false;
		this.open(msgs);
	}

	/** Embedded: the page's preamble failed; nothing opens. */
	refused() {
		this.asked = false;
	}

	private open(preamble: string[]) {
		const sock = this.mode.embed
			? new this.Socket(this.url, this.mode.protocols)
			: new this.Socket(this.url);
		this.sock = sock;
		sock.binaryType = 'arraybuffer';
		let first: string[] | null = preamble;
		this.h.opening(sock);
		sock.addEventListener('open', () => {
			for (const m of first ?? []) sock.send(m);
			first = null;
			this.h.open();
		});
		sock.addEventListener('close', (e: CloseEvent) => {
			first = null;
			if (this.sock === sock) this.sock = null;
			this.h.closed(e.code, e.reason);
			if (!this.mode.embed) setTimeout(() => this.open([]), this.retryMs);
		});
		sock.addEventListener('message', (e: MessageEvent<string | ArrayBuffer>) =>
			this.h.message(e.data)
		);
	}
}
