import { afterEach, describe, expect, test } from 'bun:test';
import { Link, RETRY_MS } from '../src/link';
import type { LinkHandlers, LinkMode } from '../src/link';
import { fakeServer, sleep, until } from './server';

/** Handlers that record what the link does; `open` sends one message of the client's own. */
function recorder() {
	const r = {
		asked: [] as number[],
		opened: [] as number[],
		closed: [] as [number, number, string][],
		failed: [] as number[],
		sock: null as WebSocket | null,
	};
	const h: LinkHandlers = {
		opening(sock) {
			r.sock = sock;
		},
		open(id) {
			r.opened.push(id);
			r.sock!.send('client-first');
		},
		message() {},
		closed(id, code, reason) {
			r.closed.push([id, code, reason]);
		},
		askPreamble(id) {
			r.asked.push(id);
		},
		connectFailed(id) {
			r.failed.push(id);
		},
	};
	return { r, h };
}

let stop: (() => void) | null = null;
function server() {
	const s = fakeServer();
	stop = s.stop;
	return s;
}
afterEach(() => {
	stop?.();
	stop = null;
});

const embed: LinkMode = { embed: true };

/** A socket that opens at once and, closed, reports an abnormal close (1006, no reason). */
class DropsOnClose extends EventTarget {
	binaryType = '';
	constructor() {
		super();
		queueMicrotask(() => this.dispatchEvent(new Event('open')));
	}
	send() {}
	close() {
		this.dispatchEvent(
			Object.assign(new Event('close'), { code: 1006, reason: '' })
		);
	}
}

/** A WebSocket constructor that throws, as one does for a bad URL. */
function Throws(): never {
	throw new SyntaxError('bad url ws://x/?token=secret');
}

describe('embedded link', () => {
	test('nothing opens until the preamble comes, even well after it was asked for', async () => {
		const s = server();
		const { r, h } = recorder();
		const link = new Link(s.url, embed, h);
		link.start();
		expect(link.request(1)).toBe(true);
		expect(r.asked).toEqual([1]);
		await sleep(200);
		expect(s.conns.length).toBe(0);
	});

	test("the preamble's messages are the first sent, and are dropped once sent", async () => {
		const s = server();
		const { r, h } = recorder();
		const link = new Link(s.url, embed, h);
		link.request(1);
		link.preamble(1, ['ticket-1', 'ticket-2']);
		expect(link.holdsPreamble).toBe(true);
		await until(() => (s.conns[0]?.got.length ?? 0) >= 3);
		expect(s.conns[0].got).toEqual(['ticket-1', 'ticket-2', 'client-first']);
		expect(r.opened).toEqual([1]);
		expect(link.holdsPreamble).toBe(false);
	});

	test('the URL is used exactly as given, query included, with the subprotocols asked for', async () => {
		const s = server();
		const { h } = recorder();
		const link = new Link(
			`${s.url}?room=a&x=1`,
			{ embed: true, protocols: ['starfold.v1'] },
			h
		);
		link.request(1);
		link.preamble(1, []);
		await until(() => s.conns.length === 1 && !!s.conns[0].ws);
		expect(s.conns[0].path).toBe('/desk/ws?room=a&x=1');
		expect(s.conns[0].protocols).toBe('starfold.v1');
	});

	test("an older attempt's preamble never reaches the attempt that replaced it", async () => {
		const s = server();
		const { r, h } = recorder();
		const link = new Link(s.url, embed, h);
		link.request(1);
		link.request(2);
		expect(r.asked).toEqual([1, 2]);
		link.preamble(1, ['ticket-1']);
		await sleep(150);
		expect(s.conns.length).toBe(0);
		link.preamble(2, ['ticket-2']);
		link.preamble(1, ['ticket-1']);
		await until(() => (s.conns[0]?.got.length ?? 0) >= 2);
		await sleep(100);
		expect(s.conns.length).toBe(1);
		expect(s.conns[0].got).toEqual(['ticket-2', 'client-first']);
	});

	test('a cancelled attempt never opens, and cancel closes an open socket', async () => {
		const s = server();
		const { r, h } = recorder();
		const link = new Link(s.url, embed, h);
		link.request(1);
		link.cancel();
		link.preamble(1, ['ticket-1']);
		await sleep(150);
		expect(s.conns.length).toBe(0);
		expect(link.holdsPreamble).toBe(false);
		link.request(2);
		link.preamble(2, ['ticket-2']);
		await until(() => r.opened.length === 1);
		link.cancel();
		await until(() => s.conns[0].closed && r.closed.length === 1);
		expect(r.closed[0][0]).toBe(2);
	});

	test('a close reaches the page with its attempt, code and reason, and nothing reconnects', async () => {
		const s = server();
		const { r, h } = recorder();
		const link = new Link(s.url, embed, h);
		link.request(1);
		link.preamble(1, ['ticket']);
		await until(() => !!s.conns[0]?.ws && r.opened.length === 1);
		s.conns[0].ws!.close(4401, 'ticket expired');
		await until(() => r.closed.length === 1);
		expect(r.closed[0]).toEqual([1, 4401, 'ticket expired']);
		await sleep(RETRY_MS + 500);
		expect(s.conns.length).toBe(1);
		expect(link.request(2)).toBe(true);
		link.preamble(2, ['ticket-again']);
		await until(() => (s.conns[1]?.got.length ?? 0) >= 1);
		expect(s.conns[1].got[0]).toBe('ticket-again');
	});

	test('a socket that cannot be made reports connect-failed, keeps nothing, and the next attempt can go', () => {
		const { r, h } = recorder();
		const link = new Link(
			'ws://x/#frag',
			embed,
			h,
			Throws as unknown as typeof WebSocket
		);
		link.request(1);
		link.preamble(1, ['ticket']);
		expect(r.failed).toEqual([1]);
		expect(link.holdsPreamble).toBe(false);
		expect(link.request(2)).toBe(true);
	});
});

