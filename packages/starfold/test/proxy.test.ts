import { expect, test } from 'bun:test';
import { spawn, spawnSync } from 'node:child_process';
import { chmodSync, mkdtempSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import type { AddressInfo } from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

// startServer under Node, where its readiness poll meets Node's own HTTP client: these need
// Node 22.21 or newer on PATH (NODE_USE_ENV_PROXY=1) and fail rather than skip without it
// (CI's web job installs one).
const here = import.meta.dir;
const node = Bun.which('node');
const posix = process.platform !== 'win32';

function needNode(): string {
	const v = node
		? (spawnSync(node, ['--version'], { encoding: 'utf8' }).stdout ?? '')
		: '';
	const m = /^v(\d+)\.(\d+)\./.exec(v.trim());
	const [major, minor] = m ? [Number(m[1]), Number(m[2])] : [0, 0];
	if (node === null || major < 22 || (major === 22 && minor < 21)) {
		throw new Error(
			`this test needs Node 22.21 or newer on PATH (found ${node === null ? 'none' : v.trim() || 'an unknown version'})`
		);
	}
	return node;
}

/** Runs startServer(options) in a Node child with env added; resolves with what it printed. */
async function underNode(
	options: string,
	env: Record<string, string> = {}
): Promise<{ out: string; code: number | null }> {
	const bin = needNode();
	const dir = mkdtempSync(join(tmpdir(), 'sf-node-'));
	const fake = join(dir, 'fake-starfold');
	writeFileSync(
		fake,
		`#!/bin/sh\nexec "${bin}" "${join(here, 'fake-server-node.mjs')}" "$@"\n`
	);
	chmodSync(fake, 0o755);
	const script = `
const { startServer } = await import(${JSON.stringify(resolve(here, '../dist/index.js'))});
const t = Date.now();
try {
	const s = await startServer({ binary: ${JSON.stringify(fake)}, ...${options} });
	console.log('started', s.url);
	await s.close();
} catch (e) {
	console.log('failed:', e.name, Date.now() - t < 5000 ? 'in time' : 'late');
	process.exitCode = 1;
}
`;
	const child = spawn(bin, ['--input-type=module', '-e', script], {
		env: { ...process.env, ...env },
		stdio: ['ignore', 'pipe', 'inherit'],
	});
	let out = '';
	child.stdout!.on('data', (d) => (out += String(d)));
	const code = await new Promise<number | null>((ok) =>
		child.once('close', (c) => ok(c))
	);
	return { out: out.trim(), code };
}

test.if(posix)(
	'under Node with NODE_USE_ENV_PROXY=1 the readiness poll goes straight to the server',
	async () => {
		needNode();
		const seen: string[] = [];
		const proxy = createServer((sock) => {
			sock.once('data', (d) => {
				seen.push(String(d).split('\r\n')[0]!);
				sock.destroy();
			});
		});
		await new Promise<void>((ok) => proxy.listen(0, '127.0.0.1', () => ok()));
		const proxyUrl = `http://127.0.0.1:${(proxy.address() as AddressInfo).port}`;
		const { out, code } = await underNode('{ timeoutMs: 3000 }', {
			NODE_USE_ENV_PROXY: '1',
			HTTP_PROXY: proxyUrl,
			http_proxy: proxyUrl,
			NO_PROXY: '',
			no_proxy: '',
		});
		proxy.close();
		expect({ out, seen }).toEqual({
			out: expect.stringMatching(/^started http:\/\/127\.0\.0\.1:\d+$/),
			seen: [],
		});
		expect(code).toBe(0);
	},
	15_000
);

test.if(posix)(
	'under Node a server that sends its headers and then stalls fails within the timeout',
	async () => {
		const { out } = await underNode('{ timeoutMs: 1500 }', { FAKE_STALL: '1' });
		expect(out).toBe('failed: ServerStartError in time');
	},
	15_000
);
