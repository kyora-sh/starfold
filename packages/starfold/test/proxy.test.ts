import { expect, test } from 'bun:test';
import { spawn, spawnSync } from 'node:child_process';
import { chmodSync, mkdtempSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import type { AddressInfo } from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const here = import.meta.dir;
const node = Bun.which('node');

// Node 22.21 and later take HTTP_PROXY from the environment with NODE_USE_ENV_PROXY=1;
// startServer's poll of the server it started must go straight to it all the same. Without such
// a Node on PATH the test fails rather than skips (CI's web job installs one).
function nodeVersion(): number[] | null {
	if (node === null) return null;
	const v = spawnSync(node, ['--version'], { encoding: 'utf8' }).stdout ?? '';
	const m = /^v(\d+)\.(\d+)\./.exec(v.trim());
	return m ? [Number(m[1]), Number(m[2])] : null;
}

test.if(process.platform !== 'win32')(
	'under Node with NODE_USE_ENV_PROXY=1 the readiness poll goes straight to the server',
	async () => {
		const v = nodeVersion();
		if (v === null || v[0]! < 22 || (v[0] === 22 && v[1]! < 21)) {
			throw new Error(
				`this test needs Node 22.21 or newer on PATH (found ${node === null ? 'none' : v?.join('.')})`
			);
		}
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
			child.once('close', (c) => ok(c))
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
