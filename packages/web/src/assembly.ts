// Complete-frame assembly: a frame is the set of stripe updates sharing an id; frames
// are presented whole and in arrival order, at most `max` waiting at once.

export interface Closable {
	close(): void;
}

export interface Part<P extends Closable> {
	i: number;
	y: number;
	pic: P;
}

interface Pending<P extends Closable> {
	need: number;
	got: Part<P>[];
	t0: number;
}

export class FrameQueue<P extends Closable> {
	private readonly frames = new Map<number, Pending<P>>();
	private readonly order: number[] = [];
	private newest: number | null = null;
	private cutoff: number | null = null;
	constructor(private readonly max: number) {}

	get size(): number {
		return this.order.length;
	}

	has(id: number): boolean {
		return this.frames.has(id);
	}

	/** Notes a stripe update of frame `id`; false when that frame began before the last cut. */
	seen(id: number): boolean {
		if (this.newest === null || ((id - this.newest) | 0) > 0) this.newest = id;
		return this.cutoff === null || ((id - this.cutoff) | 0) >= 0;
	}

	/** A reset mid-stream: drops what waits, and the remaining stripes of every frame
	 * begun so far, which could never complete. Returns the newest frame id seen. */
	cut(): number | null {
		this.clear();
		if (this.newest !== null) this.cutoff = (this.newest + 1) >>> 0;
		return this.newest;
	}

	/** Registers frame `id` on its first stripe update; false when the queue is full. */
	open(id: number, parts: number, now: number): boolean {
		if (this.frames.has(id)) return true;
		if (this.order.length >= this.max) return false;
		this.frames.set(id, { need: parts, got: [], t0: now });
		this.order.push(id);
		return true;
	}

	/** One of the frame's stripes will not come (dropped, or nothing decoded). */
	skip(id: number) {
		const f = this.frames.get(id);
		if (f) f.need--;
	}

	/** A decoded stripe; false (and the picture is the caller's) when its frame is gone. */
	add(id: number, part: Part<P>): boolean {
		const f = this.frames.get(id);
		if (!f) return false;
		f.got.push(part);
		return true;
	}

	/** Presents every complete frame at the head of the queue, oldest first. */
	flush(present: (id: number, parts: Part<P>[], t0: number) => void) {
		while (this.order.length) {
			const id = this.order[0];
			const f = this.frames.get(id)!;
			if (f.got.length < f.need) return;
			this.order.shift();
			this.frames.delete(id);
			present(id, f.got, f.t0);
		}
	}

	clear() {
		for (const f of this.frames.values()) for (const g of f.got) g.pic.close();
		this.frames.clear();
		this.order.length = 0;
	}

	/** A new stream: frame ids start over. */
	reset() {
		this.clear();
		this.newest = null;
		this.cutoff = null;
	}
}

/** p50/p95 of the samples since the last take. */
export class Percentiles {
	private sorted: number[] = [];
	add(ms: number) {
		const s = this.sorted;
		if (s.length >= 10000) return;
		let lo = 0;
		let hi = s.length;
		while (lo < hi) {
			const mid = (lo + hi) >> 1;
			if (s[mid] <= ms) lo = mid + 1;
			else hi = mid;
		}
		s.splice(lo, 0, ms);
	}
	take(): { p50: number; p95: number } {
		const s = this.sorted;
		this.sorted = [];
		const at = (q: number) =>
			s.length ? s[Math.min(s.length - 1, Math.ceil(q * s.length) - 1)] : 0;
		return { p50: at(0.5), p95: at(0.95) };
	}
}
