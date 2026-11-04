// Fitting the desktop to the window; the worker paints the canvas.
import type { App } from './app';

export function createVideo(app: App, onLayout: () => void) {
	function layout() {
		const { w: dw, h: dh } = app.desk;
		if (!dw) return;
		const vw = window.innerWidth,
			vh = window.innerHeight;
		const dpr = window.devicePixelRatio || 1;
		// A whole number of device pixels high, so the picture isn't resampled off-grid.
		const devH = Math.max(
			1,
			Math.floor((Math.min(vw / dw, vh / dh) || 1) * dh * dpr)
		);
		app.view.scale = devH / dpr / dh;
		app.surface.style.width = `${dw * app.view.scale}px`;
		app.surface.style.height = `${dh * app.view.scale}px`;
		onLayout();
	}

	return { layout };
}
