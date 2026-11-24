/** Reads the members of a zip archive (stored or deflated), enough for a recording's segments. */
export async function unzip(
	data: Uint8Array
): Promise<Map<string, Uint8Array>> {
	const v = new DataView(data.buffer, data.byteOffset, data.byteLength);
	let end = -1;
	for (
		let i = data.length - 22;
		i >= Math.max(0, data.length - 22 - 0xffff);
		i--
	) {
		if (v.getUint32(i, true) === 0x06054b50) {
			end = i;
			break;
		}
	}
	if (end < 0) {
		throw new Error('not a zip archive');
	}
	const count = v.getUint16(end + 10, true);
	let p = v.getUint32(end + 16, true);
	const names = new TextDecoder();
	const out = new Map<string, Uint8Array>();
	for (let n = 0; n < count; n++) {
		if (v.getUint32(p, true) !== 0x02014b50) {
			throw new Error('a broken zip central directory');
		}
		const method = v.getUint16(p + 10, true);
		const size = v.getUint32(p + 20, true);
		const nameLen = v.getUint16(p + 28, true);
		const extraLen = v.getUint16(p + 30, true);
		const commentLen = v.getUint16(p + 32, true);
		const local = v.getUint32(p + 42, true);
		const name = names.decode(data.subarray(p + 46, p + 46 + nameLen));
		p += 46 + nameLen + extraLen + commentLen;
		if (v.getUint32(local, true) !== 0x04034b50) {
			throw new Error(`a broken zip entry: ${name}`);
		}
		const start =
			local +
			30 +
			v.getUint16(local + 26, true) +
			v.getUint16(local + 28, true);
		const raw = data.subarray(start, start + size);
		if (method === 0) {
			out.set(name, raw);
		} else if (method === 8) {
			const stream = new Blob([raw])
				.stream()
				.pipeThrough(new DecompressionStream('deflate-raw'));
			// oxlint-disable-next-line no-await-in-loop
			out.set(name, new Uint8Array(await new Response(stream).arrayBuffer()));
		} else {
			throw new Error(`zip entry ${name} uses compression method ${method}`);
		}
	}
	return out;
}
