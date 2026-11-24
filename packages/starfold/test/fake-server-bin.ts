// Stands in for the server binary in the startServer tests: reads its flags as the server does
// (--name=value, or --name value when the value doesn't start with --), listens on --addr/--port
// under --base-path, needs $STARFOLD_PASSWORD (an empty one counts as none: a random one, as
// the server makes up) for anything but /healthz, and writes its pid, flags and password to
// $FAKE_RECORD. FAKE_EXIT=<code> makes it print to stderr and exit before listening; with
// FAKE_TAKEN=<file>, while that file exists, it removes it and exits as on a taken port; with
// FAKE_STALL=1 /healthz sends its headers and then nothing more.
import { existsSync, rmSync, writeFileSync } from 'node:fs';

const argv = process.argv.slice(2);
const flags: Record<string, string> = {};
for (let i = 0; i < argv.length; i++) {
	const a = argv[i]!;
	if (!a.startsWith('--')) continue;
	const body = a.slice(2);
	const eq = body.indexOf('=');
	if (eq >= 0) {
		flags[body.slice(0, eq).replaceAll('_', '-')] = body.slice(eq + 1);
	} else if (i + 1 < argv.length && !argv[i + 1]!.startsWith('--')) {
		flags[body.replaceAll('_', '-')] = argv[++i]!;
	} else {
		flags[body.replaceAll('_', '-')] = 'true';
	}
}
if (process.env.FAKE_EXIT) {
	console.error('fake: cannot open display');
	process.exit(Number(process.env.FAKE_EXIT));
}
const taken = process.env.FAKE_TAKEN;
if (taken && existsSync(taken)) {
	rmSync(taken);
	console.error(
		`listening on ${flags.addr}:${flags.port}: Address already in use (os error 98)`
	);
	process.exit(1);
}
const base = (flags['base-path'] ?? '').replace(/\/$/, '');
const password =
	process.env.STARFOLD_PASSWORD || Math.random().toString(36).slice(2);
const server = Bun.serve({
	hostname: flags.addr,
	port: Number(flags.port),
	fetch(req) {
		const path = new URL(req.url).pathname;
		if (path === `${base}/healthz`) {
			if (process.env.FAKE_STALL) {
				return new Response(new ReadableStream({ start() {} }), {
					headers: { 'content-type': 'text/plain' },
				});
			}
			return new Response('ok');
		}
		if (!path.startsWith(`${base}/`)) {
			return new Response('not found', { status: 404 });
		}
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
		argv,
		password,
		port: server.port,
	})
);
console.error(`listening on http://${flags.addr}:${server.port}${base}`);
process.on('SIGTERM', () => {
	server.stop(true);
	process.exit(0);
});
