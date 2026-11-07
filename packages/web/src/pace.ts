/** The display's refresh rate from `requestAnimationFrame` timestamps: the mean of the middle
 * half of the intervals (so a missed or doubled frame doesn't count), as whole frames per
 * second. Undefined with too few to tell. */
export function displayHz(stamps: number[]): number | undefined {
	if (stamps.length < 10) return undefined;
	const d = stamps
		.slice(1)
		.map((t, i) => t - stamps[i])
		.filter((x) => x > 0);
	d.sort((a, b) => a - b);
	if (d.length < 9) return undefined;
	const mid = d.slice(d.length >> 2, d.length - (d.length >> 2));
	return Math.round((1000 * mid.length) / mid.reduce((a, b) => a + b, 0));
}

/** Chunks a stripe decoder may hold at the least: 16, as at 60 fps. */
export const MIN_QUEUED = 16;
/** Time's worth of frames a stripe decoder may hold before it is taken as falling behind. */
export const QUEUE_MS = 250;

/** The rate frames arrive at, and how many of them a decoder may hold. */
export class StreamRate {
	private last: number | undefined;
	private intervalMs: number | undefined;

	/** A new frame arrived at `now` (ms); gaps of a second or more are stills. */
	note(now: number) {
		const gap = this.last === undefined ? undefined : now - this.last;
		this.last = now;
		if (gap === undefined || gap <= 0 || gap >= 1000) return;
		this.intervalMs =
			this.intervalMs === undefined ? gap : this.intervalMs * 0.9 + gap * 0.1;
	}

	maxQueued(): number {
		if (this.intervalMs === undefined) return MIN_QUEUED;
		return Math.max(MIN_QUEUED, Math.round(QUEUE_MS / this.intervalMs));
	}
}

/** Within this long, a second fall-behind says the decoder can't keep up. */
export const SLOW_WINDOW_MS = 10_000;

/** Tells once, at the second time the decoder fell behind within SLOW_WINDOW_MS. */
export class FellBehind {
	private at: number | undefined;
	private told = false;

	note(now: number): boolean {
		const again = this.at !== undefined && now - this.at <= SLOW_WINDOW_MS;
		this.at = now;
		if (!again || this.told) return false;
		this.told = true;
		return true;
	}
}
