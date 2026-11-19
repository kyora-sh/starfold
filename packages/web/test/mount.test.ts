import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { mount } from '../src/embed/index';
import type { MountOptions } from '../src/embed/index';
import { STRIPE } from '../src/embed/styles';
import { UNMOUNTED, WORKER_FAILED } from '../src/embed/types';
import type { LifeDown } from '../src/life';
import {
	FakeEl,
	FakeGenerator,
	FakeResizeObserver,
	FakeWorker,
	container,
	install,
} from './fakedom';
import type { Env } from './fakedom';
import { sleep, until } from './server';

let env: Env;
beforeEach(() => {
	env = install();
});
afterEach(() => env.restore());

/** Whether the test is inside one of the client's own calls (mount/connect/unmount). */
let inside = false;
function guarded<T>(f: () => T): T {
	inside = true;
	try {
		return f();
	} finally {
		inside = false;
	}
}

/** Options whose callbacks record what ran, and whether it ran inside a client call. */
function recording(extra: Partial<MountOptions> = {}) {
	const calls: { what: string; inside: boolean }[] = [];
	const o: MountOptions = {
		connect: { url: 'wss://example.test/ws', preamble: () => 'ticket' },
		onStatus: (s) => calls.push({ what: `status:${s}`, inside }),
		onClose: (c, r) => calls.push({ what: `close:${c}:${r}`, inside }),
		...extra,
	};
	return { o, calls };
}

/** The worker's side of the lifecycle port: what the page posted, and a way to answer. */
function workerSide(w: FakeWorker) {
	const got: LifeDown[] = [];
	const port = w.life!;
	port.addEventListener('message', (e: MessageEvent<LifeDown>) =>
		got.push(e.data)
	);
	port.start();
	return { got, port };
}

const tick = () => sleep(0);

/** The input commands the page posted to the worker. */
const sends = (w: FakeWorker) =>
	w.posted
		.filter((m) => m.t === 'send')
		.map((m) => (m as unknown as { d: string }).d);

/** A Worker constructor that throws, as a cross-origin script URL makes it. */
function crossOriginWorker(): never {
	throw new DOMException('cross-origin', 'SecurityError');
}

async function opened(w: FakeWorker) {
	const side = workerSide(w);
	await until(() => side.got.some((m) => m.t === 'connect'));
	side.port.postMessage({ t: 'preamble', id: 1 }, []);
	await until(() => side.got.some((m) => m.t === 'preamble'));
	side.port.postMessage({ t: 'open', id: 1 }, []);
	return side;
}

describe('callbacks run only from the queued dispatcher', () => {
	test('none runs inside mount, connect or unmount', async () => {
		const { o, calls } = recording();
		const h = guarded(() => mount(container() as never, o));
		const w = FakeWorker.all[0];
		await opened(w);
		await until(() => calls.some((c) => c.what === 'status:open'));
		guarded(() => h.connect());
		guarded(() => h.unmount());
		await tick();
		expect(calls.map((c) => c.what)).toEqual([
			'status:connecting',
			'status:open',
			`close:${UNMOUNTED.code}:${UNMOUNTED.reason}`,
		]);
		expect(calls.filter((c) => c.inside)).toEqual([]);
	});

	test('a throwing callback is reported and changes nothing; mount still returns its handle', async () => {
		const { o, calls } = recording({
			onStatus: (s) => {
				calls.push({ what: `status:${s}`, inside });
				throw new Error(`host bug on ${s}`);
			},
		});
		const h = mount(container() as never, o);
		expect(typeof h.unmount).toBe('function');
		await opened(FakeWorker.all[0]);
		await until(() => calls.some((c) => c.what === 'status:open'));
		expect(env.reported.map((e) => (e as Error).message)).toEqual([
			'host bug on connecting',
			'host bug on open',
		]);
		h.unmount();
		await tick();
		expect(calls.at(-1)!.what).toBe(
			`close:${UNMOUNTED.code}:${UNMOUNTED.reason}`
		);
	});
});

