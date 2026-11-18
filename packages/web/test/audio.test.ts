import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import type { App } from '../src/client/app';
import { createAudio } from '../src/client/audio';

/** Audio contexts made, and what was done with them. */
let contexts: FakeContext[] = [];
/** Whether the browser lets a context run now (autoplay, or a gesture it counts). */
let allowed = true;
/** Worklet loads not finished yet: resolve one to finish it. */
let loads: (() => void)[] = [];
const g = globalThis as unknown as Record<string, unknown>;
const saved: Record<string, unknown> = {};

class FakeContext {
	state: 'suspended' | 'running' = allowed ? 'running' : 'suspended';
	resumes = 0;
	suspends = 0;
	destination = {};
	audioWorklet = {
		addModule: () => new Promise<void>((done) => loads.push(done)),
	};
	constructor() {
		contexts.push(this);
	}
	async resume() {
		this.resumes++;
		if (allowed) this.state = 'running';
	}
	async suspend() {
		this.suspends++;
		this.state = 'suspended';
	}
}

class FakeNode {
	port = { postMessage: () => {} };
	connect() {}
}

beforeEach(() => {
	contexts = [];
	loads = [];
	allowed = true;
	for (const k of ['AudioContext', 'AudioWorkletNode', 'AudioDecoder'])
		saved[k] = g[k];
	g.AudioContext = FakeContext;
	g.AudioWorkletNode = FakeNode;
	g.AudioDecoder = {};
});

afterEach(() => {
	for (const [k, v] of Object.entries(saved)) g[k] = v;
});

/** The audio module with an app that records what it posts to the worker. */
function setup() {
	const posted: unknown[] = [];
	const app = { post: (m: unknown) => posted.push(m) } as unknown as App;
	return { audio: createAudio(app), posted };
}

const settle = () => new Promise((r) => setTimeout(r, 0));

/** Finishes every worklet load started so far. */
async function loaded() {
	for (const done of loads.splice(0)) done();
	await settle();
}

const hello = (audio: boolean) => ({ t: 'hello' as const, audio });
const closed = { t: 'status' as const, s: 'closed' };
const opened = { t: 'status' as const, s: 'open' };

describe('playback follows the last hello', () => {
	test('nothing is loaded before a hello, gesture or not', async () => {
		const { audio, posted } = setup();
		await audio.start();
		await settle();
		expect(contexts.length).toBe(0);
		expect(posted.length).toBe(0);
	});

	test('a hello without audio loads nothing, before or after a gesture', async () => {
		const { audio, posted } = setup();
		await audio.start();
		audio.follow(hello(false));
		await audio.start();
		await settle();
		expect(contexts.length).toBe(0);
		expect(posted.length).toBe(0);
	});

	test('a hello with audio sets playback up once', async () => {
		const { audio, posted } = setup();
		audio.follow(hello(true));
		await loaded();
		await audio.start();
		audio.follow(hello(true));
		await loaded();
		expect(contexts.length).toBe(1);
		expect(posted).toEqual([expect.objectContaining({ t: 'audioPort' })]);
		expect(contexts[0].state).toBe('running');
	});

	test('a reconnect whose hello has no audio leaves playback suspended', async () => {
		const { audio } = setup();
		audio.follow(hello(true));
		await loaded();
		audio.follow(closed);
		await settle();
		expect(contexts[0].state).toBe('suspended');
		const resumes = contexts[0].resumes;
		audio.follow(opened);
		audio.follow(hello(false));
		await audio.start();
		await settle();
		expect(contexts[0].state).toBe('suspended');
		expect(contexts[0].resumes).toBe(resumes);
	});

	test('a reconnect whose hello has audio starts it', async () => {
		const { audio } = setup();
		audio.follow(hello(false));
		audio.follow(closed);
		audio.follow(opened);
		audio.follow(hello(true));
		await loaded();
		expect(contexts.length).toBe(1);
		expect(contexts[0].state).toBe('running');
	});

	test('a gesture while reconnecting resumes nothing on the old hello', async () => {
		allowed = false;
		const { audio } = setup();
		audio.follow(hello(true));
		await loaded();
		expect(contexts[0].state).toBe('suspended');
		audio.follow(closed);
		await settle();
		const resumes = contexts[0].resumes;
		allowed = true;
		await audio.start();
		await settle();
		expect(contexts[0].resumes).toBe(resumes);
		expect(contexts[0].state).toBe('suspended');
		audio.follow(hello(true));
		await settle();
		expect(contexts[0].state).toBe('running');
	});

	test('a worklet that finishes loading after a hello without audio does not resume', async () => {
		const { audio } = setup();
		audio.follow(hello(true));
		await settle();
		expect(loads.length).toBe(1);
		audio.follow(hello(false));
		await settle();
		expect(contexts[0].state).toBe('suspended');
		const resumes = contexts[0].resumes;
		await loaded();
		expect(contexts[0].state).toBe('suspended');
		expect(contexts[0].resumes).toBe(resumes);
	});

	test('a worklet that finishes loading after a disconnect does not resume', async () => {
		allowed = false;
		const { audio } = setup();
		audio.follow(hello(true));
		await settle();
		audio.follow(closed);
		allowed = true;
		await loaded();
		expect(contexts[0].state).toBe('suspended');
	});
});
