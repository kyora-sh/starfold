import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
	AuthError,
	ForbiddenError,
	NotFoundError,
	ReadOnlyError,
	RecordingOffError,
	Starfold,
	StarfoldConnectionError,
	StarfoldError,
	StarfoldTimeoutError,
	act,
} from '../src/index.ts';
import type { Recording } from '../src/index.ts';
import { FakeServer, PNG, RID } from './fake.ts';

const text = (s: string) => new TextEncoder().encode(s);

let server: FakeServer;
let sf: Starfold;

beforeEach(() => {
	server = new FakeServer();
	sf = new Starfold(server.url, { password: 'pw' });
});

afterEach(() => {
	server.stop();
});

function withEnv(
	vars: Record<string, string | undefined>,
	run: () => Promise<void>
): Promise<void> {
	const before = Object.fromEntries(
		Object.keys(vars).map((k) => [k, process.env[k]])
	);
	for (const [k, v] of Object.entries(vars)) {
		if (v === undefined) delete process.env[k];
		else process.env[k] = v;
	}
	return run().finally(() => {
		for (const [k, v] of Object.entries(before)) {
			if (v === undefined) delete process.env[k];
			else process.env[k] = v;
		}
	});
}

function warnings(): { got: string[]; restore: () => void } {
	const got: string[] = [];
	const original = process.emitWarning;
	process.emitWarning = ((m: string | Error, t?: unknown) => {
		got.push(`${String(t)}: ${String(m)}`);
	}) as typeof process.emitWarning;
	return { got, restore: () => (process.emitWarning = original) };
}

describe('screenshots', () => {
	test('screenshot', async () => {
		const shot = await sf.screenshot();
		expect(shot.png).toEqual(PNG);
		expect(shot.seq).toBe(41);
		expect(shot.size).toEqual([1280, 800]);
	});

	test('save', async () => {
		const path = join(mkdtempSync(join(tmpdir(), 'sf-')), 's.png');
		expect(await (await sf.screenshot()).save(path)).toBe(path);
		expect(new Uint8Array(readFileSync(path))).toEqual(PNG);
	});

	test('seq from a server that only sends the voxl header', async () => {
		server.seqHeader = 'x-voxl-seq';
		expect((await sf.screenshot()).seq).toBe(41);
	});
});

describe('wait', () => {
	test('params and result', async () => {
		const r = await sf.wait({
			change: true,
			since: 41,
			region: [0, 0, 800, 600],
			timeoutMs: 2000,
		});
		expect(r).toEqual({ changed: true, timedOut: false, seq: 42, waitedMs: 7 });
		expect(server.last.params).toEqual({
			change: '1',
			since: '41',
			region: '0,0,800,600',
			timeout: '2000',
		});
		await sf.wait({ idleMs: 500 });
		expect(server.last.params).toEqual({ idle: '500' });
	});

	test('gets its timeout on top of the client timeout', async () => {
		const quick = new Starfold(server.url, { password: 'pw', timeoutMs: 100 });
		server.delayMs = 250;
		expect((await quick.wait({ timeoutMs: 300 })).seq).toBe(42);
		await expect(quick.screenshot()).rejects.toBeInstanceOf(
			StarfoldTimeoutError
		);
	});
});

