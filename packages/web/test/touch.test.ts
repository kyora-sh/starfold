import { afterEach, describe, expect, mock, spyOn, test } from 'bun:test';
import type { App } from '../src/client/app';
import { createTouch } from '../src/client/touch';

const ev = (pointerId: number, clientX: number, clientY: number) =>
	({ pointerId, clientX, clientY }) as PointerEvent;

/** A touch handler on a fake surface, with a clock the test moves and the positions
 * the remote pointer was moved to. */
function setup(deskPerPx = 1) {
	let now = 1000;
	spyOn(performance, 'now').mockImplementation(() => now);
	const rect = { left: 0, top: 0, width: 800, height: 600 };
	const app = {
		send: () => {},
		surface: {
			setPointerCapture: () => {},
			getBoundingClientRect: () => ({ ...rect }),
		},
	} as unknown as App;
	const moves: [number, number][] = [];
	const k = { deskPerPx };
	const touch = createTouch(app, {
		moveTo: (x, y) => moves.push([x, y]),
		queueScroll: () => {},
		deskPerPx: () => k.deskPerPx,
	});
	const tap = (x: number, y: number) => {
		touch.down(ev(1, x, y));
		touch.up(ev(1, x, y));
		return moves[moves.length - 1];
	};
	return {
		touch,
		ev,
		tap,
		rect,
		k,
		moves,
		wait: (ms: number) => {
			now += ms;
		},
	};
}

describe('touch multi-tap', () => {
	afterEach(() => {
		mock.restore();
	});

	test('a second tap close and soon clicks the first spot, and triple taps chain', () => {
		const t = setup();
		expect(t.tap(100, 100)).toEqual([100, 100]);
		t.wait(150);
		expect(t.tap(110, 106)).toEqual([100, 100]);
		t.wait(150);
		expect(t.tap(92, 104)).toEqual([100, 100]);
	});

	test('taps too far apart in time or space click where they land', () => {
		const t = setup();
		t.tap(100, 100);
		t.wait(150);
		expect(t.tap(160, 100)).toEqual([160, 100]);
		t.wait(500);
		expect(t.tap(165, 100)).toEqual([165, 100]);
	});

	test('the desktop distance bounds the chain on a scaled-down view', () => {
		const t = setup(4);
		t.tap(100, 100);
		t.wait(150);
		expect(t.tap(110, 100)).toEqual([110, 100]);
		t.wait(150);
		expect(t.tap(116, 100)).toEqual([110, 100]);
	});

	test('a drag between taps ends the chain', () => {
		const t = setup();
		t.tap(100, 100);
		t.touch.down(t.ev(1, 100, 100));
		t.touch.move(t.ev(1, 140, 100));
		t.touch.up(t.ev(1, 140, 100));
		t.wait(100);
		expect(t.tap(104, 100)).toEqual([104, 100]);
	});

	test('a two-finger gesture between taps ends the chain', () => {
		const t = setup();
		t.tap(100, 100);
		t.touch.down(t.ev(1, 100, 100));
		t.touch.down(t.ev(2, 200, 100));
		t.touch.up(t.ev(2, 200, 100));
		t.touch.up(t.ev(1, 100, 100));
		t.wait(100);
		expect(t.tap(104, 100)).toEqual([104, 100]);
	});

	test('a cancelled touch between taps ends the chain', () => {
		const t = setup();
		t.tap(100, 100);
		t.touch.down(t.ev(1, 100, 100));
		t.touch.cancel(t.ev(1, 100, 100));
		t.wait(100);
		expect(t.tap(104, 100)).toEqual([104, 100]);
	});

	test('a long press between taps ends the chain', async () => {
		const t = setup();
		t.tap(100, 100);
		t.touch.down(t.ev(1, 100, 100));
		await Bun.sleep(600);
		t.touch.up(t.ev(1, 100, 100));
		t.wait(100);
		expect(t.tap(104, 100)).toEqual([104, 100]);
	});

	test('a change of the surface geometry ends the chain', () => {
		const t = setup();
		t.tap(100, 100);
		t.rect.width = 700;
		t.wait(100);
		expect(t.tap(104, 100)).toEqual([104, 100]);
		t.k.deskPerPx = 2;
		t.wait(100);
		expect(t.tap(106, 100)).toEqual([106, 100]);
	});

	test('a geometry change while the next finger is down ends the chain, even if it changes back', () => {
		const t = setup();
		t.tap(100, 100);
		t.rect.width = 700;
		t.touch.down(ev(1, 104, 100));
		t.rect.width = 800;
		t.wait(100);
		t.touch.up(ev(1, 104, 100));
		expect(t.moves.at(-1)).toEqual([104, 100]);
	});

	test('a geometry change between the next finger going down and lifting ends the chain', () => {
		const t = setup();
		t.tap(100, 100);
		t.touch.down(ev(1, 104, 100));
		t.rect.width = 700;
		t.wait(100);
		t.touch.up(ev(1, 104, 100));
		expect(t.moves.at(-1)).toEqual([104, 100]);
	});
});
