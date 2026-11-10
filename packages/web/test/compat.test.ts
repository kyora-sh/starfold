import { expect, test } from 'bun:test';
import { aliasVoxlStats } from '../src/compat';

test('window.voxlStats reads starfoldStats and warns once', () => {
	const warnings: string[] = [];
	const w = { starfoldStats: { fps: 1 } as unknown };
	aliasVoxlStats(w, (m) => warnings.push(m));
	expect(warnings).toEqual([]);
	w.starfoldStats = { fps: 30 };
	expect((w as unknown as { voxlStats: unknown }).voxlStats).toEqual({
		fps: 30,
	});
	expect((w as unknown as { voxlStats: unknown }).voxlStats).toBe(
		w.starfoldStats
	);
	expect(warnings).toEqual([
		'[starfold] window.voxlStats is deprecated: use window.starfoldStats',
	]);
});