describe('input', () => {
	test('methods send actions', async () => {
		const r = await sf.click(10, 20);
		expect(r).toEqual({ seq: 40, events: 1 });
		expect(server.lastJson()).toEqual([
			{ type: 'click', button: 'left', count: 1, x: 10, y: 20 },
		]);
		await sf.doubleClick(undefined, undefined, 'right');
		expect(server.lastJson()).toEqual([
			{ type: 'click', button: 'right', count: 2 },
		]);
		await sf.move(1, 2);
		expect(server.lastJson()).toEqual([{ type: 'move', x: 1, y: 2 }]);
		await sf.scroll(0, 3);
		expect(server.lastJson()).toEqual([{ type: 'scroll', dx: 0, dy: 3 }]);
		await sf.scroll(0, 3, 400, 300);
		expect(server.lastJson()).toEqual([
			{ type: 'scroll', dx: 0, dy: 3, x: 400, y: 300 },
		]);
		await sf.key('ctrl+c');
		expect(server.lastJson()).toEqual([{ type: 'key', key: 'ctrl+c' }]);
		await sf.typeText('hi\n');
		expect(server.lastJson()).toEqual([{ type: 'type', text: 'hi\n' }]);
		await sf.mouseDown(5, 5);
		expect(server.lastJson()).toEqual([
			{ type: 'down', button: 'left', x: 5, y: 5 },
		]);
		await sf.mouseUp();
		expect(server.lastJson()).toEqual([{ type: 'up', button: 'left' }]);
		expect(server.last.method).toBe('POST');
		expect(server.last.headers.get('content-type')).toBe('application/json');
	});

	test('drag moves in steps', async () => {
		await sf.drag(0, 0, 100, 50, 'left', 2, 5);
		expect(server.lastJson()).toEqual([
			{ type: 'down', button: 'left', x: 0, y: 0 },
			{ type: 'pause', ms: 5 },
			{ type: 'move', x: 50, y: 25 },
			{ type: 'pause', ms: 5 },
			{ type: 'move', x: 100, y: 50 },
			{ type: 'up', button: 'left' },
		]);
	});

	test('a batch is flattened, and its pauses and typing extend the timeout', async () => {
		const quick = new Starfold(server.url, { password: 'pw', timeoutMs: 100 });
		server.delayMs = 250;
		await quick.actions([
			act.key('ctrl+l'),
			act.pause(300),
			act.drag(0, 0, 1, 1, 'left', 1),
			act.typeText('x'),
		]);
		expect(
			(server.lastJson() as { type: string }[]).map((a) => a.type)
		).toEqual(['key', 'pause', 'down', 'pause', 'move', 'up', 'type']);
		await expect(quick.key('a')).rejects.toBeInstanceOf(StarfoldTimeoutError);
	});

	test('the other act builders', () => {
		expect(act.keyDown('shift')).toEqual({ type: 'key_down', key: 'shift' });
		expect(act.keyUp('shift')).toEqual({ type: 'key_up', key: 'shift' });
		expect(
			act.drag(10, 10, 0, 0, 'left', 3, 1).filter((a) => a.type === 'move')
		).toEqual([
			{ type: 'move', x: 6, y: 6 },
			{ type: 'move', x: 3, y: 3 },
			{ type: 'move', x: 0, y: 0 },
		]);
		expect(act.drag(0, 0, 1, 1, 'left', 0).length).toBe(4);
	});

	test('half a position is refused', () => {
		expect(() => act.click(1)).toThrow(RangeError);
		expect(() => act.scroll(0, 1, undefined, 3)).toThrow(
			'give both x and y, or neither'
		);
	});

	test('read-only', async () => {
		server.readOnly = true;
		const e = await sf.key('Enter').catch((x: unknown) => x);
		expect(e).toBeInstanceOf(ReadOnlyError);
		expect(e).toBeInstanceOf(ForbiddenError);
		expect((e as ReadOnlyError).status).toBe(403);
		expect((e as ReadOnlyError).message).toContain('read-only');
	});
});

describe('password', () => {
	test('a wrong password', async () => {
		const c = new Starfold(server.url, { password: 'nope' });
		const e = await c.screenshot().catch((x: unknown) => x);
		expect(e).toBeInstanceOf(AuthError);
		expect((e as AuthError).status).toBe(401);
		expect((e as AuthError).message).toBe('starfold needs its password');
	});

	test('from the environment', () =>
		withEnv({ STARFOLD_PASSWORD: 'pw', VOXL_PASSWORD: undefined }, async () => {
			const w = warnings();
			try {
				expect((await new Starfold(server.url).screenshot()).seq).toBe(41);
				expect(w.got).toEqual([]);
			} finally {
				w.restore();
			}
		}));

	test('VOXL_PASSWORD still works and warns', () =>
		withEnv({ STARFOLD_PASSWORD: undefined, VOXL_PASSWORD: 'pw' }, async () => {
			const w = warnings();
			try {
				expect((await new Starfold(server.url).screenshot()).seq).toBe(41);
				expect(w.got).toEqual([
					'DeprecationWarning: VOXL_PASSWORD is deprecated: use STARFOLD_PASSWORD',
				]);
			} finally {
				w.restore();
			}
		}));

	test('STARFOLD_PASSWORD wins over VOXL_PASSWORD', () =>
		withEnv({ STARFOLD_PASSWORD: 'pw', VOXL_PASSWORD: 'old' }, async () => {
			const w = warnings();
			try {
				expect((await new Starfold(server.url).screenshot()).seq).toBe(41);
				expect(w.got).toEqual([
					'DeprecationWarning: VOXL_PASSWORD is ignored: STARFOLD_PASSWORD is set',
				]);
			} finally {
				w.restore();
			}
		}));

	test('an explicit password wins and sends no warning', () =>
		withEnv({ STARFOLD_PASSWORD: 'other', VOXL_PASSWORD: 'old' }, async () => {
			const w = warnings();
			try {
				expect(
					(await new Starfold(server.url, { password: 'pw' }).screenshot()).seq
				).toBe(41);
				expect(w.got).toEqual([]);
			} finally {
				w.restore();
			}
		}));
});

