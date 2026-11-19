// Everything mount() sets up that has to be undone, as one list: run() undoes each in the order
// it was registered, once, even if one of them throws.

export class Disposers {
	private list: { name: string; undo: () => void }[] = [];
	private done = false;
	/** Names, in the order run() undid them (for tests). */
	readonly undone: string[] = [];

	/** Registers `undo` under `name`; after run(), it runs at once. */
	add(name: string, undo: () => void) {
		if (this.done) {
			undo();
			this.undone.push(name);
			return;
		}
		this.list.push({ name, undo });
	}

	get registered(): string[] {
		return this.list.map((d) => d.name);
	}

	run() {
		if (this.done) return;
		this.done = true;
		let first: unknown = null;
		for (const d of this.list) {
			try {
				d.undo();
			} catch (e) {
				first ??= e;
			}
			this.undone.push(d.name);
		}
		if (first) throw first;
	}
}
