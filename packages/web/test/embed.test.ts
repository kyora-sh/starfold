import { afterEach, describe, expect, test } from 'bun:test';
import { Link, RETRY_MS } from '../src/link';
import type { LinkHandlers, LinkMode } from '../src/link';
import { answerPreamble } from '../src/embed/preamble';
import type { ToWorker } from '../src/protocol';

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** A WebSocket server that records every connection's path, subprotocols and messages. */
function fakeServer() {
	const conns: {
		path: string;
		protocols: string | null;
		got: string[];
		ws?: Bun.ServerWebSocket<number>;
	}[] = [];
	const server = Bun.serve<number>({
		port: 0,
		fetch(req, srv) {
			const u = new URL(req.url);
			const protocols = req.headers.get('sec-websocket-protocol');
			conns.push({ path: u.pathname + u.search, protocols, got: [] });
			const chosen = protocols?.split(',')[0].trim();
			return srv.upgrade(req, {
				data: conns.length - 1,
				headers: chosen ? { 'Sec-WebSocket-Protocol': chosen } : undefined,
			})
				? undefined
				: new Response('no upgrade', { status: 400 });
		},
		websocket: {
			open(ws) {
				conns[ws.data].ws = ws;
			},
			message(ws, m) {
				conns[ws.data].got.push(String(m));
			},
		},
	});
	return { server, conns, url: `ws://127.0.0.1:${server.port}/desk/ws` };
}

/** Handlers that record what the link does; `open` sends one message of the client's own. */
function recorder() {
	const r = {
		asked: 0,
		opened: 0,
		closed: [] as [number, string][],
		sock: null as WebSocket | null,
	};
	const h: LinkHandlers = {
		opening(sock) {
			r.sock = sock;
		},
		open() {
			r.opened++;
			r.sock!.send('client-first');
		},
		message() {},
		closed(code, reason) {
			r.closed.push([code, reason]);
		},
		askPreamble() {
			r.asked++;
		},
	};
	return { r, h };
}

async function until(f: () => boolean, ms = 2000) {
	const end = Date.now() + ms;
	while (!f()) {
		if (Date.now() > end) throw new Error('timed out');
		await sleep(5);
	}
}

let stop: (() => void) | null = null;
afterEach(() => {
	stop?.();
	stop = null;
});

describe('embedded link', () => {
	const embed: LinkMode = { embed: true };

	test('opens nothing until asked, and asks for the preamble first', async () => {
		const s = fakeServer();
		stop = () => s.server.stop(true);
		const { r, h } = recorder();
		const link = new Link(s.url, embed, h);
		link.start();
		await sleep(100);
		expect(s.conns.length).toBe(0);
		expect(link.request()).toBe(true);
		expect(r.asked).toBe(1);
		expect(link.request()).toBe(false);
		expect(r.asked).toBe(1);
		expect(s.conns.length).toBe(0);
	});

	test("the preamble's messages are the first sent, before anything of starfold's", async () => {
		const s = fakeServer();
		stop = () => s.server.stop(true);
		const { r, h } = recorder();
		const link = new Link(s.url, embed, h);
		link.request();
		link.preamble(['ticket-1', 'ticket-2']);
		await until(() => (s.conns[0]?.got.length ?? 0) >= 3);
		expect(s.conns[0].got).toEqual(['ticket-1', 'ticket-2', 'client-first']);
		expect(r.opened).toBe(1);
	});

	test('the URL is used exactly as given, with the subprotocols asked for', async () => {
		const s = fakeServer();
		stop = () => s.server.stop(true);
		const { h } = recorder();
		const link = new Link(
			s.url,
			{ embed: true, protocols: ['starfold.v1'] },
			h
		);
		link.request();
		link.preamble([]);
		await until(() => s.conns.length === 1 && !!s.conns[0].ws);
		expect(s.conns[0].path).toBe('/desk/ws');
		expect(s.conns[0].protocols).toBe('starfold.v1');
	});

	test('a close reaches the embedder with its code and reason, and nothing reconnects', async () => {
		const s = fakeServer();
		stop = () => s.server.stop(true);
		const { r, h } = recorder();
		const link = new Link(s.url, embed, h, WebSocket, 50);
		link.request();
		link.preamble(['ticket']);
		await until(() => !!s.conns[0]?.ws && r.opened === 1);
		s.conns[0].ws!.close(4401, 'ticket expired');
		await until(() => r.closed.length === 1);
		expect(r.closed[0]).toEqual([4401, 'ticket expired']);
		await sleep(300);
		expect(s.conns.length).toBe(1);
		// The embedder connects again: the preamble is asked for again.
		expect(link.request()).toBe(true);
		expect(r.asked).toBe(2);
		link.preamble(['ticket-again']);
		await until(() => (s.conns[1]?.got.length ?? 0) >= 1);
		expect(s.conns[1].got[0]).toBe('ticket-again');
	});

	test('a failed preamble opens nothing, and the next connect asks again', async () => {
		const s = fakeServer();
		stop = () => s.server.stop(true);
		const { r, h } = recorder();
		const link = new Link(s.url, embed, h);
		link.request();
		link.refused();
		await sleep(100);
		expect(s.conns.length).toBe(0);
		expect(link.request()).toBe(true);
		expect(r.asked).toBe(2);
	});
});

describe("the served page's link is unchanged", () => {
	test('it opens at once with no preamble and opens again after a close', async () => {
		const s = fakeServer();
		stop = () => s.server.stop(true);
		const { r, h } = recorder();
		const link = new Link(s.url, { embed: false }, h, WebSocket, 50);
		link.start();
		await until(() => (s.conns[0]?.got.length ?? 0) >= 1);
		expect(s.conns[0].got[0]).toBe('client-first');
		expect(r.asked).toBe(0);
		s.conns[0].ws!.close(1000, 'bye');
		await until(() => s.conns.length === 2, 2000);
		expect(link.request()).toBe(false);
	});

	test('it makes its socket from the URL alone and waits a second to reopen', () => {
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
		expect(RETRY_MS).toBe(1000);
	});
});

describe('answerPreamble', () => {
	const run = async (p: Parameters<typeof answerPreamble>[0]) => {
		const posted: ToWorker[] = [];
		const failed: unknown[] = [];
		await answerPreamble(
			p,
			(m) => posted.push(m),
			(e) => failed.push(e)
		);
		return { posted, failed };
	};

	test('posts the messages a sync or async preamble gives', async () => {
		expect((await run(() => ['a', 'b'])).posted).toEqual([
			{ t: 'preamble', msgs: ['a', 'b'] },
		]);
		expect((await run(async () => 'ticket')).posted).toEqual([
			{ t: 'preamble', msgs: ['ticket'] },
		]);
		expect((await run(undefined)).posted).toEqual([
			{ t: 'preamble', msgs: [] },
		]);
	});

	test('a throwing, rejecting or non-text preamble opens nothing and reports', async () => {
		const boom = new Error('no ticket');
		for (const p of [
			() => {
				throw boom;
			},
			() => Promise.reject(boom),
			() => [1] as unknown as string[],
		]) {
			const { posted, failed } = await run(p);
			expect(posted).toEqual([{ t: 'preambleFailed' }]);
			expect(failed.length).toBe(1);
		}
	});
});
