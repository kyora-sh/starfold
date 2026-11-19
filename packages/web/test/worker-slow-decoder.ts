// Loaded after worker-preload.ts for the test that needs the hello's decoder probe to finish
// late: every configuration check answers "unsupported" after SLOW_MS.
const SLOW_MS = 300;
(globalThis as unknown as { VideoDecoder: unknown }).VideoDecoder = {
	isConfigSupported: () =>
		new Promise((r) => setTimeout(() => r({ supported: false }), SLOW_MS)),
};
