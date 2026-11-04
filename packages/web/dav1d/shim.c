// The exports of dav1d.wasm: one single-threaded decoder per stripe, fed one temporal
// unit at a time, handing out 8-bit 4:2:0 pictures that stay valid until released.
#include <dav1d/dav1d.h>
#include <errno.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#define EXPORT(name) __attribute__((export_name(#name)))

typedef struct {
	uint32_t width;
	uint32_t height;
	uint32_t stride_y;
	uint32_t stride_uv;
	uint32_t y;
	uint32_t u;
	uint32_t v;
	uint32_t matrix;
	uint32_t full_range;
	Dav1dPicture pic;
} Picture;

EXPORT(sf_alloc) void *sf_alloc(uint32_t n) { return malloc(n); }

EXPORT(sf_free) void sf_free(void *p) { free(p); }

EXPORT(sf_open) Dav1dContext *sf_open(void) {
	Dav1dSettings s;
	dav1d_default_settings(&s);
	s.n_threads = 1;
	s.max_frame_delay = 1;
	s.logger.callback = NULL;
	Dav1dContext *c = NULL;
	return dav1d_open(&c, &s) < 0 ? NULL : c;
}

EXPORT(sf_close) void sf_close(Dav1dContext *c) { dav1d_close(&c); }

static void release_input(const uint8_t *buf, void *cookie) {
	(void)cookie;
	free((void *)buf);
}

static Picture *take(Dav1dContext *c, int *err) {
	Picture *p = calloc(1, sizeof(Picture));
	if (!p) {
		*err = -ENOMEM;
		return NULL;
	}
	int r = dav1d_get_picture(c, &p->pic);
	if (r < 0) {
		free(p);
		if (r != DAV1D_ERR(EAGAIN))
			*err = r;
		return NULL;
	}
	if (p->pic.p.layout != DAV1D_PIXEL_LAYOUT_I420 || p->pic.p.bpc != 8) {
		dav1d_picture_unref(&p->pic);
		free(p);
		*err = -ENOTSUP;
		return NULL;
	}
	p->width = p->pic.p.w;
	p->height = p->pic.p.h;
	p->stride_y = (uint32_t)p->pic.stride[0];
	p->stride_uv = (uint32_t)p->pic.stride[1];
	p->y = (uint32_t)(uintptr_t)p->pic.data[0];
	p->u = (uint32_t)(uintptr_t)p->pic.data[1];
	p->v = (uint32_t)(uintptr_t)p->pic.data[2];
	p->matrix = p->pic.seq_hdr->mtrx;
	p->full_range = p->pic.seq_hdr->color_range;
	return p;
}

/**
 * Decodes one temporal unit (`buf`, from sf_alloc, owned by the decoder from here on)
 * and returns its picture, or NULL with *err set (0: the unit showed no picture).
 */
EXPORT(sf_decode)
Picture *sf_decode(Dav1dContext *c, uint8_t *buf, uint32_t len, int *err) {
	*err = 0;
	Dav1dData data = {0};
	if (dav1d_data_wrap(&data, buf, len, release_input, NULL) < 0) {
		free(buf);
		*err = -EINVAL;
		return NULL;
	}
	Picture *out = NULL;
	int stalls = 0;
	while (data.sz) {
		int r = dav1d_send_data(c, &data);
		if (r < 0 && r != DAV1D_ERR(EAGAIN)) {
			*err = r;
			break;
		}
		Picture *p = take(c, err);
		if (*err)
			break;
		if (p) {
			if (out) {
				dav1d_picture_unref(&out->pic);
				free(out);
			}
			out = p;
		} else if (r == DAV1D_ERR(EAGAIN) && ++stalls > 4) {
			*err = r;
			break;
		}
	}
	if (data.sz)
		dav1d_data_unref(&data);
	if (*err && out) {
		dav1d_picture_unref(&out->pic);
		free(out);
		return NULL;
	}
	return out;
}

EXPORT(sf_release) void sf_release(Picture *p) {
	dav1d_picture_unref(&p->pic);
	free(p);
}
