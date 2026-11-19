import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import test from 'node:test';
import { armOf, baseArm, expandArms, roundOrder, setOrder, TWIN } from './arms.mjs';
import { ARMS, KYORA_VIEWER, KYORA_X11VNC, X11VNC_ARGS } from './config.mjs';

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
test('run 1 starts where run 0 would have gone on', () => {
	const arms = expandArms(['a', 'b', 'c', 'd']);
	assert.deepEqual(roundOrder(arms, 0, 0, 1, 4), roundOrder(arms, 4, 0, 0, 0));
	assert.deepEqual(roundOrder(arms, 0, 0, 2, 4), roundOrder(arms, 8, 0, 0, 0));
});
test('AA=0 keeps the old order whatever RUN says', () => {
	const order = setOrder(['novnc', 'starfold-video'], { aa: false, run: 1 });
	assert.deepEqual(order(0, 0, 1), ['novnc', 'starfold-video']);
	assert.deepEqual(order(1, 0, 1), ['starfold-video', 'novnc']);
	assert.deepEqual(setOrder(['a', 'b', 'c'], { run: 1 })(0, 0, 1), ['b', 'c', 'a']);
});
test('runs continue the rotation, so every arm takes every position', () => {
	const arms = expandArms(['a', 'b', 'c', 'd']);
	const at = Object.fromEntries(arms.map((x) => [x, arms.map(() => 0)]));
	for (let run = 0; run < 3; run++)
		for (let round = 0; round < 4; round++) roundOrder(arms, round, 0, run, 4).forEach((x, p) => at[x][p]++);
	for (const counts of Object.values(at)) assert.ok(Math.max(...counts) - Math.min(...counts) <= 1, JSON.stringify(at));
});
test('the vendored kyora viewer is the pinned file, unchanged', () => {
	const got = createHash('sha256').update(fs.readFileSync(new URL('./kyora/vms-viewer.js', import.meta.url))).digest('hex');
	assert.equal(got, KYORA_VIEWER.sha256);
});
test('kyora-rfb runs view only on the shared x11vnc, the noVNC arm the same flags without -viewonly', () => {
	assert.equal(ARMS['kyora-rfb'].x11vnc, KYORA_X11VNC.view);
	assert.ok(ARMS['kyora-rfb'].viewOnly && !ARMS['kyora-rfb'].control);
	assert.deepEqual([...KYORA_X11VNC.view].filter((f) => f !== '-viewonly').sort(), [...X11VNC_ARGS].sort());
});
test('kyora-rfb-control takes keys at x11vnc default timing, and neither kyora arm runs unless named', () => {
	const c = ARMS['kyora-rfb-control'];
	assert.ok(c.control && !c.viewOnly);
	for (const f of ['-viewonly', '-wait', '-defer', '-nonap']) assert.ok(!c.x11vnc.includes(f), f);
	assert.ok(ARMS['kyora-rfb'].probe && c.probe);
});
