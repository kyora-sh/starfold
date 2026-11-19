// Every arm's settings. noVNC: stock 1.6.0 with kyora's viewer profiles, x11vnc with kyora's flags
// on a unix socket, websockify in place of kyora-vmd. -viewonly is dropped because every run types.
export const NOVNC_VERSION = '1.6.0';
export const NOVNC_PROFILES = { high: { quality: 7, compression: 2 }, low: { quality: 4, compression: 6 } };
export const X11VNC_ARGS = [
	'-nopw', '-q', '-norc', '-nosel', '-noclipboard', '-nolookup', '-nocmds', '-noremote', '-novncconnect', '-nobell',
	'-rfbport', '0', '-rfbportv6', '0', '-no6', '-shared', '-alwaysshared', '-dontdisconnect', '-forever',
	'-wait', '2', '-defer', '2', '-nonap',
];
// kyora's dashboard live view: kyora-platform's packages/cloud/worker/public/vms-viewer.js, vendored verbatim in kyora/
// (raw and copy-rect, 32-bit pixels, no scaling), with the x11vnc flags kyora_vm_agent.py gives each mode. View only
// is the shared x11vnc (X11VNC_SHARED, X11VNC_TIMING, X11VNC_COMMON); take control on the sandbox's own desktop is
// an x11vnc -inetd of its own with x11vnc's default timing. Here it listens on the unix socket instead of -inetd,
// and -forever outlives the view connection the page opens before taking control.
export const KYORA_VIEWER = { repo: 'kyora-sh/kyora-platform', commit: 'd47e2fade2bc281018ee7dbbd503cd49a6e99177', path: 'packages/cloud/worker/public/vms-viewer.js', sha256: 'e6ab710a0c854de6073f2d100d01cf264434e3f4b9c99aef82a0390e522ab3a6' };
export const KYORA_X11VNC = {
	view: [
		'-rfbport', '0', '-rfbportv6', '0', '-no6', '-viewonly', '-shared', '-alwaysshared', '-dontdisconnect', '-forever',
		'-wait', '2', '-defer', '2', '-nonap',
		'-nopw', '-q', '-norc', '-nosel', '-noclipboard', '-nolookup', '-nocmds', '-noremote', '-novncconnect', '-nobell',
	],
	control: ['-nopw', '-q', '-norc', '-shared', '-nosel', '-nolookup', '-rfbport', '0', '-rfbportv6', '0', '-no6', '-forever'],
};
export const STARFOLD_ARGS = ['--audio-enabled=false', '--enable-basic-auth=true', '--framerate=30', '--encoder=auto'];
export const WIDTH = 1920, HEIGHT = 1080, FPS = 30;
export const STARFOLD_PORT = 6930, NOVNC_PORT = 6932, CTL_PORT = 6940;
// The shaped link: bench/shaper.py in front of each port at +1000, the queue bounded at 2x BDP.
export const WAN = { rttMs: Number(process.env.WAN_RTT || 50), mbps: Number(process.env.WAN_MBPS || 20), queueBytes: Number(process.env.WAN_QUEUE || 250000), portOffset: 1000 };
// `codec` is what the viewer must be served (/stats clients[].stream.codec): omitted = h264, null = this
// server doesn't report it. Being served anything else fails the run.
export const ARMS = {
	novnc: { kind: 'novnc', profile: 'high' },
	'novnc-low': { kind: 'novnc', profile: 'low', wanOnly: true },
	'starfold-video': { kind: 'starfold', query: '' },
	'starfold-canvas': { kind: 'starfold', query: '&present=canvas' },
	// Only when named in ARMS: which encoder path refines a still picture. `bin` names the env var holding the binary.
	before: { kind: 'starfold', query: '', probe: true, bin: 'STARFOLD_BEFORE_BIN', args: STARFOLD_ARGS },
	'floor-16': { kind: 'starfold', query: '', probe: true, args: [...STARFOLD_ARGS, '--crf=16'] },
	'floor-12': { kind: 'starfold', query: '', probe: true, args: [...STARFOLD_ARGS, '--crf=12'] },
	'starfold-x264': { kind: 'starfold', query: '', probe: true, bin: 'STARFOLD_X264_BIN', args: [...STARFOLD_ARGS.slice(0, 3), '--encoder=x264'] },
	'x264-lossless-paintover': { kind: 'starfold', query: '', probe: true, bin: 'STARFOLD_X264_BIN', args: [...STARFOLD_ARGS.slice(0, 3), '--encoder=x264', '--paintover-crf=0', '--max-kbps=0'] },
	// kyora's live view, only when named: view only takes no keys, so it runs quality rounds alone.
	'kyora-rfb': { kind: 'rfb', probe: true, viewOnly: true, x11vnc: KYORA_X11VNC.view },
	'kyora-rfb-control': { kind: 'rfb', probe: true, control: true, x11vnc: KYORA_X11VNC.control },
	'voxl-upstream': { kind: 'starfold', query: '', probe: true, bin: 'VOXL_BIN', args: STARFOLD_ARGS.slice(0, 3), codec: null },
};
export const WORKLOADS = ['typing', 'scroll', 'video'];
/** Every key the source window receives, one `<ms> <letter>` line each; truncated at each start. */
export const KEYS_LOG = '/tmp/vs-keys.log';
