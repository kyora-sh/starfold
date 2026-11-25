# starfold (TypeScript client)

A client for agents and scripts driving a starfold desktop: screenshots, waiting for the
screen, mouse and keyboard, recordings and files. Node 20+, Bun and Deno; no dependencies.
The server, its `starfold` command and `startServer()` are
[@starfold/server](https://www.npmjs.com/package/@starfold/server)'s.

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

## The server

Up to 0.3.2 this package also carried the server: the `starfold` command and `startServer()`.
From 0.4.0 they are in [@starfold/server](https://www.npmjs.com/package/@starfold/server), which depends on this package:

| 0.3.2 | 0.4.0 |
| --- | --- |
| `import { startServer } from 'starfold'`, and likewise `LocalServer`, `StartServerOptions`, `binaryPath`, `checkLibraries`, `missingLibraries`, `platformKey`, `serverPackage`, `LINUX_LIBRARIES_HINT`, `BinaryNotFoundError`, `MissingLibrariesError`, `ServerStartError` | the same names from `'@starfold/server'` |
| `npx starfold` / `npm i -g starfold` | `npx @starfold/server` / `npm i -g @starfold/server` |
| `Starfold`, `act`, `StarfoldError` and the client's other errors, the types | unchanged, from `starfold` |

## Tests

```sh
bun run test                                                   # unit tests, no server
bun run build && STARFOLD_BIN=../../target/release/starfold node test/integration.mjs   # Linux, Xvfb + xterm
```

The integration test also runs under `bun` and `deno run -A`.
