// The only way an embedder callback is ever called. Calls are queued (a microtask after the
// client's own code that asked for them has returned, never inside mount/connect/unmount), run
// one by one in order, and each inside try/catch: a throw is passed to reportError and changes
// nothing in the client, whose state changed before the call was queued. seal() drops
// everything still queued; after it, only the call given to it runs.

export type Report = (err: unknown) => void;

const defaultReport: Report = (err) => {
	if (typeof globalThis.reportError === 'function') globalThis.reportError(err);
	else
		setTimeout(() => {
			throw err;
		});
};

export class Dispatcher {
	private queue: (() => void)[] = [];
	private scheduled = false;
	private sealed = false;

	constructor(private readonly report: Report = defaultReport) {}

	/** Queues `call`; dropped once sealed. */
	call(call: () => void) {
		if (this.sealed) return;
		this.queue.push(call);
		this.schedule();
	}

	/** Drops every queued call, accepts no more, and queues `last` (if any) as the final one. */
	seal(last?: () => void) {
		this.sealed = true;
		this.queue = last ? [last] : [];
		if (last) this.schedule();
	}

	private schedule() {
		if (this.scheduled) return;
		this.scheduled = true;
		queueMicrotask(() => this.flush());
	}

	private flush() {
		this.scheduled = false;
		while (this.queue.length) {
			const call = this.queue.shift()!;
			try {
				call();
			} catch (err) {
				this.report(err);
			}
		}
	}
}
