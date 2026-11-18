// The production worker (src/worker.ts) in a real Worker, driven over its lifecycle port by the
// page's PageLife, against a real WebSocket server: the worker's own close and open posts, its
// handling of what the server sends, and termination with a preamble reply in transit.
import { afterEach, describe, expect, test } from 'bun:test';
import { PageLife } from '../src/embed/attempts';
import type { LifeOptions } from '../src/embed/attempts';
import { PROTOCOL_ERROR } from '../src/embed/types';
import type { Status } from '../src/embed/types';
import { fakeServer, sleep, until } from './server';

const WORKER = new URL('../src/worker.ts', import.meta.url).href;
const PRELOAD = new URL('./worker-preload.ts', import.meta.url).href;

let cleanup: (() => void)[] = [];
afterEach(() => {
	for (const f of cleanup.toReversed()) f();
	cleanup = [];
});

/** The real worker on the lifecycle port, as mount() starts it, and the page's side. */
function start(o: LifeOptions, onReplyPosted?: (w: Worker) => void) {
	const s = fakeServer();
	const worker = new Worker(WORKER, { preload: [PRELOAD] } as WorkerOptions);
	const ch = new MessageChannel();
	const seen = {
		status: [] as Status[],
		closes: [] as [number, string][],
	};
	if (onReplyPosted) {
		const post = ch.port1.postMessage.bind(ch.port1);
		ch.port1.postMessage = (m: { t: string }, t?: Transferable[]) => {
			post(m, t ?? []);
			if (m.t === 'preamble') onReplyPosted(worker);
		};
	}
	const page = new PageLife(ch.port1, {
		...o,
		onStatus: (st) => seen.status.push(st),
		onClose: (c, r) => seen.closes.push([c, r]),
	});
	worker.postMessage(
		{
			t: 'init',
			canvas: {},
			url: s.url,
			presentToMain: false,
			embed: { life: ch.port2 },
		},
		[ch.port2]
	);
	cleanup.push(s.stop, () => worker.terminate());
	return { s, page, seen, worker };
}

describe('the production worker', () => {
	test("sends the preamble first, posts open, and posts the server's close with its code and reason", async () => {
		const { s, page, seen } = start({ preamble: () => ['t1', 't2'] });
		page.connect();
		await until(() => (s.conns[0]?.got.length ?? 0) >= 2, 5000);
		expect(s.conns[0].got.slice(0, 2)).toEqual(['t1', 't2']);
		await until(() => seen.status.includes('open'));
		s.conns[0].ws!.close(4401, 'ticket expired');
		await until(() => seen.closes.length === 1);
		expect(seen.closes).toEqual([[4401, 'ticket expired']]);
	});

	test('a server message that is JSON null ends the attempt with PROTOCOL_ERROR', async () => {
		const { s, page, seen } = start({ preamble: () => 'ticket' });
		page.connect();
		await until(() => !!s.conns[0]?.ws && seen.status.includes('open'), 5000);
		s.conns[0].ws!.send('null');
		await until(() => seen.closes.length === 1);
		expect(seen.closes).toEqual([[PROTOCOL_ERROR.code, PROTOCOL_ERROR.reason]]);
		await until(() => s.conns[0].closed);
	});

	test('a malformed video offer ends the attempt with PROTOCOL_ERROR', async () => {
		const { s, page, seen } = start({ preamble: () => 'ticket' });
		page.connect();
		await until(() => !!s.conns[0]?.ws && seen.status.includes('open'), 5000);
		s.conns[0].ws!.send(
			JSON.stringify({
				t: 'hello',
				protocol: 2,
				w: 640,
				h: 360,
				videoOffers: [null],
			})
		);
		await until(() => seen.closes.length === 1, 5000);
		expect(seen.closes).toEqual([[PROTOCOL_ERROR.code, PROTOCOL_ERROR.reason]]);
	});

	test('terminated in the task that posted the preamble reply, it opens no socket', async () => {
		let replied = 0;
		const { s, page } = start({ preamble: () => 'ticket' }, (w) => {
			replied++;
			w.terminate();
		});
		page.connect();
		await until(() => replied === 1, 5000);
		await sleep(1500);
		expect(s.conns.length).toBe(0);
	});
});