describe('unmount ends everything at once', () => {
	test('the worker is terminated before unmount returns; afterwards only the unmounted close arrives', async () => {
		let asked = 0;
		const { o, calls } = recording({
			connect: {
				url: 'wss://example.test/ws',
				preamble: () => {
					asked++;
					return 'ticket';
				},
			},
		});
		const h = mount(container() as never, o);
		const w = FakeWorker.all[0];
		const side = await opened(w);
		await until(() => calls.some((c) => c.what === 'status:open'));
		const before = calls.length;
		side.port.postMessage(
			{ t: 'closed', id: 1, code: 4401, reason: 'late' },
			[]
		);
		h.unmount();
		expect(w.terminated).toBe(true);
		await sleep(50);
		side.port.postMessage({ t: 'preamble', id: 1 }, []);
		w.emit({ t: 'hello', w: 1280, h: 720 });
		h.connect();
		await sleep(50);
		expect(calls.slice(before).map((c) => c.what)).toEqual([
			`close:${UNMOUNTED.code}:${UNMOUNTED.reason}`,
		]);
		expect(asked).toBe(1);
		expect(side.got.filter((m) => m.t === 'connect').length).toBe(1);
	});

	test('an unmount while the preamble is pending reports nothing at all', async () => {
		const { o, calls } = recording({
			connect: { url: 'wss://x/ws', preamble: () => new Promise(() => {}) },
		});
		const h = mount(container() as never, o);
		const side = workerSide(FakeWorker.all[0]);
		await until(() => side.got.length === 1);
		side.port.postMessage({ t: 'preamble', id: 1 }, []);
		await tick();
		h.unmount();
		await sleep(50);
		expect(calls.map((c) => c.what)).toEqual(['status:connecting']);
	});

	test('everything mount set up is undone', async () => {
		const { o } = recording();
		const box = container();
		const h = mount(box as never, o);
		const w = FakeWorker.all[0];
		await opened(w);
		w.emit({
			t: 'hello',
			w: 1280,
			h: 720,
			stripes: [
				[0, 368],
				[368, 352],
			],
		});
		w.emit({
			t: 'frames',
			parts: [0, 1].map((i) => ({ i, frame: { close() {} } })),
		});
		const surface = box.children[0];
		expect(FakeGenerator.all.length).toBe(2);
		// A wheel turn queues a scroll frame; a touch starts the long-press timer.
		surface.dispatchEvent(
			Object.assign(new Event('wheel', { cancelable: true }), {
				deltaX: 0,
				deltaY: 40,
				deltaMode: 0,
				clientX: 1,
				clientY: 1,
			})
		);
		surface.dispatchEvent(
			Object.assign(new Event('pointerdown', { cancelable: true }), {
				pointerType: 'touch',
				pointerId: 1,
				clientX: 5,
				clientY: 5,
				button: 0,
			})
		);
		expect(env.frames.size).toBeGreaterThan(0);
		expect(env.window.active.size).toBeGreaterThan(0);
		expect(env.document.active.size).toBeGreaterThan(0);
		const closedBefore = env.portsClosed;

		h.unmount();

		expect(w.terminated).toBe(true);
		expect(w.active.size).toBe(0);
		expect(env.window.active.size).toBe(0);
		expect(env.document.active.size).toBe(0);
		expect(env.frames.size).toBe(0);
		expect(FakeResizeObserver.all.every((r) => r.disconnected)).toBe(true);
		expect(FakeGenerator.all.every((g) => g.stopped)).toBe(true);
		expect(box.children).toEqual([]);
		expect(env.portsClosed).toBeGreaterThan(closedBefore);
		await sleep(700);
		expect(env.vibrated).toBe(0);
	});

	test('the removed elements keep no listener: a touch on a kept surface starts no long press', async () => {
		const { o } = recording();
		const box = container();
		const h = mount(box as never, o);
		const w = FakeWorker.all[0];
		await opened(w);
		const [surface, kbd] = box.children;

		h.unmount();

		expect(surface.active.size).toBe(0);
		expect(kbd.active.size).toBe(0);
		surface.dispatchEvent(
			Object.assign(new Event('pointerdown', { cancelable: true }), {
				pointerType: 'touch',
				pointerId: 1,
				clientX: 5,
				clientY: 5,
				button: 0,
			})
		);
		await sleep(700);
		expect(env.vibrated).toBe(0);
		expect(w.late).toEqual([]);
	});

	test('an image paste whose bytes arrive after unmount posts nothing', async () => {
		const { o } = recording();
		const box = container();
		const h = mount(box as never, o);
		const w = FakeWorker.all[0];
		await opened(w);
		const kbd = box.children[1];
		let bytes: ((b: ArrayBuffer) => void) | undefined;
		const file = {
			type: 'image/png',
			arrayBuffer: () => new Promise<ArrayBuffer>((r) => (bytes = r)),
		};
		kbd.dispatchEvent(
			Object.assign(new Event('paste', { cancelable: true }), {
				clipboardData: {
					getData: () => '',
					items: [{ kind: 'file', type: 'image/png', getAsFile: () => file }],
				},
			})
		);

		expect(bytes).toBeDefined();
		h.unmount();
		bytes?.(new Uint8Array([1, 2, 3]).buffer);
		await sleep(50);

		expect(w.late).toEqual([]);
	});
});

