# starfold (TypeScript)

A client for agents and scripts driving a starfold desktop: screenshots, waiting for the
screen, mouse and keyboard, recordings and files. Node 20+, Bun and Deno; no dependencies.

```sh
npm install starfold   # or: bun add starfold, pnpm add starfold, deno add npm:starfold
```

```ts
import { Starfold, act } from 'starfold';

// password defaults to $STARFOLD_PASSWORD
const sf = new Starfold('http://localhost:6901', { password: '...' });

const shot = await sf.screenshot(); // shot.png (Uint8Array), shot.seq, shot.size
await shot.save('before.png');

await sf.click(400, 300);
const r = await sf.typeText('hello\n');
await sf.wait({ change: true, since: r.seq }); // resolves once the typing changed the screen
await sf.wait({ idleMs: 300 }); // and once it settled

await sf.key('ctrl+shift+t');
await sf.scroll(0, 3, 400, 300);
await sf.drag(100, 100, 400, 100);
await sf.actions([
	act.key('ctrl+l'),
	act.typeText('example.com'),
	act.key('Enter'),
]);

const clips = await sf.record(5); // MP4s: one, or more if the encoder changed mid-recording
await sf.filesUpload('data.csv', 'local/data.csv');
console.log((await sf.filesList()).entries.map((e) => e.name));
```

The calls are the Python SDK's in camelCase, each returning a Promise; `wait()` takes its
options as an object (`idleMs`, `timeoutMs`, `change`, `region`, `since`). `timeoutMs` in the
constructor (default 30000) applies per request; waits and recordings get their own length on
top. `filesUpload()` takes bytes, a Blob or a local file's path.

Errors: `AuthError` (wrong or missing password), `ReadOnlyError` (the server runs with
`--read-only`), `RecordingOffError` (without `--enable-recording`), `ForbiddenError`,
`NotFoundError`, `StarfoldTimeoutError`, `StarfoldConnectionError`, all `StarfoldError`, with
`message` and `status`. `wait()` reports a timeout in its result (`timedOut`) rather than
throwing.

Keys for `key()` are a single character or a name (Enter, Tab, Escape, BackSpace, Delete,
Home, End, PageUp, PageDown, Up, Down, Left, Right, F1-F24, ...), joined with `+` after
modifiers (ctrl, shift, alt, altgr, super). With shift, a letter or a US-layout digit or
punctuation key means the shifted key: `ctrl+shift+t`, `cmd+shift+4`.

## The server, from the same install

On Linux x64 and arm64 (glibc), macOS arm64 and Windows x64, `npm install starfold` also
installs the starfold server binary, from the optional dependency
`@starfold/server-<platform>` that matches the machine. On other platforms it installs the
client alone, which works against a server running elsewhere.

```ts
import { startServer } from 'starfold';

await using server = await startServer(); // loopback, a free port, a generated password
const sf = server.client(); // signed in already
await (await sf.screenshot()).save('desktop.png');
```

`startServer()` listens on 127.0.0.1 unless you pass `addr`; `port`, `password`, `args`
(more server flags, `starfold --help`), `env`, `binary` and `timeoutMs` (until it answers,
default 30000) are optional. It resolves to a `LocalServer` with `url`, `password`, `process`,
`client()`, `output()` (the last of what the server printed) and `close()`; the server stops on
`close()`, at the end of an `await using` block, when your process exits, and on Linux and
macOS also when your process is killed. If the server exits before it answers, the error
carries what it printed. On Linux it needs a display (`DISPLAY`, e.g. from Xvfb); it doesn't
start one.

`binaryPath()` is the binary: `$STARFOLD_BIN` if set, else the platform package's. The
`starfold` command runs it (`npx starfold --help`).

On Linux the server uses the system's libyuv, Opus and PulseAudio libraries and needs glibc
2.39 or newer (Ubuntu 24.04 or newer):

```sh
sudo apt install libyuv0 libopus0 libpulse0
```

Without them, `startServer()` and `starfold` say which libraries are missing.

## Tests

```sh
bun run test                                                   # unit tests, no server
bun run build && STARFOLD_BIN=../../target/release/starfold node test/integration.mjs   # Linux, Xvfb + xterm
```

The integration test also runs under `bun` and `deno run -A`.
