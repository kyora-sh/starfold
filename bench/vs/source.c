// Bench source at a fixed rate: typing (a glyph per key), scroll (a text-like page), video (a
// panning, grainy picture). Top left, a marker strip in 16 px cells the harness reads back:
// rows 0-1 are 8 check bits + a 12-bit frame number + 20 bits of wall-clock ms; row 2 is the
// last key, 5 bits (0-25, 31 = none) then the same 5 inverted, so a frame carrying a tag was
// provably caused by that key. SIGUSR1 freezes the picture and prints FROZEN, then writes what scroll
// and video draw to the file named by argv[5] (the harness rebuilds any frame from it) and prints
// DUMPED. argv[6], if given, is the page as raw BGRX, W wide and H + 6000 tall: real rendered text
// for the scroll and as the typing backdrop.
#define _GNU_SOURCE
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/extensions/XShm.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/shm.h>
#include <sys/time.h>
#include <time.h>
#include <math.h>
#include <poll.h>
#include <signal.h>
#include <unistd.h>
#define CELL 16
#define STRIP_W (20 * CELL)
#define STRIP_H (3 * CELL)
#define LOOP 30
static int W, H;
static Display *d;
static Window w;
static GC g;
static unsigned tag = 31;
static void cell(int col, int row, int on) {
	XSetForeground(d, g, on ? 0xffffff : 0x000000);
	XFillRectangle(d, w, g, col * CELL, row * CELL, CELL, CELL);
}
static void draw_tag(void) {
	for (int b = 0; b < 5; b++) {
		cell(b, 2, tag >> b & 1);
		cell(b + 5, 2, !(tag >> b & 1));
	}
}
static void draw_frame(unsigned n) {
	struct timeval tv;
	gettimeofday(&tv, NULL);
	uint32_t v = (n & 0xfff) << 20 | (uint32_t)((tv.tv_sec * 1000LL + tv.tv_usec / 1000) & 0xfffff);
	uint32_t c = (v ^ v >> 8 ^ v >> 16 ^ v >> 24) & 0xff;
	for (int b = 0; b < 40; b++) {
		int bit = b < 32 ? v >> b & 1 : c >> (b - 32) & 1;
		cell(b % 20, b / 20, bit);
	}
}
static void page(Pixmap p, int ph) {
	XSetForeground(d, g, 0xffffff);
	XFillRectangle(d, p, g, 0, 0, W, ph);
	for (int y = 20; y < ph - 24; y += 24) {
		if (rand() % 23 == 0) {
			XSetForeground(d, g, rand() & 0xffffff);
			XFillRectangle(d, p, g, 80, y, 300 + rand() % 900, 180);
			for (int k = 0; k < 400; k++) {
				XSetForeground(d, g, rand() & 0xffffff);
				XFillRectangle(d, p, g, 80 + rand() % 1100, y + rand() % 170, 6, 6);
			}
			y += 190;
			continue;
		}
		int x = 80, end = 800 + rand() % (W - 900);
		XSetForeground(d, g, rand() % 9 ? 0x202020 : 0x1a5fb4);
		while (x < end) {
			int n = 14 + rand() % 70;
			for (int c = 0; c < n; c += 7) XFillRectangle(d, p, g, x + c, y + rand() % 4, 5, 9 + rand() % 5);
			x += n + 8;
		}
	}
}
static uint32_t rng = 2463534242u;
static uint32_t xs(void) {
	rng ^= rng << 13;
	rng ^= rng >> 17;
	rng ^= rng << 5;
	return rng;
}
static void picture(uint32_t *px) {
	float *a = malloc(sizeof(float) * W), *b = malloc(sizeof(float) * H);
	for (int f = 0; f < LOOP; f++) {
		int pan = f * W / LOOP;
		float ph = 2 * M_PI * f / LOOP;
		for (int x = 0; x < W; x++) a[x] = sin(2 * M_PI * 3 * (x + pan) / W) * 50 + sin(2 * M_PI * 11 * (x + pan) / W) * 20;
		for (int y = 0; y < H; y++) b[y] = sin(y / 61.0 + ph) * 40 + sin(y / 17.0 - 2 * ph) * 12;
		for (int y = 0; y < H; y++) {
			uint32_t *row = px + (size_t)(f * H + y) * W;
			for (int x = 0; x < W; x++) {
				uint32_t n = xs();
				int base = 128 + a[x] + b[y];
				int r = base + (n & 15) - 8, gr = base / 2 + 40 + (n >> 8 & 15) - 8, bl = 255 - base + (n >> 16 & 15) - 8;
				r = r < 0 ? 0 : r > 255 ? 255 : r;
				gr = gr < 0 ? 0 : gr > 255 ? 255 : gr;
				bl = bl < 0 ? 0 : bl > 255 ? 255 : bl;
				row[x] = 0xff000000u | r << 16 | gr << 8 | bl;
			}
		}
		for (int k = 0; k < 8; k++) {
			double t = ph + k * 0.8;
			int cx = W / 2 + cos(t * (1 + k % 2)) * W * 0.38, cy = H / 2 + sin(t) * H * 0.36, rad = 60 + k * 14;
			for (int y = cy - rad; y < cy + rad; y++)
				for (int x = cx - rad; x < cx + rad; x++)
					if (x >= 0 && y >= 0 && x < W && y < H && (x - cx) * (x - cx) + (y - cy) * (y - cy) < rad * rad)
						px[(size_t)(f * H + y) * W + x] = 0xff000000u | (k * 31) << 16 | (255 - k * 25) << 8 | (xs() & 31) + 40;
		}
	}
	free(a);
	free(b);
}
static volatile sig_atomic_t freeze;
static void on_freeze(int s) {
	(void)s;
	freeze = 1;
}
static int cx = 80, cy = 74;
static XFontStruct *font;
static void key(XEvent *e) {
	KeySym s = XLookupKeysym(&e->xkey, 0);
	if (s < 'a' || s > 'z') return;
	tag = s - 'a';
	draw_tag();
	XSetBackground(d, g, 0xffffff);
	XSetForeground(d, g, 0x202020);
	char c = (char)s;
	if (font) XDrawImageString(d, w, g, cx, cy + 16, &c, 1);
	else XFillRectangle(d, w, g, cx, cy, 8, 16);
	cx += 10;
	if (cx > W - 100) {
		cx = 80;
		cy += 22;
		if (cy > H - 60) cy = 74;
		XSetForeground(d, g, 0xffffff);
		XFillRectangle(d, w, g, 0, cy, W, 22);
	}
	XFlush(d);
}
static int load_page(Pixmap p, int ph, const char *path) {
	FILE *f = fopen(path, "rb");
	if (!f) return 0;
	size_t size = (size_t)W * ph * 4;
	char *px = malloc(size);
	int ok = fread(px, 1, size, f) == size;
	fclose(f);
	if (ok) {
		XImage *im = XCreateImage(d, DefaultVisual(d, DefaultScreen(d)), 24, ZPixmap, 0, px, W, ph, 32, W * 4);
		XPutImage(d, p, g, im, 0, 0, 0, 0, W, ph);
		XSync(d, False);
		XDestroyImage(im);
	} else free(px);
	return ok;
}
static void events(void) {
	while (XPending(d)) {
		XEvent e;
		XNextEvent(d, &e);
		if (e.type == KeyPress) key(&e);
	}
}
int main(int argc, char **argv) {
	if (argc < 5) return 2;
	const char *mode = argv[1];
	W = atoi(argv[2]);
	H = atoi(argv[3]);
	long ns = 1000000000L / atoi(argv[4]);
	d = XOpenDisplay(NULL);
	if (!d) return 1;
	srand(1);
	XSetWindowAttributes a = {.override_redirect = True, .background_pixel = 0xffffff};
	w = XCreateWindow(d, DefaultRootWindow(d), 0, 0, W, H, 0, CopyFromParent, InputOutput, CopyFromParent, CWOverrideRedirect | CWBackPixel, &a);
	XSelectInput(d, w, KeyPressMask);
	XMapRaised(d, w);
	XSync(d, False);
	XSetInputFocus(d, w, RevertToParent, CurrentTime);
	g = XCreateGC(d, w, 0, NULL);
	XClearWindow(d, w);
	int paged = !strcmp(mode, "paged"), mixed = !strcmp(mode, "mixed"), typing = !strcmp(mode, "typing"), scroll = !strcmp(mode, "scroll"), video = !strcmp(mode, "video");
	int ph = H + 6000;
	Pixmap p = 0;
	XImage *img = NULL;
	if (scroll || typing || mixed || paged) {
		p = XCreatePixmap(d, w, W, ph, DefaultDepth(d, DefaultScreen(d)));
		if (argc <= 6 || !load_page(p, ph, argv[6])) page(p, ph);
		XCopyArea(d, p, w, g, 0, 0, W, H, 0, 0);
		font = XLoadQueryFont(d, "9x15");
		if (font) XSetFont(d, g, font->fid);
		if (typing) {
			XSetForeground(d, g, 0xffffff);
			XFillRectangle(d, w, g, 0, 72, W, 24);
		}
	}
	if (video || mixed) {
		XShmSegmentInfo *si = calloc(1, sizeof *si);
		img = XShmCreateImage(d, DefaultVisual(d, DefaultScreen(d)), 24, ZPixmap, NULL, si, W, H * LOOP);
		si->shmid = shmget(IPC_PRIVATE, (size_t)img->bytes_per_line * H * LOOP, IPC_CREAT | 0600);
		si->shmaddr = img->data = shmat(si->shmid, 0, 0);
		XShmAttach(d, si);
		XSync(d, False);
		shmctl(si->shmid, IPC_RMID, 0);
		picture((uint32_t *)img->data);
	}
	draw_frame(0);
	draw_tag();
	XSync(d, False);
	struct sigaction sa = {.sa_handler = on_freeze};
	sigaction(SIGUSR1, &sa, NULL);
	puts("SOURCE READY");
	fflush(stdout);
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	struct pollfd pf = {.fd = ConnectionNumber(d), .events = POLLIN};
	for (unsigned n = 1, off = 0;; n++) {
		if (freeze) {
			XSync(d, False);
			puts("FROZEN");
			fflush(stdout);
			FILE *dump = argc > 5 && !typing ? fopen(argv[5], "wb") : NULL;
			if (dump && video) fwrite(img->data, img->bytes_per_line, (size_t)H * LOOP, dump);
			if (dump && scroll) {
				XImage *all = XGetImage(d, p, 0, 0, W, ph, AllPlanes, ZPixmap);
				fwrite(all->data, all->bytes_per_line, ph, dump);
			}
			if (dump) fclose(dump);
			puts("DUMPED");
			fflush(stdout);
			for (;;) pause();
		}
		if (typing) {
			events();
			poll(&pf, 1, 100);
			continue;
		}
		/* mixed: 200 ms of scrolling, a still moment, 200 ms of video, a still moment */
		int phase = paged ? n % 150 : n % 40;
		if (paged && phase >= 6) {
			/* page down, then read for 5 s */
		} else if (mixed && (phase >= 26 || (phase >= 6 && phase < 20))) {
			/* still */
		} else if (scroll || paged || (mixed && phase < 6)) {
			off = (off + 20) % (ph - H);
			XCopyArea(d, p, w, g, 0, off + STRIP_H, W, H - STRIP_H, 0, STRIP_H);
			XCopyArea(d, p, w, g, STRIP_W, off, W - STRIP_W, STRIP_H, STRIP_W, 0);
		} else
		{
			int top = (n % LOOP) * H;
			XShmPutImage(d, w, g, img, 0, top + STRIP_H, 0, STRIP_H, W, H - STRIP_H, False);
			XShmPutImage(d, w, g, img, STRIP_W, top, STRIP_W, 0, W - STRIP_W, STRIP_H, False);
		}
		if (paged ? phase < 6 : !mixed || phase < 6 || (phase >= 20 && phase < 26)) {
			draw_frame(n);
			draw_tag();
		}
		XSync(d, False);
		t.tv_nsec += ns;
		while (t.tv_nsec >= 1000000000L) {
			t.tv_nsec -= 1000000000L;
			t.tv_sec++;
		}
		for (;;) {
			events();
			struct timespec now, left;
			clock_gettime(CLOCK_MONOTONIC, &now);
			left.tv_sec = t.tv_sec - now.tv_sec;
			left.tv_nsec = t.tv_nsec - now.tv_nsec;
			if (left.tv_nsec < 0) {
				left.tv_nsec += 1000000000L;
				left.tv_sec--;
			}
			if (left.tv_sec < 0 || freeze) break;
			if (ppoll(&pf, 1, &left, NULL) == 0) break;
		}
	}
}
