import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';
import { stealBetween } from './steal.mjs';
test('share of the ticks in the window', () => assert.equal(stealBetween({ steal: 10, all: 1000 }, { steal: 15, all: 1200 }), 2.5));
test('no ticks is not a zero', () => assert.ok(Number.isNaN(stealBetween({ steal: 1, all: 5 }, { steal: 1, all: 5 }))));
test('quality rows carry the steal of their window', () => {
	const src = fs.readFileSync(new URL('./driver.mjs', import.meta.url), 'utf8');
	const body = src.slice(src.indexOf('async function quality('), src.indexOf('async function soak('));
	assert.match(body, /stealPct: stealBetween\(/);
});