describe('recording', () => {
	test('record, start and stop', async () => {
		const clips = await sf.record(2.5);
		expect(clips).toEqual(
			Object.assign([text('mp4:2.5')], { endReason: 'stopped' })
		);
		expect(server.last.params).toEqual({ seconds: '2.5', format: 'zip' });
		const rec = await sf.recordStart();
		expect(rec).toEqual({ id: RID, maxSeconds: 300 });
		const stopped = await sf.recordStop(rec);
		expect([...stopped]).toEqual([text('mp4')]);
		expect(stopped.endReason).toBe('stopped');
		expect(server.last.params).toEqual({ id: RID, format: 'zip' });
		expect([...(await sf.recordStop(RID))]).toEqual([text('mp4')]);
		await expect(sf.recordStop('0'.repeat(32))).rejects.toBeInstanceOf(
			NotFoundError
		);
	});

	test('record gets its length on top of the timeout', async () => {
		const quick = new Starfold(server.url, { password: 'pw', timeoutMs: 100 });
		server.delayMs = 250;
		expect((await quick.record(0.3)).length).toBe(1);
	});

	test('a number is not a recording id and is never sent', async () => {
		const sent = server.seen.length;
		const wrong = [3, 3.0, null, { id: 3, maxSeconds: 300 }] as unknown as (
			| Recording
			| string
		)[];
		const errors = await Promise.all(
			wrong.map((w) => sf.recordStop(w).catch((x: unknown) => x))
		);
		for (const e of errors) {
			expect(e).toBeInstanceOf(TypeError);
			expect((e as TypeError).message).toContain('a recording id is a string');
		}
		expect(server.seen.length).toBe(sent);
	});

	test('a server that does not record says so', async () => {
		server.recordingOn = false;
		const errors = await Promise.all(
			[sf.record(1), sf.recordStart(), sf.recordStop(RID)].map((p) =>
				p.catch((x: unknown) => x)
			)
		);
		for (const e of errors) {
			expect(e).toBeInstanceOf(RecordingOffError);
			expect(e).toBeInstanceOf(ForbiddenError);
			expect((e as RecordingOffError).status).toBe(403);
			expect((e as RecordingOffError).message).toContain('--enable-recording');
		}
	});

	test('several segments', async () => {
		server.segments = [text('part1'), text('part2'), text('part3')];
		server.endReason = 'segment_cap';
		const clips = await sf.recordStop(RID);
		expect([...clips]).toEqual([text('part1'), text('part2'), text('part3')]);
		expect(clips.endReason).toBe('segment_cap');
	});

	test('deflated members', async () => {
		server.deflated = true;
		server.segments = [text('part1'.repeat(100)), text('part2')];
		expect([...(await sf.recordStop(RID))]).toEqual([
			text('part1'.repeat(100)),
			text('part2'),
		]);
	});

	test('the zip media type is case-insensitive', async () => {
		server.contentType = 'Application/Zip; charset=binary';
		expect([...(await sf.recordStop(RID))]).toEqual([text('mp4')]);
	});

	test('a server without segments answers with one MP4', async () => {
		server.handler = () =>
			new Response('mp4', { headers: { 'content-type': 'video/mp4' } });
		const clips = await sf.recordStop(RID);
		expect([...clips]).toEqual([text('mp4')]);
		expect(clips.endReason).toBeNull();
	});
});

