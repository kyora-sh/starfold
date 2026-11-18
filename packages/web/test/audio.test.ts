import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import type { App } from '../src/client/app';
import { createAudio } from '../src/client/audio';

/** Audio contexts made, and the calls made on them. */
let contexts: FakeContext[] = [];
const g = globalThis as unknown as Record<string, unknown>;
const saved: Record<string, unknown> = {};

class FakeContext {
	state: 'suspended' | 'running' = 'suspended';
	resumes = 0;
	suspends = 0;
	destination = {};
	audioWorklet = { addModule: async () => {} };
	constructor() {
		contexts.push(this);
	}
	async resume() {
		this.resumes++;
		this.state = 'running';
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

describe('audio follows the hello', () => {
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
		audio.setEnabled(false);
		await audio.start();
		await settle();
		expect(contexts.length).toBe(0);
		expect(posted.length).toBe(0);
	});

	test('a hello with audio starts it once, a gesture before it included', async () => {
		const { audio, posted } = setup();
		await audio.start();
		audio.setEnabled(true);
		await settle();
		await audio.start();
		await settle();
		expect(contexts.length).toBe(1);
		expect(posted).toEqual([expect.objectContaining({ t: 'audioPort' })]);
	});

	test('a later hello without audio suspends it and one with audio resumes it', async () => {
		const { audio } = setup();
		audio.setEnabled(true);
		await settle();
		expect(contexts[0].state).toBe('running');
		audio.setEnabled(false);
		await settle();
		expect(contexts[0].suspends).toBe(1);
		await audio.start();
		await settle();
		expect(contexts[0].state).toBe('suspended');
		audio.setEnabled(true);
		await settle();
		expect(contexts.length).toBe(1);
		expect(contexts[0].state).toBe('running');
	});
});
