import { describe, expect, test } from 'bun:test';
import {
	NEVER_OPENED_MAX,
	RECONNECT_DEFAULTS,
	Reconnector,
	STABLE_MS,
	backoff,
	retryable,
} from '../src/embed/reconnect';
import type { Env } from '../src/embed/reconnect';
import type { ReconnectOptions } from '../src/embed/types';

/** A clock the test moves, its timers, and a page whose visibility and network the test sets. */
function fakeEnv(random = 0.5) {
	let now = 0;
	let next = 1;
	const timers = new Map<number, { at: number; f: () => void }>();
	const wakes = new Set<() => void>();
	const page = { hidden: false, online: true };
	const env: Env = {
		now: () => now,
		setTimeout(f, ms) {
			const id = next++;
			timers.set(id, { at: now + ms, f });
			return id;
		},
		clearTimeout(h) {
			timers.delete(h as number);
		},
		random: () => random,
		hidden: () => page.hidden,
		online: () => page.online,
		onWake(f) {
			wakes.add(f);
			return () => wakes.delete(f);
		},
	};
	return {
		env,
		page,
		timers,
		wakes,
		/** Moves the clock, running each timer that falls due. */
		advance(ms: number) {
			const end = now + ms;
			for (;;) {
				const due = [...timers.entries()]
					.filter(([, t]) => t.at <= end)
					.toSorted((a, b) => a[1].at - b[1].at)[0];
				if (!due) break;
				now = due[1].at;
				timers.delete(due[0]);
				due[1].f();
			}
			now = end;
		},
		wake() {
			for (const f of wakes) f();
		},
	};
}

function reconnector(o: true | ReconnectOptions = true, random = 0.5) {
	const e = fakeEnv(random);
	return { r: new Reconnector(o, e.env), ...e };
}

describe('which closes reconnect', () => {
	test('the default table', () => {
		for (const code of [1006, 1001, 1012, 1013, 4401, 4400, 4499, 4429])
			expect([code, retryable(code)]).toEqual([code, true]);
		for (const code of [
			1000, 1005, 1003, 1007, 1009, 1011, 4000, 4001, 4002, 4406, 4500, 4399,
			3000,
		])
			expect([code, retryable(code)]).toEqual([code, false]);
	});

	test('a close not worth another attempt gives up with why "code"', () => {
		const { r } = reconnector();
		r.opened();
		expect(r.closed(1000, 'bye')).toEqual({ t: 'give-up', why: 'code' });
	});

	test('shouldReconnect replaces the table', () => {
		const asked: [number, string][] = [];
		const { r } = reconnector({
			shouldReconnect: (code, reason) => {
				asked.push([code, reason]);
				return code === 1000;
			},
		});
		expect(r.closed(1000, 'restart').t).toBe('retry');
		expect(r.closed(1006, '')).toEqual({ t: 'give-up', why: 'code' });
		expect(asked).toEqual([
			[1000, 'restart'],
			[1006, ''],
		]);
	});
});

describe('backoff', () => {
	test('full jitter below min(cap, initial * factor^(n-1))', () => {
		const o = { initialMs: 500, factor: 2, capMs: 30_000 };
		const top = (n: number) => backoff(n, o, () => 0.999999);
		expect([1, 2, 3, 4, 5, 6, 7, 8].map(top)).toEqual([
			499, 999, 1999, 3999, 7999, 15999, 29999, 29999,
		]);
		expect(backoff(5, o, () => 0)).toBe(0);
	});

	test('each retry carries its attempt number and a delay from the defaults', () => {
		const { r } = reconnector(true, 0.5);
		const got = [1, 2, 3].map(() => r.closed(1006, ''));
		expect(got).toEqual([
			{ t: 'retry', attempt: 1, delayMs: 250 },
			{ t: 'retry', attempt: 2, delayMs: 500 },
			{ t: 'give-up', why: 'never-opened' },
		]);
	});
});

/** A close right after an open, with a code that never trips another rule. */
function dropAfterOpen(r: Reconnector) {
	r.opened();
	return r.closed(1012, 'restart');
}

