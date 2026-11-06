import assert from 'node:assert/strict';
import test from 'node:test';
import { answer, delivered } from './match.mjs';
const keys = (s, gap = 350) => [...s].map((k, i) => [i * gap, k]);
const tag = (k, t) => [t, k.charCodeAt(0) - 97];
test('every key read', () => assert.deepEqual(answer(keys('abc'), [tag('a', 100), tag('b', 450), tag('c', 800)]), { i2p: [100, 100, 100], noOwnFrame: 0 }));
test('a lost key leaves the keys after it alone', () => {
	const r = answer(keys('axyza'), [tag('x', 450), tag('y', 800), tag('z', 1150), tag('a', 1500)]);
	assert.deepEqual(r, { i2p: [100, 100, 100, 100], noOwnFrame: 1 });
});
test('a retyped letter goes to the later key when both can have it', () => assert.deepEqual(answer(keys('aba'), [tag('a', 800)]), { i2p: [100], noOwnFrame: 2 }));
test('a tag past the cap answers nothing', () => assert.deepEqual(answer(keys('a'), [tag('a', 5000)]), { i2p: [], noOwnFrame: 1 }));
test('a tag from before the key answers nothing', () => assert.deepEqual(answer([[1000, 'a']], [tag('a', 900)]), { i2p: [], noOwnFrame: 1 }));
test('every key delivered in order', () => assert.equal(delivered([...'abca'], [...'abca']), 4));
test('a key lost before the source', () => assert.equal(delivered([...'abca'], [...'aca']), 3));
test('delivered out of order counts once', () => assert.equal(delivered([...'ab'], [...'ba']), 1));
