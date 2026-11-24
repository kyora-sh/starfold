import { expect, test } from 'bun:test';
import { spawn } from 'node:child_process';
import { chmodSync, mkdtempSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import type { AddressInfo } from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const here = import.meta.dir;
const node = Bun.which('node');

// Node takes HTTP_PROXY from the environment with NODE_USE_ENV_PROXY=1; startServer's poll of
// the server it started must go straight to it all the same.
test.if(process.platform !== 'win32' && node !== null)(
	'under Node with NODE_USE_ENV_PROXY=1 the readiness poll goes straight to the server',
	async () => {
		const seen: string[] = [];
		const proxy = createServer((sock) => {
			sock.once('data', (d) => {
				seen.push(String(d).split('\r\n')[0]!);
				sock.destroy();
			});
		});
		await new Promise<void>((ok) => proxy.listen(0, '127.0.0.1', () => ok()));
		const proxyUrl = `http://127.0.0.1:${(proxy.address() as AddressInfo).port}`;
		const dir = mkdtempSync(join(tmpdir(), 'sf-proxy-'));
		const bin = join(dir, 'fake-starfold');
		writeFileSync(
			bin,
			`#!/bin/sh\nexec "${node}" "${join(here, 'fake-server-node.mjs')}" "$@"\n`
		);
		chmodSync(bin, 0o755);
		const script = `
const { startServer } = await import(${JSON.stringify(resolve(here, '../dist/index.js'))});
try {
	const s = await startServer({ binary: ${JSON.stringify(bin)}, timeoutMs: 3000 });
	console.log('started', s.url);
	await s.close();
} catch (e) {
	console.log('failed:', e.message.split('\\n')[0]);
	process.exitCode = 1;
}
`;
		const child = spawn(node!, ['--input-type=module', '-e', script], {
			env: {
				...process.env,
				NODE_USE_ENV_PROXY: '1',
				HTTP_PROXY: proxyUrl,
				http_proxy: proxyUrl,
				NO_PROXY: '',
				no_proxy: '',
			},
			stdio: ['ignore', 'pipe', 'inherit'],
		});
		let out = '';
		child.stdout!.on('data', (d) => (out += String(d)));
		const code = await new Promise<number | null>((ok) =>
			child.once('exit', (c) => ok(c))
		);
		proxy.close();
		expect({ out: out.trim(), seen }).toEqual({
			out: expect.stringMatching(/^started http:\/\/127\.0\.0\.1:\d+$/),
			seen: [],
		});
		expect(code).toBe(0);
	},
	15_000
);