describe('abort', () => {
	test('closes the socket and reports the given code and reason, whatever the close echoes', async () => {
		const s = server();
		const { r, h } = recorder();
		const link = new Link(s.url, embed, h);
		link.request(1);
		link.preamble(1, ['ticket']);
		await until(() => r.opened.length === 1);
		link.abort(4000, 'starfold: protocol error');
		await until(() => r.closed.length === 1 && s.conns[0].closed);
		expect(r.closed[0]).toEqual([1, 4000, 'starfold: protocol error']);
	});

	test("reports the given code even when the socket's own close says otherwise", async () => {
		const { r, h } = recorder();
		const link = new Link(
			'ws://x/ws',
			embed,
			h,
			DropsOnClose as unknown as typeof WebSocket
		);
		link.request(1);
		link.preamble(1, []);
		await until(() => r.opened.length === 1);
		link.abort(4000, 'starfold: protocol error');
		expect(r.closed).toEqual([[1, 4000, 'starfold: protocol error']]);
	});

	test('does nothing on the served page', async () => {
		const s = server();
		const { r, h } = recorder();
		const link = new Link(s.url, { embed: false }, h);
		link.start();
		await until(() => r.opened.length === 1);
		link.abort(4000, 'x');
		await sleep(100);
		expect(r.closed).toEqual([]);
	});
});

describe("the served page's link is unchanged", () => {
	test('it opens at once, asks for no preamble, and ignores the embedded calls', async () => {
		const s = server();
		const { r, h } = recorder();
		const link = new Link(s.url, { embed: false }, h);
		link.start();
		await until(() => (s.conns[0]?.got.length ?? 0) >= 1);
		expect(s.conns[0].got[0]).toBe('client-first');
		expect(link.request(1)).toBe(false);
		link.preamble(1, ['x']);
		expect(r.asked).toEqual([]);
	});

	test('it opens again a second after a close, on the real timer', async () => {
		const s = server();
		const { r, h } = recorder();
		new Link(s.url, { embed: false }, h).start();
		await until(() => !!s.conns[0]?.ws);
		const closedAt = Date.now();
		s.conns[0].ws!.close(1000, 'bye');
		await until(() => r.closed.length === 1);
		await sleep(RETRY_MS - 150);
		expect(s.conns.length).toBe(1);
		await until(() => s.conns.length === 2, 1500);
		expect(s.conns[1].at - closedAt).toBeGreaterThanOrEqual(RETRY_MS - 50);
		expect(RETRY_MS).toBe(1000);
	});

	test('it makes its socket from the URL alone', () => {
		const made: unknown[][] = [];
		class Recording extends EventTarget {
			binaryType = '';
			constructor(...args: unknown[]) {
				super();
				made.push(args);
			}
		}
		const { h } = recorder();
		new Link(
			'ws://x/ws?token=t',
			{ embed: false },
			h,
			Recording as unknown as typeof WebSocket
		).start();
		expect(made).toEqual([['ws://x/ws?token=t']]);
	});
});
