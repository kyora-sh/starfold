export const KEY_CAP_MS = 5000;
/**
 * Pairs keys with the tags read off the screen, in order, as many as fit: tags hold a letter, not a sequence
 * number, so a lost key must not take the same letter typed later and skip the keys between. A tag answers a
 * key only inside the cap; a key left without one is missed. Of equal pairings, the one with the shortest
 * latencies wins. keys [[t, letter]], tags [[t, 0-25]].
 */
export function answer(keys, tags) {
	const n = keys.length, m = tags.length;
	const fits = (i, j) => tags[j][1] === keys[i][1].charCodeAt(0) - 97 && tags[j][0] >= keys[i][0] && tags[j][0] < keys[i][0] + KEY_CAP_MS;
	const best = Array.from({ length: n + 1 }, () => new Int32Array(m + 1));
	for (let i = n - 1; i >= 0; i--)
		for (let j = m - 1; j >= 0; j--)
			best[i][j] = Math.max(best[i + 1][j], best[i][j + 1], fits(i, j) ? best[i + 1][j + 1] + 1 : 0);
	const i2p = [];
	let i = 0, j = 0;
	while (i < n && j < m) {
		if (best[i + 1][j] === best[i][j]) i++;
		else if (fits(i, j) && best[i + 1][j + 1] + 1 === best[i][j]) i2p.push(tags[j++][0] - keys[i++][0]);
		else j++;
	}
	return { i2p, missing: n - i2p.length };
}
