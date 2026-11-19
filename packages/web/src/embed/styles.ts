// The styles the client's elements get from the served page's index.html (#surface, #screen,
// .stripe, #kbd), applied inline by the embed so it renders in a page without that CSS. A test
// keeps them equal to index.html's rules.

type Style = Partial<Record<keyof CSSStyleDeclaration, string>>;

export const SURFACE: Style = {
	position: 'relative',
	outline: 'none',
	touchAction: 'none',
};

export const SCREEN: Style = {
	position: 'absolute',
	inset: '0',
	width: '100%',
	height: '100%',
	display: 'block',
	pointerEvents: 'none',
};

export const STRIPE: Style = {
	position: 'absolute',
	left: '0',
	width: '100%',
	display: 'block',
	pointerEvents: 'none',
	objectFit: 'fill',
};

export const KBD: Style = {
	position: 'fixed',
	left: '0',
	bottom: '0',
	width: '1px',
	height: '1px',
	opacity: '0.01',
	border: '0',
	padding: '0',
	resize: 'none',
	pointerEvents: 'none',
	fontSize: '16px',
};

export function applyStyle(e: HTMLElement, s: Style) {
	Object.assign(e.style, s);
}

/**
 * Where worker.js, dav1d.js, dav1d.wasm and audio.js are: `assetBase` (a directory, with or
 * without its trailing slash) resolved against the module's URL, or the module's directory.
 */
export function assetBaseUrl(
	assetBase: string | URL | undefined,
	moduleUrl: string
): URL {
	const u = new URL(String(assetBase ?? '.'), moduleUrl);
	if (!u.pathname.endsWith('/')) u.pathname += '/';
	return u;
}
