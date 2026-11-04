// Every arm's settings. noVNC: stock 1.6.0 with kyora's viewer profiles, x11vnc with kyora's flags
// on a unix socket, websockify in place of kyora-vmd. -viewonly is dropped because every run types.
export const NOVNC_VERSION = '1.6.0';
export const NOVNC_PROFILES = { high: { quality: 7, compression: 2 }, low: { quality: 4, compression: 6 } };
export const X11VNC_ARGS = [
	'-nopw', '-q', '-norc', '-nosel', '-noclipboard', '-nolookup', '-nocmds', '-noremote', '-novncconnect', '-nobell',
	'-rfbport', '0', '-rfbportv6', '0', '-no6', '-shared', '-alwaysshared', '-dontdisconnect', '-forever',
	'-wait', '2', '-defer', '2', '-nonap',
];
export const STARFOLD_ARGS = ['--audio-enabled=false', '--enable-basic-auth=true', '--framerate=30', '--encoder=auto'];
export const WIDTH = 1920, HEIGHT = 1080, FPS = 30;
export const STARFOLD_PORT = 6930, NOVNC_PORT = 6932, CTL_PORT = 6940;
export const ARMS = {
	novnc: { kind: 'novnc', profile: 'high' },
	'novnc-low': { kind: 'novnc', profile: 'low', wanOnly: true },
	'starfold-video': { kind: 'starfold', query: '' },
	'starfold-canvas': { kind: 'starfold', query: '&present=canvas' },
};
export const WORKLOADS = ['typing', 'scroll', 'video'];
