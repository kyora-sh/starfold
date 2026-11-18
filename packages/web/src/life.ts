// Embedded: the page and the worker run the connection's lifecycle (connect attempts, the
// preamble, open, close) over a MessagePort of their own, made by the page and handed over at
// init. Nothing the server sends is ever posted on it, so server data can't be read as a
// lifecycle message; the worker's ordinary channel carries only the stream.
import type { Link } from './link';

/** Page to worker. */
export type LifeDown =
	| { t: 'connect'; id: number }
	| { t: 'preamble'; id: number; msgs: string[] }
	| { t: 'preambleFailed'; id: number }
	| { t: 'cancel' };

/** Worker to page. */
export type LifeUp =
	| { t: 'preamble'; id: number }
	| { t: 'open'; id: number }
	| { t: 'closed'; id: number; code: number; reason: string }
	| { t: 'connectFailed'; id: number };

/** The worker's side: the page's requests go to `link`. */
export function serveLife(port: MessagePort, link: Link) {
	port.addEventListener('message', (e: MessageEvent<LifeDown>) => {
		const m = e.data;
		switch (m.t) {
			case 'connect':
				link.request(m.id);
				break;
			case 'preamble':
				link.preamble(m.id, m.msgs);
				break;
			case 'preambleFailed':
				link.refused(m.id);
				break;
			case 'cancel':
				link.cancel();
				break;
		}
	});
	port.start();
}
