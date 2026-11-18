// Embedded: the embedder's preamble, asked for before every connect. Its messages go to the
// worker, which sends them first on the socket it opens next; nothing here keeps or logs them.
import type { ToWorker } from '../protocol';

/** Text message(s) to send before anything of starfold's: a ticket, say. Called before every connect. */
export type Preamble = () => string | string[] | Promise<string | string[]>;

/**
 * Calls `preamble` (none: nothing to send) and posts its messages to the worker. If it throws,
 * rejects or gives anything but text, the worker is told that nothing opens, and `failed` gets
 * the error; the error itself is passed on, not logged.
 */
export async function answerPreamble(
	preamble: Preamble | undefined,
	post: (m: ToWorker) => void,
	failed: (err: unknown) => void
): Promise<void> {
	let msgs: string[];
	try {
		const given = preamble ? await preamble() : [];
		msgs = typeof given === 'string' ? [given] : given;
		if (!Array.isArray(msgs) || msgs.some((m) => typeof m !== 'string'))
			throw new TypeError('the preamble must give a string or an array of strings');
	} catch (err) {
		post({ t: 'preambleFailed' });
		failed(err);
		return;
	}
	post({ t: 'preamble', msgs });
}