describe('the limits of a run', () => {
	test('gives up after maxAttempts', () => {
		const { r } = reconnector({ maxAttempts: 3 });
		expect([1, 2, 3, 4].map(() => dropAfterOpen(r).t)).toEqual([
			'retry',
			'retry',
			'retry',
			'give-up',
		]);
		expect(dropAfterOpen(r)).toEqual({ t: 'give-up', why: 'attempts' });
		expect(RECONNECT_DEFAULTS.maxAttempts).toBe(10);
	});

	test('gives up after maxMs since the first close', () => {
		const { r, advance } = reconnector({ maxMs: 60_000, maxAttempts: 100 });
		expect(dropAfterOpen(r).t).toBe('retry');
		advance(59_000);
		expect(dropAfterOpen(r).t).toBe('retry');
		advance(1_000);
		expect(dropAfterOpen(r)).toEqual({ t: 'give-up', why: 'time' });
	});

	test('a connection open for 10 s starts the count over; a shorter one does not', () => {
		const { r, advance } = reconnector({ maxAttempts: 2 });
		expect(dropAfterOpen(r)).toMatchObject({ attempt: 1 });
		r.opened();
		advance(STABLE_MS - 1);
		expect(r.closed(1012, '')).toMatchObject({ attempt: 2 });
		r.opened();
		advance(STABLE_MS);
		expect(r.closed(1012, '')).toMatchObject({ t: 'retry', attempt: 1 });
	});

	test('two closes in a row with the same 44xx code right after open give up', () => {
		const { r } = reconnector();
		r.opened();
		expect(r.closed(4401, 'ticket')).toMatchObject({ t: 'retry', attempt: 1 });
		r.opened();
		expect(r.closed(4401, 'ticket')).toEqual({
			t: 'give-up',
			why: 'repeated-close',
		});
	});

	test('a different code between two 44xx closes, or a stable open, keeps going', () => {
		const { r, advance } = reconnector();
		r.opened();
		expect(r.closed(4401, '').t).toBe('retry');
		r.opened();
		expect(r.closed(4429, '').t).toBe('retry');
		r.opened();
		expect(r.closed(1012, '').t).toBe('retry');
		r.opened();
		expect(r.closed(4429, '').t).toBe('retry');
		r.opened();
		advance(STABLE_MS);
		expect(r.closed(4429, '')).toMatchObject({ t: 'retry', attempt: 1 });
	});

	test('a 44xx close after a stable open is not the first of a repeat', () => {
		const { r, advance } = reconnector();
		r.opened();
		advance(STABLE_MS + 2000);
		expect(r.closed(4401, '')).toMatchObject({ t: 'retry', attempt: 1 });
		r.opened();
		advance(50);
		expect(r.closed(4401, '')).toMatchObject({ t: 'retry', attempt: 2 });
		r.opened();
		expect(r.closed(4401, '')).toEqual({
			t: 'give-up',
			why: 'repeated-close',
		});
	});

	test('never-opened counts only 1006 closes in a row: another code in between starts it over', () => {
		const { r } = reconnector();
		expect(r.closed(1006, '').t).toBe('retry');
		expect(r.closed(1012, '').t).toBe('retry');
		expect(r.closed(1006, '').t).toBe('retry');
		expect(r.closed(1006, '').t).toBe('retry');
		expect(r.closed(1006, '')).toEqual({ t: 'give-up', why: 'never-opened' });
	});

	test('a 44xx close that never reached open does not count toward the repeat', () => {
		const { r } = reconnector({
			shouldReconnect: () => true,
		});
		expect(r.closed(4401, '').t).toBe('retry');
		expect(r.closed(4401, '').t).toBe('retry');
	});

	test(`${NEVER_OPENED_MAX} closes with 1006 in a row that never opened give up; an open in between resets it`, () => {
		const { r } = reconnector();
		expect(r.closed(1006, '').t).toBe('retry');
		expect(r.closed(1006, '').t).toBe('retry');
		r.opened();
		expect(r.closed(1006, '').t).toBe('retry');
		expect(r.closed(1006, '').t).toBe('retry');
		expect(r.closed(1006, '').t).toBe('retry');
		expect(r.closed(1006, '')).toEqual({ t: 'give-up', why: 'never-opened' });
	});
});