describe('files', () => {
	test('list, download and upload', async () => {
		const listing = await sf.filesList('Documents');
		expect(listing).toEqual({
			path: 'Documents',
			entries: [{ name: 'a.txt', dir: false, size: 3, mtimeMs: 1700000000000 }],
		});
		expect(server.last.params).toEqual({ path: 'Documents' });
		await sf.filesList();
		expect(server.last.params).toEqual({ path: '' });
		expect(await sf.filesDownload('ok.txt')).toEqual(text('abc'));
		const e = await sf.filesDownload('a.txt').catch((x: unknown) => x);
		expect(e).toBeInstanceOf(ForbiddenError);
		expect(e).not.toBeInstanceOf(ReadOnlyError);

		const local = join(mkdtempSync(join(tmpdir(), 'sf-')), 'up.bin');
		writeFileSync(local, '12345');
		const up = await sf.filesUpload('up.bin', local, undefined, 'x');
		expect(up).toEqual({ path: 'Downloads/up.bin', size: 5 });
		expect(server.last.method).toBe('PUT');
		expect(server.last.params).toEqual({ name: 'up.bin', sub: 'x' });
		expect(server.last.body).toEqual(text('12345'));
		expect((await sf.filesUpload('b.bin', text('ab'), 'Desktop')).size).toBe(2);
		expect(server.last.params).toEqual({ name: 'b.bin', path: 'Desktop' });
		expect((await sf.filesUpload('c.bin', new Blob(['abc']))).size).toBe(3);
		expect((await sf.filesUpload('d.bin', new ArrayBuffer(4))).size).toBe(4);
		expect(
			(await sf.filesUpload('e.bin', new URL(`file://${local}`))).size
		).toBe(5);
	});
});

describe('errors', () => {
	test('other errors keep their status', async () => {
		server.handler = () =>
			Response.json({ error: 'region is off screen' }, { status: 400 });
		const e = await sf
			.wait({ region: [9999, 0, 1, 1] })
			.catch((x: unknown) => x);
		expect(e).toBeInstanceOf(StarfoldError);
		expect((e as StarfoldError).status).toBe(400);
		expect((e as StarfoldError).message).toBe('400: region is off screen');
	});

	test('an empty error body falls back to the status text', async () => {
		server.handler = () =>
			new Response('', { status: 500, statusText: 'Internal Server Error' });
		const e = await sf.screenshot().catch((x: unknown) => x);
		expect((e as StarfoldError).message).toBe('500: Internal Server Error');
		expect((e as StarfoldError).status).toBe(500);
	});

	test('a server that is not there', async () => {
		server.stop();
		const e = await sf.screenshot().catch((x: unknown) => x);
		expect(e).toBeInstanceOf(StarfoldConnectionError);
		expect((e as StarfoldError).status).toBeNull();
	});

	test('other 403s on input are not read-only', async () => {
		server.handler = () =>
			new Response('host read-only.example is not allowed (--allowed-hosts)', {
				status: 403,
			});
		const e = await sf.click().catch((x: unknown) => x);
		expect(e).toBeInstanceOf(ForbiddenError);
		expect(e).not.toBeInstanceOf(ReadOnlyError);
		expect((e as ForbiddenError).message).toBe(
			'host read-only.example is not allowed (--allowed-hosts)'
		);
	});

	test('the URL prefix is kept', async () => {
		server.handler = () =>
			Response.json(
				{
					error: 'this server is read-only (--read-only): input is refused',
					read_only: true,
				},
				{ status: 403 }
			);
		const c = new Starfold(`${server.url}/desk/`);
		expect(c.url).toBe(`${server.url}/desk`);
		await expect(c.key('a')).rejects.toBeInstanceOf(ReadOnlyError);
		await Bun.sleep(50);
		expect(server.last.path).toBe('/desk/input');
	});

	test('an injected fetch', async () => {
		const urls: string[] = [];
		const c = new Starfold('http://desk:6901', {
			password: 'pw',
			fetch: (async (input: string | URL | Request) => {
				urls.push(String(input));
				return Response.json({ seq: 1, events: 1 });
			}) as typeof fetch,
		});
		expect(await c.key('a')).toEqual({ seq: 1, events: 1 });
		expect(urls).toEqual(['http://desk:6901/input']);
	});

	test('close and asyncDispose', async () => {
		{
			await using c = new Starfold(server.url, { password: 'pw' });
			expect((await c.screenshot()).seq).toBe(41);
		}
		await sf.close();
	});
});
