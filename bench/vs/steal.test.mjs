import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';
import { stealBetween } from './steal.mjs';
test('share of the ticks in the window', () => assert.equal(stealBetween({ steal: 10, all: 1000 }, { steal: 15, all: 1200 }), 2.5));
test('no ticks is not a zero', () => assert.ok(Number.isNaN(stealBetween({ steal: 1, all: 5 }, { steal: 1, all: 5 }))));

const src = fs.readFileSync(new URL('./driver.mjs', import.meta.url), 'utf8');
const quality = src.slice(src.indexOf('async function quality('), src.indexOf('async function soak('));
const H0 = ", h0 = await ctlJson('/cpu')";
const H1 = "const h1 = await ctlJson('/cpu');\n\t\t";
/** Throws unless the row's steal covers the start of motion to the last settle point. */
function checkWindow(body) {
	assert.match(body, /stealPct: stealBetween\(h0\.host, h1\.host\)/);
	const steps = [`const s0 = await serverStats(arm)${H0};`, "if (wl === 'typing')", 'for (let i = 0; i < 6; i++)', 'const s1 = await serverStats(arm);', "await ctl('/freeze');", 'for (const ms of SETTLE_MS)', 'polling = false;', `await poller;\n\t\t${H1}`, '/truth?out='];
	const at = steps.map((x) => body.indexOf(x));
	assert.ok(at.every((x, i) => x >= 0 && (i === 0 || x > at[i - 1])), `window out of order: ${at}`);
	for (const read of ['h0 =', 'h1 =']) assert.equal(body.split(read).length, 2, read);
}
test('quality rows take steal from the start of motion to the last settle point', () => checkWindow(quality));

const moveH0 = (before) => quality.replace(H0, '').replace(before, `const h0 = await ctlJson('/cpu');\n\t\t${before}`);
const moveH1 = (before) => quality.replace(H1, '').replace(before, `${H1}${before}`);
const mutations = {
	'h0 at the start of the motion branch': moveH0("if (wl === 'typing')"),
	'h0 inside the motion loop': moveH0('for (let i = 0; i < 6; i++)'),
	'h0 after motion': moveH0('const s1 = await serverStats(arm);'),
	'h0 right before the freeze': moveH0("await ctl('/freeze');"),
	'h0 right before the settle loop': moveH0('for (const ms of SETTLE_MS)'),
	'h0 at the end of the window': moveH0(H1),
	'h1 before motion': moveH1("if (wl === 'typing')"),
	'h1 after motion': moveH1('const s1 = await serverStats(arm);'),
	'h1 right before the freeze': moveH1("await ctl('/freeze');"),
	'h1 right before the settle loop': moveH1('for (const ms of SETTLE_MS)'),
	'h1 inside the settle loop': moveH1('shotAt[ms] = Date.now() - t0;'),
	'h1 before the poller ends': moveH1('polling = false;'),
	'h1 after the truth capture': moveH1('const crops ='),
	'readings swapped': quality.replace('stealBetween(h0.host, h1.host)', 'stealBetween(h1.host, h0.host)'),
	'wrong field': quality.replace('stealBetween(h0.host, h1.host)', 'stealBetween(h0.x, h1.x)'),
};
for (const [name, body] of Object.entries(mutations))
	test(`a moved read fails: ${name}`, () => {
		assert.notEqual(body, quality);
		assert.throws(() => checkWindow(body));
	});