describe('mount', () => {
	test('a worker that fails to load ends the attempt with connect-failed', async () => {
		const { o, calls } = recording();
		mount(container() as never, o);
		const w = FakeWorker.all[0];
		await tick();
		w.dispatchEvent(new Event('error', { cancelable: true }));
		await tick();
		expect(calls.map((c) => c.what)).toEqual([
			'status:connecting',
			'status:connect-failed',
		]);
	});

	test('a worker error while streaming stops the worker and reports one WORKER_FAILED close', async () => {
		const { o, calls } = recording();
		const h = mount(container() as never, o);
		const w = FakeWorker.all[0];
		const side = await opened(w);
		await until(() => calls.some((c) => c.what === 'status:open'));
		w.dispatchEvent(new Event('error', { cancelable: true }));
		await tick();
		expect(w.terminated).toBe(true);
		side.port.postMessage({ t: 'closed', id: 1, code: 1006, reason: '' }, []);
		await tick();
		h.unmount();
		await tick();
		expect(calls.map((c) => c.what)).toEqual([
			'status:connecting',
			'status:open',
			`close:${WORKER_FAILED.code}:${WORKER_FAILED.reason}`,
			'status:closed',
		]);
	});

	test('a top-level page keeps its focus at mount; a pointer on the surface takes it', async () => {
		const box = container();
		const h = mount(box as never, recording().o);
		const [surface, kbd] = box.children;
		expect(kbd.focused).toBe(0);
		surface.dispatchEvent(
			Object.assign(new Event('pointerdown', { cancelable: true }), {
				pointerType: 'mouse',
				pointerId: 1,
				button: 0,
				clientX: 1,
				clientY: 1,
			})
		);
		expect(kbd.focused).toBe(1);
		h.unmount();
		await tick();
	});

	test('keys held when focus leaves the keyboard field are released', async () => {
		const box = container();
		const h = mount(box as never, recording().o);
		const w = FakeWorker.all[0];
		await opened(w);
		const kbd = box.children[1];
		kbd.dispatchEvent(
			Object.assign(new Event('keydown', { cancelable: true }), {
				code: 'KeyA',
				key: 'a',
			})
		);
		const before = sends(w).length;
		kbd.dispatchEvent(new Event('blur'));
		expect(sends(w).slice(before)).toEqual(['k,97,0']);
		h.unmount();
		await tick();
	});

	test('a paste shortcut still pending when focus leaves the keyboard field is dropped', async () => {
		const box = container();
		const h = mount(box as never, recording().o);
		const w = FakeWorker.all[0];
		await opened(w);
		const kbd = box.children[1];
		const key = (type: string, code: string, k: string) =>
			kbd.dispatchEvent(
				Object.assign(new Event(type, { cancelable: true }), {
					code,
					key: k,
					ctrlKey: true,
					metaKey: true,
				})
			);
		key('keydown', 'ControlLeft', 'Control');
		key('keydown', 'MetaLeft', 'Meta');
		key('keydown', 'KeyV', 'v');
		kbd.dispatchEvent(new Event('blur'));
		await sleep(300);
		expect(sends(w).filter((s) => s.startsWith('k,118,'))).toEqual([]);
		h.unmount();
		await tick();
	});

	test('a touch tap on the surface focuses the keyboard field', async () => {
		const box = container();
		const h = mount(box as never, recording().o);
		const [surface, kbd] = box.children;
		expect(kbd.focused).toBe(0);
		for (const type of ['pointerdown', 'pointerup'])
			surface.dispatchEvent(
				Object.assign(new Event(type, { cancelable: true }), {
					pointerType: 'touch',
					pointerId: 1,
					button: 0,
					clientX: 5,
					clientY: 5,
				})
			);
		expect(kbd.focused).toBe(1);
		h.unmount();
		await tick();
	});

	test('a worker that cannot be created leaves nothing in the container', () => {
		const g = globalThis as unknown as Record<string, unknown>;
		g.Worker = crossOriginWorker;
		const box = container();
		expect(() => mount(box as never, recording().o)).toThrow('cross-origin');
		expect(box.children).toEqual([]);
		expect(env.window.active.size).toBe(0);
	});

	test('a container smaller than the smallest seam-free picture still holds it', () => {
		const box = container(160, 90);
		mount(box as never, recording().o);
		FakeWorker.all[0].emit({
			t: 'hello',
			w: 1920,
			h: 1080,
			stripes: [
				[0, 544],
				[544, 536],
			],
		});
		const surface = box.children[0];
		expect(parseFloat(surface.style.width)).toBeLessThanOrEqual(160);
		expect(parseFloat(surface.style.height)).toBeLessThanOrEqual(90);
	});

	test("the stripe <video>s get the page's .stripe rule through createVideo", () => {
		const box = container();
		mount(box as never, recording().o);
		const w = FakeWorker.all[0];
		w.emit({ t: 'hello', w: 1280, h: 720, stripes: [[0, 720]] });
		w.emit({ t: 'frames', parts: [{ i: 0, frame: { close() {} } }] });
		const videos = box.children[0].children.filter(
			(c: FakeEl) => c.tagName === 'video'
		);
		expect(videos.length).toBe(1);
		for (const [k, v] of Object.entries(STRIPE))
			expect(videos[0].style[k]).toBe(v as string);
	});
});
