// A WebSocket server for the link and embed tests: records every connection's path,
// subprotocols, messages and when it arrived.
export interface Conn {
	path: string;
	protocols: string | null;
	at: number;
	got: string[];
	closed: boolean;
	ws?: Bun.ServerWebSocket<number>;
}

export function fakeServer() {
	const conns: Conn[] = [];
	const server = Bun.serve<number>({
		port: 0,
		fetch(req, srv) {
			const u = new URL(req.url);
			const protocols = req.headers.get('sec-websocket-protocol');
			conns.push({
				path: u.pathname + u.search,
				protocols,
				at: Date.now(),
				got: [],
				closed: false,
			});
			const chosen = protocols?.split(',')[0].trim();
			return srv.upgrade(req, {
				data: conns.length - 1,
				headers: chosen ? { 'Sec-WebSocket-Protocol': chosen } : undefined,
			})
				? undefined
				: new Response('no upgrade', { status: 400 });
		},
		websocket: {
			open(ws) {
				conns[ws.data].ws = ws;
			},
			message(ws, m) {
				conns[ws.data].got.push(String(m));
			},
			close(ws) {
				conns[ws.data].closed = true;
			},
		},
	});
	return {
		conns,
		url: `ws://127.0.0.1:${server.port}/desk/ws`,
		stop: () => server.stop(true),
	};
}

export const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export function until(f: () => boolean, ms = 2000): Promise<void> {
	const end = Date.now() + ms;
	return new Promise((resolve, reject) => {
		const check = () => {
			if (f()) return resolve();
			if (Date.now() > end) return reject(new Error('timed out'));
			setTimeout(check, 5);
		};
		check();
	});
}
