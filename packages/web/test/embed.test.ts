import { afterEach, describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { PageLife } from '../src/embed/attempts';
import type { LifeOptions } from '../src/embed/attempts';
import { Dispatcher } from '../src/embed/dispatch';
import { CONNECT_FAILED, callPreamble } from '../src/embed/preamble';
import { Reconnector } from '../src/embed/reconnect';
import type { Env } from '../src/embed/reconnect';
import { onStream } from '../src/embed/stream';
import type { StreamSink } from '../src/embed/stream';
import {
	KBD,
	SCREEN,
	STRIPE,
	SURFACE,
	assetBaseUrl,
} from '../src/embed/styles';
import { UNMOUNTED } from '../src/embed/types';
import type {
	ConnectFailure,
	ReconnectEvent,
	ReconnectOptions,
	Status,
} from '../src/embed/types';
import { serveLife } from '../src/life';
import type { LifeUp } from '../src/life';
import { Link } from '../src/link';
import type { FromWorker } from '../src/protocol';
import { fakeServer, sleep, until } from './server';

const SECRET = 'SECRET-TICKET-7f3a';

/** An array of `n` holes. */
function sparse(n: number): string[] {
	const a: string[] = [];
	a.length = n;
	return a;
}

/** A promise the test settles. */
function deferred<T>() {
	let resolve!: (v: T) => void;
	let reject!: (e: unknown) => void;
	const promise = new Promise<T>((a, b) => {
		resolve = a;
		reject = b;
	});
	return { promise, resolve, reject };
}

/**
 * The page and the worker's lifecycle over a real MessageChannel, wired as worker.ts wires
 * them (the worker's Link handlers post LifeUp; serveLife takes LifeDown), against a real
 * server. Server messages go to `stream`, the worker's ordinary channel.
 */
function embedded(
	url: string,
	o: LifeOptions,
	Socket: typeof WebSocket = WebSocket
) {
	const seen = {
		status: [] as [Status, ConnectFailure | undefined][],
		closes: [] as [number, string][],
		stream: [] as string[],
	};
	const ch = new MessageChannel();
	let sock: WebSocket | null = null;
	const up = (m: LifeUp) => ch.port2.postMessage(m);
	const link = new Link(
		url,
		{ embed: true },
		{
			opening(s) {
				sock = s;
			},
			open(id) {
				up({ t: 'open', id });
				sock!.send('client-first');
			},
			message(d) {
				if (typeof d === 'string') seen.stream.push(d);
			},
			closed(id, code, reason) {
				up({ t: 'closed', id, code, reason });
			},
			askPreamble(id) {
				up({ t: 'preamble', id });
			},
			connectFailed(id) {
				up({ t: 'connectFailed', id });
			},
		},
		Socket
	);
	serveLife(ch.port2, link);
	const page = new PageLife(ch.port1, {
		...o,
		onStatus(s, d) {
			seen.status.push([s, d]);
			o.onStatus?.(s, d);
		},
		onClose(code, reason) {
			seen.closes.push([code, reason]);
			o.onClose?.(code, reason);
		},
	});
	return {
		page,
		link,
		seen,
		/** As mount's unmount(): the worker stops (here: its link is cancelled), then end(). */
		unmount: () => {
			link.cancel();
			page.end();
		},
		close: () => {
			ch.port1.close();
			ch.port2.close();
		},
	};
}

let cleanup: (() => void)[] = [];
afterEach(() => {
	for (const f of cleanup) f();
	cleanup = [];
});
function setup(o: LifeOptions, Socket?: typeof WebSocket) {
	const s = fakeServer();
	const e = embedded(s.url, o, Socket);
	cleanup.push(e.close, s.stop);
	return { s, ...e };
}

const statuses = (seen: { status: [Status, unknown][] }) =>
	seen.status.map(([s]) => s);

describe('the lifecycle through the page and worker ports', () => {
	test('the preamble is the first thing the server gets, then open is reported', async () => {
		const { s, page, seen } = setup({ preamble: async () => ['t1', 't2'] });
		page.connect();
		await until(() => (s.conns[0]?.got.length ?? 0) >= 3);
		expect(s.conns[0].got).toEqual(['t1', 't2', 'client-first']);
		await until(() => statuses(seen).includes('open'));
		expect(statuses(seen)).toEqual(['connecting', 'open']);
	});

	test('server text shaped like lifecycle messages does nothing lifecycle-ish', async () => {
		let calls = 0;
		const { s, page, seen } = setup({
			preamble: () => {
				calls++;
				return 'ticket';
			},
		});
		page.connect();
		await until(() => !!s.conns[0]?.ws && statuses(seen).includes('open'));
		for (const m of [
			{ t: 'preamble', id: 1 },
			{ t: 'closed', id: 1, code: 4401, reason: SECRET },
			{ t: 'status', s: 'closed', code: 4401, reason: SECRET },
			{ t: 'connectFailed', id: 1 },
			{ t: 'log', s: SECRET },
		])
			s.conns[0].ws!.send(JSON.stringify(m));
		await until(() => seen.stream.length === 5);
		await sleep(100);
		expect(calls).toBe(1);
		expect(seen.closes).toEqual([]);
		expect(statuses(seen)).toEqual(['connecting', 'open']);
		expect(s.conns.length).toBe(1);
	});

	test('the page channel acts only on stream messages: no lifecycle, no log', () => {
		const hits: string[] = [];
		const sink = new Proxy({} as StreamSink, {
			get: (_t, k) => () => hits.push(String(k)),
		});
		for (const m of [
			{ t: 'preamble' },
			{ t: 'status', s: 'closed', code: 4401, reason: SECRET },
			{ t: 'log', s: SECRET },
			{ t: 'closed', id: 1, code: 1000, reason: '' },
		])
			onStream(m as unknown as FromWorker, sink);
		expect(hits).toEqual([]);
		onStream({ t: 'surface', s: 'canvas' }, sink);
		expect(hits).toEqual(['surface']);
	});

	test("a close reaches onClose once with the server's code and reason, and nothing reconnects", async () => {
		let calls = 0;
		const { s, page, seen } = setup({
			preamble: () => `ticket-${++calls}`,
		});
		page.connect();
		await until(() => !!s.conns[0]?.ws && statuses(seen).includes('open'));
		s.conns[0].ws!.close(4401, 'ticket expired');
		await until(() => seen.closes.length === 1);
		expect(seen.closes).toEqual([[4401, 'ticket expired']]);
		expect(statuses(seen).at(-1)).toBe('closed');
		await sleep(1500);
		expect(s.conns.length).toBe(1);
		page.connect();
		await until(() => (s.conns[1]?.got.length ?? 0) >= 1);
		expect(calls).toBe(2);
		expect(s.conns[1].got[0]).toBe('ticket-2');
		expect(seen.closes.length).toBe(1);
	});

	test('a preamble that resolves after unmount opens nothing and reports nothing', async () => {
		const d = deferred<string>();
		const { s, page, seen, unmount } = setup({ preamble: () => d.promise });
		page.connect();
		await sleep(50);
		unmount();
		d.resolve('ticket');
		await sleep(200);
		expect(s.conns.length).toBe(0);
		expect(seen.closes).toEqual([]);
		expect(statuses(seen)).toEqual(['connecting']);
	});

	test('a preamble that rejects after unmount reports nothing', async () => {
		const d = deferred<string>();
		const { page, seen, unmount } = setup({ preamble: () => d.promise });
		page.connect();
		await sleep(50);
		unmount();
		d.reject(new Error(SECRET));
		await sleep(100);
		expect(statuses(seen)).toEqual(['connecting']);
	});

	test("attempt 1's preamble, resolving after attempt 2 began, never reaches attempt 2", async () => {
		const answers = [deferred<string>(), deferred<string>()];
		let n = 0;
		const { s, page } = setup({ preamble: () => answers[n++].promise });
		page.connect();
		await until(() => n === 1);
		page.connect();
		await until(() => n === 2);
		answers[1].resolve('ticket-2');
		await until(() => (s.conns[0]?.got.length ?? 0) >= 1);
		answers[0].resolve('ticket-1');
		await sleep(200);
		expect(s.conns.length).toBe(1);
		expect(s.conns[0].got).toEqual(['ticket-2', 'client-first']);
	});

	test("attempt 1's preamble, resolving first after attempt 2 began, doesn't stop attempt 2", async () => {
		const answers = [deferred<string>(), deferred<string>()];
		let n = 0;
		const { s, page, seen } = setup({ preamble: () => answers[n++].promise });
		page.connect();
		await until(() => n === 1);
		page.connect();
		await until(() => n === 2);
		answers[0].resolve('ticket-1');
		await sleep(100);
		answers[1].resolve('ticket-2');
		await until(() => (s.conns[0]?.got.length ?? 0) >= 2);
		await until(() => statuses(seen).includes('open'));
		await sleep(100);
		expect(s.conns.length).toBe(1);
		expect(s.conns[0].got).toEqual(['ticket-2', 'client-first']);
	});

	test('unmount reports UNMOUNTED once, the socket closes, and the late worker close is dropped', async () => {
		const { s, page, seen, unmount } = setup({ preamble: () => 'ticket' });
		page.connect();
		await until(() => !!s.conns[0]?.ws && statuses(seen).includes('open'));
		const before = seen.status.length;
		unmount();
		expect(seen.closes).toEqual([]);
		await until(() => seen.closes.length === 1);
		expect(seen.closes).toEqual([[UNMOUNTED.code, UNMOUNTED.reason]]);
		expect(UNMOUNTED).toEqual({ code: 1001, reason: 'unmounted' });
		await until(() => s.conns[0].closed);
		await sleep(200);
		expect(seen.closes.length).toBe(1);
		expect(seen.status.length).toBe(before);
		page.connect();
		await sleep(100);
		expect(s.conns.length).toBe(1);
	});

	test('a socket that cannot be made ends the attempt with connect-failed and no onClose', async () => {
		function Throws(): never {
			throw new SyntaxError(`bad url ?t=${SECRET}`);
		}
		const { page, seen } = setup(
			{ preamble: () => 'ticket' },
			Throws as unknown as typeof WebSocket
		);
		page.connect();
		await until(() => statuses(seen).includes('connect-failed'));
		expect(seen.status.at(-1)).toEqual(['connect-failed', CONNECT_FAILED]);
		expect(JSON.stringify(seen.status)).not.toContain(SECRET);
		expect(seen.closes).toEqual([]);
		page.connect();
		await until(
			() => statuses(seen).filter((x) => x === 'connecting').length === 2
		);
	});
});

describe('credentials never reach a callback', () => {
	const bad: [string, () => unknown][] = [
		[
			'throws',
			() => {
				throw new Error(SECRET);
			},
		],
		['rejects', () => Promise.reject(new Error(SECRET))],
		['rejects a string', () => Promise.reject(SECRET)],
		['gives a non-string', () => [SECRET, 1]],
		['gives a sparse array', () => sparse(2)],
	];
	for (const [name, preamble] of bad)
		test(`a preamble that ${name} opens nothing, and the report carries none of it`, async () => {
			const { s, page, seen } = setup({
				preamble: preamble as () => string,
			});
			page.connect();
			await until(() => statuses(seen).includes('preamble-failed'));
			await sleep(100);
			const [, detail] = seen.status.at(-1)!;
			expect(detail?.kind).toBe('preamble-failed');
			expect(JSON.stringify(seen)).not.toContain(SECRET);
			expect(String(detail?.message)).not.toContain(SECRET);
			expect(seen.closes).toEqual([]);
			expect(s.conns.length).toBe(0);
		});

	test("the worker holds the preamble's messages only until they are sent", async () => {
		const { s, page, link } = setup({ preamble: () => 'ticket' });
		page.connect();
		await until(() => (s.conns[0]?.got.length ?? 0) >= 1);
		expect(link.holdsPreamble).toBe(false);
	});
});

/** A PageLife on a port the test drives by hand; thrown callback errors go to `reported`. */
function driven(o: LifeOptions) {
	const sent: { t: string }[] = [];
	const reported: unknown[] = [];
	const listeners = new Set<(e: { data: LifeUp }) => void>();
	let closed = false;
	const port = {
		addEventListener: (_t: string, f: (e: { data: LifeUp }) => void) =>
			listeners.add(f),
		removeEventListener: (_t: string, f: (e: { data: LifeUp }) => void) =>
			listeners.delete(f),
		start: () => {},
		close: () => {
			closed = true;
		},
		postMessage: (m: { t: string }) => {
			if (!closed) sent.push(m);
		},
	};
	const life = new PageLife(port as unknown as MessagePort, {
		...o,
		report: (e) => reported.push(e),
	});
	return {
		life,
		sent,
		reported,
		isClosed: () => closed,
		deliver: (m: LifeUp) => {
			for (const f of listeners) f({ data: m });
		},
	};
}

const microtasks = () => sleep(0);

async function toSocket(d: ReturnType<typeof driven>) {
	d.life.connect();
	d.deliver({ t: 'preamble', id: 1 });
	await until(() => d.sent.some((m) => (m as { t: string }).t === 'preamble'));
}

describe('the close is reported once, whatever a callback does', () => {
	test('onClose runs before onStatus("closed"), and a throwing onStatus cannot skip it', async () => {
		const order: string[] = [];
		const d = driven({
			preamble: () => 'ticket',
			onClose: () => order.push('onClose'),
			onStatus: (s) => {
				order.push(s);
				if (s === 'closed') throw new Error('host bug');
			},
		});
		await toSocket(d);
		d.deliver({ t: 'closed', id: 1, code: 4401, reason: 'x' });
		expect(order).toEqual(['connecting']);
		await microtasks();
		expect(order).toEqual(['connecting', 'onClose', 'closed']);
		expect(d.reported.map((e) => (e as Error).message)).toEqual(['host bug']);
	});

	test('a throwing onClose still gets onStatus("closed"), and the attempt is over', async () => {
		const order: string[] = [];
		const d = driven({
			preamble: () => 'ticket',
			onClose: () => {
				order.push('onClose');
				throw new Error('host bug');
			},
			onStatus: (s) => order.push(s),
		});
		await toSocket(d);
		d.deliver({ t: 'closed', id: 1, code: 1000, reason: '' });
		d.deliver({ t: 'closed', id: 1, code: 1000, reason: '' });
		await microtasks();
		expect(order).toEqual(['connecting', 'onClose', 'closed']);
		expect(d.reported.length).toBe(1);
	});

	test('end() while a socket was asked for reports UNMOUNTED once, drops queued callbacks and later closes', async () => {
		const calls: string[] = [];
		const d = driven({
			preamble: () => 'ticket',
			onClose: (c, r) => calls.push(`close:${c}:${r}`),
			onStatus: (s) => calls.push(s),
		});
		await toSocket(d);
		await microtasks();
		d.deliver({ t: 'open', id: 1 });
		d.life.end();
		d.life.end();
		d.deliver({ t: 'closed', id: 1, code: 1000, reason: '' });
		await microtasks();
		expect(calls).toEqual(['connecting', 'close:1001:unmounted']);
		expect(d.isClosed()).toBe(true);
	});
});

describe('one attempt at a time on the port', () => {
	test('once a reply is posted, connect() posts nothing until that attempt closes', async () => {
		const d = driven({ preamble: () => 'ticket' });
		await toSocket(d);
		d.life.connect();
		d.life.connect();
		expect(d.sent.map((m) => m.t)).toEqual(['connect', 'preamble']);
		d.deliver({ t: 'closed', id: 1, code: 1000, reason: '' });
		d.life.connect();
		expect(d.sent.map((m) => m.t)).toEqual(['connect', 'preamble', 'connect']);
	});

	test('a worker that fails ends the attempt with connect-failed, and nothing follows', async () => {
		const calls: string[] = [];
		const d = driven({
			preamble: () => 'ticket',
			onStatus: (s) => calls.push(s),
		});
		d.life.connect();
		d.life.workerFailed();
		d.life.connect();
		d.deliver({ t: 'preamble', id: 1 });
		await microtasks();
		expect(calls).toEqual(['connecting', 'connect-failed']);
		expect(d.sent.map((m) => m.t)).toEqual(['connect']);
	});
});

describe('the dispatcher', () => {
	test('queues calls after the caller returns, reports throws and carries on', async () => {
		const reported: unknown[] = [];
		const order: string[] = [];
		const q = new Dispatcher((e) => reported.push(e));
		q.call(() => order.push('a'));
		q.call(() => {
			throw new Error('b');
		});
		q.call(() => order.push('c'));
		expect(order).toEqual([]);
		await microtasks();
		expect(order).toEqual(['a', 'c']);
		expect(reported.map((e) => (e as Error).message)).toEqual(['b']);
	});

	test('seal drops what is queued and runs only the last call', async () => {
		const order: string[] = [];
		const q = new Dispatcher();
		q.call(() => order.push('queued'));
		q.seal(() => order.push('last'));
		q.call(() => order.push('after'));
		await microtasks();
		expect(order).toEqual(['last']);
	});
});

describe('callPreamble', () => {
	test('copies a sync array, an async string, a proxied array, or nothing', async () => {
		expect(await callPreamble(() => ['a', 'b'])).toEqual({
			ok: true,
			msgs: ['a', 'b'],
		});
		expect(await callPreamble(async () => 'ticket')).toEqual({
			ok: true,
			msgs: ['ticket'],
		});
		expect(await callPreamble(undefined)).toEqual({ ok: true, msgs: [] });
		const r = await callPreamble(() => new Proxy(['p1', 'p2'], {}));
		expect(r).toEqual({ ok: true, msgs: ['p1', 'p2'] });
		expect(structuredClone(r)).toEqual(r);
	});

	test('a sparse array is not text', async () => {
		expect(await callPreamble(() => sparse(1))).toEqual({
			ok: false,
			failure: {
				kind: 'preamble-failed',
				cause: 'not-text',
				message:
					'the preamble gave something other than a string or an array of strings',
			},
		});
	});
});

/** index.html's declarations for `selector`, camelCased. */
function rule(selector: string): Record<string, string> {
	const html = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
	const at = html.indexOf(`${selector} {`);
	if (at < 0) throw new Error(`index.html has no ${selector} rule`);
	const body = html.slice(html.indexOf('{', at) + 1, html.indexOf('}', at));
	const out: Record<string, string> = {};
	for (const decl of body.split(';')) {
		const [k, ...v] = decl.split(':');
		if (!k.trim()) continue;
		out[k.trim().replace(/-([a-z])/g, (_m, c: string) => c.toUpperCase())] = v
			.join(':')
			.trim();
	}
	return out;
}

describe('the embed renders without the served page', () => {
	test("the inline styles are the served page's rules", () => {
		expect(SURFACE).toEqual(rule('#surface'));
		expect(SCREEN).toEqual(rule('#screen'));
		expect(STRIPE).toEqual(rule('.stripe'));
		expect(KBD).toEqual(rule('#kbd'));
	});

	test('assetBase is a directory, with or without its trailing slash', () => {
		const mod = 'https://cdn.example/pkg/starfold/index.js';
		expect(assetBaseUrl(undefined, mod).href).toBe(
			'https://cdn.example/pkg/starfold/'
		);
		expect(assetBaseUrl('/static/starfold', mod).href).toBe(
			'https://cdn.example/static/starfold/'
		);
		expect(assetBaseUrl('/static/starfold/', mod).href).toBe(
			'https://cdn.example/static/starfold/'
		);
		expect(
			new URL('worker.js', assetBaseUrl('/static/starfold', mod)).href
		).toBe('https://cdn.example/static/starfold/worker.js');
	});
});

/** Real timers, a visible page that is online, and the delay's jitter at `random`. */
function liveEnv(random: number): Env {
	return {
		now: () => performance.now(),
		setTimeout: (f, ms) => setTimeout(f, ms),
		clearTimeout: (h) => clearTimeout(h as ReturnType<typeof setTimeout>),
		random: () => random,
		hidden: () => false,
		online: () => true,
		onWake: () => () => {},
	};
}

function reconnecting(
	o: LifeOptions,
	r: true | ReconnectOptions = true,
	random = 0
) {
	const events: ReconnectEvent[] = [];
	const e = setup({
		...o,
		reconnect: new Reconnector(r, liveEnv(random)),
		onReconnect: (ev) => events.push(ev),
	});
	return { ...e, events };
}

describe('reconnect, opt-in', () => {
	test('a restart close reconnects with a fresh preamble and reports reconnecting, then reconnected', async () => {
		let calls = 0;
		const { s, page, seen, events } = reconnecting({
			preamble: () => `ticket-${++calls}`,
		});
		page.connect();
		await until(() => !!s.conns[0]?.ws && statuses(seen).includes('open'));
		s.conns[0].ws!.close(1012, 'restarting');
		await until(() => (s.conns[1]?.got.length ?? 0) >= 1);
		expect(s.conns[1].got[0]).toBe('ticket-2');
		await until(() => events.length === 2);
		expect(statuses(seen)).toEqual([
			'connecting',
			'open',
			'reconnecting',
			'connecting',
			'open',
		]);
		expect(seen.closes).toEqual([[1012, 'restarting']]);
		expect(events).toEqual([
			{
				kind: 'reconnecting',
				attempt: 1,
				delayMs: 0,
				code: 1012,
				reason: 'restarting',
			},
			{ kind: 'reconnected', attempt: 1 },
		]);
	});

	test('the same 44xx close twice right after open gives up with that code, then closed', async () => {
		const { s, page, seen, events } = reconnecting({ preamble: () => 't' });
		page.connect();
		for (const i of [0, 1]) {
			await until(() => !!s.conns[i]?.ws && (s.conns[i]?.got.length ?? 0) >= 1);
			await until(() => statuses(seen).filter((x) => x === 'open').length === i + 1);
			s.conns[i].ws!.close(4401, 'ticket rejected');
		}
		await until(() => statuses(seen).at(-1) === 'closed');
		expect(statuses(seen).slice(-2)).toEqual(['gave-up', 'closed']);
		expect(events.at(-1)).toEqual({
			kind: 'gave-up',
			why: 'repeated-close',
			code: 4401,
			reason: 'ticket rejected',
		});
		await sleep(300);
		expect(s.conns.length).toBe(2);
	});

	test('a close not worth another attempt gives up at once', async () => {
		const { s, page, seen, events } = reconnecting({});
		page.connect();
		await until(() => !!s.conns[0]?.ws && statuses(seen).includes('open'));
		s.conns[0].ws!.close(1000, 'done');
		await until(() => statuses(seen).at(-1) === 'closed');
		expect(statuses(seen)).toEqual(['connecting', 'open', 'gave-up', 'closed']);
		expect(events).toEqual([
			{ kind: 'gave-up', why: 'code', code: 1000, reason: 'done' },
		]);
		await sleep(300);
		expect(s.conns.length).toBe(1);
	});

	test('a preamble that fails on a reconnect ends the run: preamble-failed, then gave-up', async () => {
		let calls = 0;
		const { s, page, seen, events } = reconnecting({
			preamble: () => {
				if (++calls > 1) throw new Error(SECRET);
				return 't';
			},
		});
		page.connect();
		await until(() => !!s.conns[0]?.ws && statuses(seen).includes('open'));
		s.conns[0].ws!.close(1012, '');
		await until(() => statuses(seen).at(-1) === 'gave-up');
		expect(statuses(seen).slice(-3)).toEqual([
			'connecting',
			'preamble-failed',
			'gave-up',
		]);
		expect(events.at(-1)).toEqual({
			kind: 'gave-up',
			why: 'preamble-failed',
			code: 1012,
			reason: '',
		});
		expect(JSON.stringify([seen.status, events])).not.toContain(SECRET);
		expect(s.conns.length).toBe(1);
	});

	test("the host's connect() during the delay connects now; unmount during it ends everything", async () => {
		const { s, page, seen, unmount } = reconnecting(
			{ preamble: () => 't' },
			{ initialMs: 60_000 },
			0.99
		);
		page.connect();
		await until(() => !!s.conns[0]?.ws && statuses(seen).includes('open'));
		s.conns[0].ws!.close(1012, '');
		await until(() => statuses(seen).at(-1) === 'reconnecting');
		page.connect();
		await until(() => !!s.conns[1]?.ws && statuses(seen).at(-1) === 'open');
		s.conns[1].ws!.close(1012, '');
		await until(() => statuses(seen).at(-1) === 'reconnecting');
		const before = seen.status.length;
		unmount();
		await sleep(300);
		expect(seen.status.length).toBe(before);
		expect(s.conns.length).toBe(2);
	});
});
