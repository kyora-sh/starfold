// Stands in for the server binary in the startServer tests: listens on --addr/--port, needs
// $STARFOLD_PASSWORD for anything but /healthz, and writes its pid, flags and password to
// $FAKE_RECORD. FAKE_EXIT=<code> makes it print to stderr and exit before listening.
import { writeFileSync } from 'node:fs';

const flags = Object.fromEntries(
	process.argv.slice(2).map((a) => {
		const [k, v] = a.replace(/^--/, '').split('=');
		return [k, v ?? 'true'];
	})
);
if (process.env.FAKE_EXIT) {
	console.error('fake: cannot open display');
	process.exit(Number(process.env.FAKE_EXIT));
}
const password = process.env.STARFOLD_PASSWORD;
const server = Bun.serve({
	hostname: flags.addr,
	port: Number(flags.port),
	fetch(req) {
		const path = new URL(req.url).pathname;
		if (path === '/healthz') return new Response('ok');
		if (req.headers.get('authorization') !== `Bearer ${password}`) {
			return new Response('starfold needs its password', { status: 401 });
		}
		return Response.json({ seq: 1, events: 1 });
	},
});
writeFileSync(
	process.env.FAKE_RECORD!,
	JSON.stringify({
		pid: process.pid,
		argv: process.argv.slice(2),
		password,
		port: server.port,
	})
);
console.error(`listening on http://${flags.addr}:${server.port}`);
process.on('SIGTERM', () => {
	server.stop(true);
	process.exit(0);
});
