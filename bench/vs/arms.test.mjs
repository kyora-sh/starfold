import assert from 'node:assert/strict';
import test from 'node:test';
import { armOf, baseArm, expandArms, roundOrder, TWIN } from './arms.mjs';
import { ARMS } from './config.mjs';

test('the first arm runs twice by default', () => {
	assert.deepEqual(expandArms(['before', 'starfold-video']), ['before', `before${TWIN}`, 'starfold-video']);
	assert.deepEqual(expandArms(['before']), ['before', `before${TWIN}`]);
});
test('AA=0, an empty list and a list with its twin already stay as they are', () => {
	assert.deepEqual(expandArms(['before', 'starfold-video'], false), ['before', 'starfold-video']);
	assert.deepEqual(expandArms([]), []);
	assert.deepEqual(expandArms(['a', `a${TWIN}`]), ['a', `a${TWIN}`]);
});
test('a twin has its arm settings', () => {
	assert.equal(baseArm(`novnc${TWIN}`), 'novnc');
	assert.equal(armOf(`novnc${TWIN}`), ARMS.novnc);
	assert.equal(armOf('starfold-video'), ARMS['starfold-video']);
});
test('each round starts one arm later, as the driver always did', () => {
	const a = ['a', 'b', 'c'];
	assert.deepEqual(roundOrder(a, 0), ['a', 'b', 'c']);
	assert.deepEqual(roundOrder(a, 1), ['b', 'c', 'a']);
	assert.deepEqual(roundOrder(a, 1, 1), ['c', 'a', 'b']);
});
test('runs continue the rotation, so every arm takes every position', () => {
	const arms = expandArms(['a', 'b', 'c', 'd']);
	const at = Object.fromEntries(arms.map((x) => [x, arms.map(() => 0)]));
	for (let run = 0; run < 3; run++)
		for (let round = 0; round < 4; round++) roundOrder(arms, round, 0, run, 4).forEach((x, p) => at[x][p]++);
	for (const counts of Object.values(at)) assert.ok(Math.max(...counts) - Math.min(...counts) <= 1, JSON.stringify(at));
});