describe('waiting for the attempt', () => {
	const never = () => {
		throw new Error('expired');
	};

	test('connects when the delay is up, once', () => {
		const { r, advance, wakes } = reconnector();
		let connects = 0;
		r.schedule(1000, () => connects++, never);
		expect(r.pending).toBe(true);
		advance(999);
		expect(connects).toBe(0);
		advance(1);
		expect(connects).toBe(1);
		expect(r.pending).toBe(false);
		expect(wakes.size).toBe(0);
		advance(10_000);
		expect(connects).toBe(1);
	});

	test('a delay that ends past maxMs gives up at the deadline instead of connecting', () => {
		const { r, advance, timers } = reconnector({ maxMs: 1000 });
		let connects = 0;
		let expired = 0;
		expect(r.closed(1006, '').t).toBe('retry');
		advance(600);
		r.schedule(
			5000,
			() => connects++,
			() => expired++
		);
		advance(399);
		expect(expired).toBe(0);
		advance(1);
		expect([connects, expired, timers.size, r.pending]).toEqual([
			0,
			1,
			0,
			false,
		]);
	});

	test('a hidden page past the deadline gives up instead of connecting', () => {
		const { r, advance, page, wake } = reconnector({ maxMs: 1000 });
		let connects = 0;
		let expired = 0;
		expect(r.closed(1006, '').t).toBe('retry');
		page.hidden = true;
		r.schedule(
			10,
			() => connects++,
			() => expired++
		);
		advance(5000);
		expect(expired).toBe(0);
		page.hidden = false;
		wake();
		expect([connects, expired]).toEqual([0, 1]);
	});

	test('a hidden page connects only once it is visible again, at once', () => {
		const { r, advance, page, wake } = reconnector();
		let connects = 0;
		page.hidden = true;
		r.schedule(1000, () => connects++, never);
		advance(5000);
		expect(connects).toBe(0);
		expect(r.pending).toBe(true);
		wake();
		expect(connects).toBe(0);
		page.hidden = false;
		wake();
		expect(connects).toBe(1);
		wake();
		expect(connects).toBe(1);
		expect(r.pending).toBe(false);
	});

	test('offline waits for online, and time offline does not count toward maxMs', () => {
		const { r, advance, page, wake } = reconnector({
			maxMs: 10_000,
			maxAttempts: 100,
		});
		let connects = 0;
		r.opened();
		expect(r.closed(1012, '').t).toBe('retry');
		page.online = false;
		r.schedule(100, () => connects++, never);
		advance(60_000);
		expect(connects).toBe(0);
		page.online = true;
		wake();
		expect(connects).toBe(1);
		r.opened();
		advance(1000);
		expect(r.closed(1012, '').t).toBe('retry');
	});

	test('offline from the start of the delay: back online connects at once, the time not counted', () => {
		const { r, advance, page, wake, timers } = reconnector({
			maxMs: 10_000,
			maxAttempts: 100,
		});
		let connects = 0;
		r.opened();
		expect(r.closed(1012, '').t).toBe('retry');
		page.online = false;
		r.schedule(4000, () => connects++, never);
		advance(1000);
		wake();
		expect(connects).toBe(0);
		page.online = true;
		wake();
		expect([connects, timers.size]).toEqual([1, 0]);
		r.opened();
		advance(9500);
		expect(r.closed(1012, '').t).toBe('retry');
	});

	test('going offline during the delay is noticed then, not when it ends', () => {
		const { r, advance, page, wake } = reconnector({
			maxMs: 10_000,
			maxAttempts: 100,
		});
		let connects = 0;
		r.opened();
		expect(r.closed(1012, '').t).toBe('retry');
		r.schedule(4000, () => connects++, never);
		advance(500);
		page.online = false;
		wake();
		advance(60_000);
		expect(connects).toBe(0);
		page.online = true;
		wake();
		expect(connects).toBe(1);
		r.opened();
		advance(9000);
		expect(r.closed(1012, '').t).toBe('retry');
	});

	test('hold runs at once on a visible page, else once it is visible and online', () => {
		const { r, page, wake, wakes } = reconnector();
		let sent = 0;
		r.hold(() => sent++);
		expect(sent).toBe(1);
		page.hidden = true;
		r.hold(() => sent++);
		expect([sent, r.pending]).toEqual([1, true]);
		page.hidden = false;
		page.online = false;
		wake();
		expect(sent).toBe(1);
		page.online = true;
		wake();
		expect([sent, r.pending, wakes.size]).toEqual([2, false, 0]);
	});

	test('cancel and reset drop the waiting attempt and its listener', () => {
		const { r, advance, page, wake, wakes, timers } = reconnector();
		let connects = 0;
		r.schedule(1000, () => connects++, never);
		r.cancel();
		advance(2000);
		expect(connects).toBe(0);
		page.hidden = true;
		r.schedule(10, () => connects++, never);
		advance(20);
		r.reset();
		page.hidden = false;
		wake();
		expect([connects, r.pending, wakes.size, timers.size]).toEqual([
			0,
			false,
			0,
			0,
		]);
	});
});

describe('the limits themselves', () => {
	test('a limit that is not a positive number throws; a factor below 1 too', () => {
		const env = fakeEnv().env;
		for (const o of [
			{ maxAttempts: Number.NaN },
			{ maxMs: Number.POSITIVE_INFINITY },
			{ capMs: -1 },
			{ initialMs: 0 },
			{ factor: 0.5 },
			{ maxAttempts: '3' as unknown as number },
		])
			expect(() => new Reconnector(o, env)).toThrow(TypeError);
		expect(() => new Reconnector({ factor: 1 }, env)).not.toThrow();
	});
});
