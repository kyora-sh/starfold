// node webcodecs-decode.mjs <stream.h264s>...   (HW=prefer-hardware,prefer-software)
// Decodes streams written by the encoder's dump_streams test through WebCodecs in Chrome,
// configured as packages/web/src/worker.ts does (codec string from the first SPS), and
// fails unless every access unit decodes to a frame without a decoder error.
import { readFileSync } from 'fs';
import { createServer } from 'http';
import { createRequire } from 'module';
const require = createRequire(process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/');
const { chromium } = require('playwright');

const browser = await chromium.launch({
	executablePath: process.env.CHROMIUM || 'chromium',
	args: ['--no-sandbox'],
});
// WebCodecs needs a secure context, which localhost is and about:blank isn't.
const server = createServer((_, res) => res.end('<!doctype html>')).listen(0, '127.0.0.1');
await new Promise((r) => server.once('listening', r));
const page = await browser.newPage();
page.on('console', (m) => console.log('[page]', m.text()));
await page.goto(`http://127.0.0.1:${server.address().port}/`);
let failed = false;
const modes = (process.env.HW || 'no-preference').split(',');
for (const [file, hw] of process.argv.slice(2).flatMap((f) => modes.map((m) => [f, m]))) {
	const b64 = readFileSync(file).toString('base64');
	const r = await page.evaluate(async ([b64, hw]) => {
		const d = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
		const units = [];
		for (let i = 0; i + 5 <= d.length; ) {
			const n = ((d[i] << 24) | (d[i + 1] << 16) | (d[i + 2] << 8) | d[i + 3]) >>> 0;
			units.push({ key: d[i + 4] === 1, data: d.subarray(i + 5, i + 5 + n) });
			i += 5 + n;
		}
		const hex2 = (x) => x.toString(16).padStart(2, '0');
		const sps = (u) => {
			for (let i = 0; i + 6 < u.length; i++) {
				if (u[i] === 0 && u[i + 1] === 0 && u[i + 2] === 1 && (u[i + 3] & 0x1f) === 7) {
					return 'avc1.' + hex2(u[i + 4]) + hex2(u[i + 5]) + hex2(u[i + 6]);
				}
			}
			return null;
		};
		const codec = sps(units[0].data);
		const config = { codec, optimizeForLatency: true, hardwareAcceleration: hw };
		const support = await VideoDecoder.isConfigSupported(config);
		if (!support.supported) return { codec, error: 'unsupported' };
		const errors = [];
		let frames = 0;
		let size = null;
		const dec = new VideoDecoder({
			output: (f) => {
				frames++;
				size = [f.displayWidth, f.displayHeight];
				f.close();
			},
			error: (e) => errors.push(e.message),
		});
		dec.configure(config);
		units.forEach((u, i) => {
			try {
				dec.decode(
					new EncodedVideoChunk({ type: u.key ? 'key' : 'delta', timestamp: i * 33333, data: u.data }),
				);
			} catch (e) {
				errors.push(String(e));
			}
		});
		try {
			await dec.flush();
		} catch (e) {
			errors.push(String(e));
		}
		const keys = units.filter((u) => u.key).length;
		return { hw, codec, units: units.length, keys, frames, size, errors };
	}, [b64, hw]);
	const ok = !r.error && r.errors.length === 0 && r.frames === r.units;
	failed ||= !ok;
	console.log(ok ? 'ok' : 'FAIL', file, JSON.stringify(r));
}
await browser.close();
server.close();
process.exit(failed ? 1 : 0);
