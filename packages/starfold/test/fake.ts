// An HTTP server that answers like the starfold server and keeps the requests it saw.

export const RID = '5f0c2a9e41b7d36c8e0f1a2b3c4d5e6f';

function be32(n: number): number[] {
	return [(n >>> 24) & 255, (n >>> 16) & 255, (n >>> 8) & 255, n & 255];
}

export const PNG = new Uint8Array([
	0x89,
	0x50,
	0x4e,
	0x47,
	0x0d,
	0x0a,
	0x1a,
	0x0a,
	0,
	0,
	0,
	13,
	...new TextEncoder().encode('IHDR'),
	...be32(1280),
	...be32(800),
	8,
	2,
	0,
	0,
	0,
]);

const CRC = Array.from({ length: 256 }, (_, n) => {
	let c = n;
	for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
	return c >>> 0;
});

function crc32(b: Uint8Array): number {
	let c = 0xffffffff;
	for (const x of b) c = CRC[(c ^ x) & 255]! ^ (c >>> 8);
	return (c ^ 0xffffffff) >>> 0;
}

async function deflate(b: Uint8Array): Promise<Uint8Array> {
	const s = new Blob([b])
		.stream()
		.pipeThrough(new CompressionStream('deflate-raw'));
	return new Uint8Array(await new Response(s).arrayBuffer());
}

function le16(n: number): number[] {
	return [n & 255, (n >>> 8) & 255];
}

function le32(n: number): number[] {
	return [n & 255, (n >>> 8) & 255, (n >>> 16) & 255, (n >>> 24) & 255];
}

/** A zip of `files`, stored or deflated. */
export async function zip(
	files: [string, Uint8Array][],
	deflated = false
): Promise<Uint8Array> {
	const local: number[] = [];
	const central: number[] = [];
	const bodies = await Promise.all(
		files.map(([, data]) => (deflated ? deflate(data) : data))
	);
	for (const [i, [name, data]] of files.entries()) {
		const nameBytes = [...new TextEncoder().encode(name)];
		const body = bodies[i]!;
		const method = deflated ? 8 : 0;
		const crc = crc32(data);
		const offset = local.length;
		local.push(
			...le32(0x04034b50),
			...le16(20),
			...le16(0),
			...le16(method),
			...le32(0),
			...le32(crc),
			...le32(body.length),
			...le32(data.length),
			...le16(nameBytes.length),
			...le16(0),
			...nameBytes,
			...body
		);
		central.push(
			...le32(0x02014b50),
			...le16(20),
			...le16(20),
			...le16(0),
			...le16(method),
			...le32(0),
			...le32(crc),
			...le32(body.length),
			...le32(data.length),
			...le16(nameBytes.length),
			...le16(0),
			...le16(0),
			...le16(0),
			...le16(0),
			...le32(0),
			...le32(offset),
			...nameBytes
		);
	}
	const end = [
		...le32(0x06054b50),
		...le16(0),
		...le16(0),
		...le16(files.length),
		...le16(files.length),
		...le32(central.length),
		...le32(local.length),
		...le16(0),
	];
	return new Uint8Array([...local, ...central, ...end]);
}

export interface Seen {
	method: string;
	path: string;
	params: Record<string, string>;
	headers: Headers;
	body: Uint8Array;
}

export class FakeServer {
	password = 'pw';
	seen: Seen[] = [];
	readOnly = false;
	recordingOn = true;
	segments: Uint8Array[] = [new TextEncoder().encode('mp4')];
	endReason = 'stopped';
	contentType = 'application/zip';
	seqHeader = 'x-starfold-seq';
	deflated = false;
	delayMs = 0;
	handler: ((req: Request) => Response | Promise<Response>) | null = null;
	readonly server: ReturnType<typeof Bun.serve>;

	constructor() {
		this.server = Bun.serve({
			port: 0,
			hostname: '127.0.0.1',
			fetch: (req) => this.#answer(req),
		});
	}

	get url(): string {
		return `http://127.0.0.1:${this.server.port}`;
	}

	stop(): void {
		this.server.stop(true);
	}

	get last(): Seen {
		return this.seen[this.seen.length - 1]!;
	}

	lastJson(): unknown {
		return JSON.parse(new TextDecoder().decode(this.last.body));
	}

	async recording(parts: Uint8Array[]): Promise<Response> {
		const names = parts.map((_, i) => `segment-${i + 1}.mp4`);
		const manifest = {
			end_reason: this.endReason,
			segments: names.map((file) => ({
				file,
				start_ms: 0,
				duration_ms: 1,
				frames: 1,
			})),
		};
		const body = await zip(
			[
				['manifest.json', new TextEncoder().encode(JSON.stringify(manifest))],
				...names.map((n, i): [string, Uint8Array] => [n, parts[i]!]),
			],
			this.deflated
		);
		return new Response(body, {
			headers: { 'content-type': this.contentType },
		});
	}

	async #answer(req: Request): Promise<Response> {
		const u = new URL(req.url);
		const q = Object.fromEntries(u.searchParams);
		const body = new Uint8Array(await req.arrayBuffer());
		this.seen.push({
			method: req.method,
			path: u.pathname,
			params: q,
			headers: req.headers,
			body,
		});
		if (this.delayMs) await new Promise((r) => setTimeout(r, this.delayMs));
		if (this.handler) return this.handler(req);
		if (req.headers.get('authorization') !== `Bearer ${this.password}`) {
			return new Response('starfold needs its password', { status: 401 });
		}
		const path = u.pathname;
		if (path === '/screenshot') {
			return new Response(PNG, { headers: { [this.seqHeader]: '41' } });
		}
		if (path === '/wait') {
			return Response.json({
				changed: 'since' in q,
				timed_out: false,
				seq: 42,
				waited_ms: 7,
			});
		}
		if (path === '/input') {
			if (this.readOnly) {
				return Response.json(
					{
						error: 'this server is read-only (--read-only): input is refused',
						read_only: true,
					},
					{ status: 403 }
				);
			}
			return Response.json({
				seq: 40,
				events: (JSON.parse(new TextDecoder().decode(body)) as unknown[])
					.length,
			});
		}
		if (path.startsWith('/record') && !this.recordingOn) {
			return Response.json(
				{
					error: 'recording is off (start the server with --enable-recording)',
					recording: false,
				},
				{ status: 403 }
			);
		}
		if (path === '/record') {
			if (q.format !== 'zip') return new Response('format', { status: 400 });
			return this.recording([new TextEncoder().encode(`mp4:${q.seconds}`)]);
		}
		if (path === '/record/start') {
			return Response.json({ id: RID, max_seconds: 300 });
		}
		if (path === '/record/stop') {
			if (q.id !== RID)
				return Response.json({ error: 'no such recording' }, { status: 404 });
			if (q.format !== 'zip') return new Response('format', { status: 400 });
			return this.recording(this.segments);
		}
		if (path === '/files/list') {
			return Response.json({
				path: q.path,
				entries: [{ name: 'a.txt', dir: false, size: 3, mtime: 1700000000000 }],
			});
		}
		if (path === '/files/download') {
			if (q.path === 'ok.txt') return new Response('abc');
			return Response.json(
				{ error: 'downloads are disabled' },
				{ status: 403 }
			);
		}
		if (path === '/files/upload') {
			if (
				req.headers.get('x-starfold-upload') !== '1' ||
				req.headers.get('x-voxl-upload') !== '1'
			) {
				return new Response('no upload header', { status: 400 });
			}
			return Response.json({ path: `Downloads/${q.name}`, size: body.length });
		}
		return Response.json({ error: 'not found' }, { status: 404 });
	}
}
