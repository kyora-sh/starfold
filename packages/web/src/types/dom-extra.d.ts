// Browser APIs voxl uses that TypeScript's DOM library doesn't include yet.

// Keyboard Lock (Chromium): capture reserved shortcuts in fullscreen.
interface Keyboard {
	lock(keyCodes?: string[]): Promise<void>;
	unlock(): void;
}
interface Navigator {
	readonly keyboard?: Keyboard;
}

// Read by the bench tools (measure.mjs) and handy in the console.
interface Window {
	fps: number;
	voxlStats: unknown;
}
