import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';
import { stealBetween } from './steal.mjs';
test('share of the ticks in the window', () => assert.equal(stealBetween({ steal: 10, all: 1000 }, { steal: 15, all: 1200 }), 2.5));
test('no ticks is not a zero', () => assert.ok(Number.isNaN(stealBetween({ steal: 1, all: 5 }, { steal: 1, all: 5 }))));
test('quality rows take steal from the start of motion to the last settle point', () => {
	const src = fs.readFileSync(new URL('./driver.mjs', import.meta.url), 'utf8');
	const body = src.slice(src.indexOf('async function quality('), src.indexOf('async function soak('));
	assert.match(body, /stealPct: stealBetween\(h0\.host, h1\.host\)/);
	const order = ["h0 = await ctlJson('/cpu')", "await ctl('/freeze')", 'for (const ms of SETTLE_MS)', 'await poller', "h1 = await ctlJson('/cpu')", '/truth?out='].map((x) => body.indexOf(x));
	assert.ok(order.every((at, i) => at >= 0 && (i === 0 || at > order[i - 1])), `window out of order: ${order}`);
});
