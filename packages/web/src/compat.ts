// window.voxlStats, the stats' name before the rename: still readable, with one warning.
export function aliasVoxlStats(
	w: { starfoldStats: unknown },
	warn: (msg: string) => void = console.warn
) {
	let warned = false;
	Object.defineProperty(w, 'voxlStats', {
		configurable: true,
		get() {
			if (!warned) {
				warned = true;
				warn(
					'[starfold] window.voxlStats is deprecated: use window.starfoldStats'
				);
			}
			return w.starfoldStats;
		},
	});
}
