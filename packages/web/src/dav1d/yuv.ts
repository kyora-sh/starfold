// Draws 8-bit 4:2:0 planes onto a 2D canvas context: through WebGL2 on a scratch
// OffscreenCanvas, or converted on the CPU where WebGL2 is unavailable.

export interface Planes {
	heap: Uint8Array;
	strideY: number;
	strideUV: number;
	y: number;
	u: number;
	v: number;
	/** AV1 matrix_coefficients: 1 BT.709, 5 or 6 BT.601; anything else is taken as BT.709. */
	matrix: number;
	fullRange: boolean;
}

/** Normalized Y'CbCr to R'G'B': rgb = m (rows) * (yuv - off). */
export function yuvToRgb(
	matrix: number,
	fullRange: boolean
): { m: number[]; off: number[] } {
	const [kr, kb] =
		matrix === 5 || matrix === 6 ? [0.299, 0.114] : [0.2126, 0.0722];
	const kg = 1 - kr - kb;
	const ys = fullRange ? 1 : 255 / 219;
	const cs = fullRange ? 1 : 255 / 224;
	return {
		m: [
			ys,
			0,
			2 * (1 - kr) * cs,
			ys,
			((-2 * kb * (1 - kb)) / kg) * cs,
			((-2 * kr * (1 - kr)) / kg) * cs,
			ys,
			2 * (1 - kb) * cs,
			0,
		],
		off: [fullRange ? 0 : 16 / 255, 128 / 255, 128 / 255],
	};
}

const VS = `#version 300 es
out vec2 uv;
void main() {
	vec2 p = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
	uv = vec2(p.x, 1.0 - p.y);
	gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;
const FS = `#version 300 es
precision highp float;
uniform sampler2D ty, tu, tv;
uniform mat3 m;
uniform vec3 off;
in vec2 uv;
out vec4 color;
void main() {
	vec3 yuv = vec3(texture(ty, uv).r, texture(tu, uv).r, texture(tv, uv).r);
	color = vec4(clamp(m * (yuv - off), 0.0, 1.0), 1.0);
}`;

interface Gl {
	gl: WebGL2RenderingContext;
	canvas: OffscreenCanvas;
	m: WebGLUniformLocation | null;
	off: WebGLUniformLocation | null;
	tex: WebGLTexture[];
}

function setupGl(): Gl | null {
	const canvas = new OffscreenCanvas(1, 1);
	const gl = canvas.getContext('webgl2', {
		alpha: false,
		antialias: false,
		depth: false,
		preserveDrawingBuffer: true,
	});
	if (!gl) return null;
	const shader = (type: number, src: string) => {
		const s = gl.createShader(type)!;
		gl.shaderSource(s, src);
		gl.compileShader(s);
		return s;
	};
	const prog = gl.createProgram()!;
	gl.attachShader(prog, shader(gl.VERTEX_SHADER, VS));
	gl.attachShader(prog, shader(gl.FRAGMENT_SHADER, FS));
	gl.linkProgram(prog);
	if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) return null;
	gl.useProgram(prog);
	gl.bindVertexArray(gl.createVertexArray());
	const tex = ['ty', 'tu', 'tv'].map((name, i) => {
		const t = gl.createTexture()!;
		gl.activeTexture(gl.TEXTURE0 + i);
		gl.bindTexture(gl.TEXTURE_2D, t);
		const filter = i ? gl.LINEAR : gl.NEAREST;
		gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, filter);
		gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, filter);
		gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
		gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
		gl.uniform1i(gl.getUniformLocation(prog, name), i);
		return t;
	});
	gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
	return {
		gl,
		canvas,
		m: gl.getUniformLocation(prog, 'm'),
		off: gl.getUniformLocation(prog, 'off'),
		tex,
	};
}

export class YuvRenderer {
	private readonly g: Gl | null;
	private image: ImageData | null = null;
	constructor() {
		try {
			this.g = setupGl();
		} catch {
			this.g = null;
		}
	}

	get backend(): 'webgl2' | '2d' {
		return this.g ? 'webgl2' : '2d';
	}

	/** Draws the top-left `w` x `h` of the picture at (0, `y`). */
	draw(
		ctx: OffscreenCanvasRenderingContext2D,
		p: Planes,
		y: number,
		w: number,
		h: number
	) {
		if (w <= 0 || h <= 0) return;
		if (this.g) this.drawGl(this.g, ctx, p, y, w, h);
		else this.draw2d(ctx, p, y, w, h);
	}

	private drawGl(
		g: Gl,
		ctx: OffscreenCanvasRenderingContext2D,
		p: Planes,
		y: number,
		w: number,
		h: number
	) {
		const { gl, canvas } = g;
		if (canvas.width !== w || canvas.height !== h) {
			canvas.width = w;
			canvas.height = h;
		}
		gl.viewport(0, 0, w, h);
		const cw = (w + 1) >> 1;
		const ch = (h + 1) >> 1;
		const planes: [number, number, number, number][] = [
			[p.y, p.strideY, w, h],
			[p.u, p.strideUV, cw, ch],
			[p.v, p.strideUV, cw, ch],
		];
		planes.forEach(([offset, stride, pw, ph], i) => {
			gl.activeTexture(gl.TEXTURE0 + i);
			gl.bindTexture(gl.TEXTURE_2D, g.tex[i]);
			gl.pixelStorei(gl.UNPACK_ROW_LENGTH, stride);
			gl.texImage2D(
				gl.TEXTURE_2D,
				0,
				gl.R8,
				pw,
				ph,
				0,
				gl.RED,
				gl.UNSIGNED_BYTE,
				p.heap,
				offset
			);
		});
		const { m, off } = yuvToRgb(p.matrix, p.fullRange);
		gl.uniformMatrix3fv(g.m, true, m);
		gl.uniform3fv(g.off, off);
		gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
		ctx.drawImage(canvas, 0, y);
	}

	private draw2d(
		ctx: OffscreenCanvasRenderingContext2D,
		p: Planes,
		y: number,
		w: number,
		h: number
	) {
		if (!this.image || this.image.width !== w || this.image.height !== h)
			this.image = new ImageData(w, h);
		const out = this.image.data;
		const { m, off } = yuvToRgb(p.matrix, p.fullRange);
		const H = p.heap;
		for (let r = 0; r < h; r++) {
			const yRow = p.y + r * p.strideY;
			const cRow = (r >> 1) * p.strideUV;
			for (let c = 0; c < w; c++) {
				const Y = H[yRow + c] / 255 - off[0];
				const U = H[p.u + cRow + (c >> 1)] / 255 - off[1];
				const V = H[p.v + cRow + (c >> 1)] / 255 - off[2];
				const o = (r * w + c) * 4;
				out[o] = (m[0] * Y + m[1] * U + m[2] * V) * 255;
				out[o + 1] = (m[3] * Y + m[4] * U + m[5] * V) * 255;
				out[o + 2] = (m[6] * Y + m[7] * U + m[8] * V) * 255;
				out[o + 3] = 255;
			}
		}
		ctx.putImageData(this.image, 0, y);
	}
}
