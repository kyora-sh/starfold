# @starfold/server

The starfold server for Node 20+, Bun and Deno: the `starfold` command, and `startServer()` to
start one from code with a client already signed in to it. The client is the
[starfold](https://www.npmjs.com/package/starfold) package, which this one depends on.

```sh
npm install @starfold/server && npx starfold --version   # or: npx @starfold/server --version
```

On Linux x64 and arm64 (glibc), macOS arm64 and Windows x64 the binary comes with it, from the
optional dependency `@starfold/server-<platform>` that matches the machine. Elsewhere set
`STARFOLD_BIN` to a starfold binary built for it.

```ts
import { startServer } from '@starfold/server';

await using server = await startServer(); // loopback, a free port, a generated password
const sf = server.client(); // a starfold Starfold, signed in already
await (await sf.screenshot()).save('desktop.png');
```

`startServer()` listens on 127.0.0.1 unless you pass `addr`; `port` (default: a free one,
tried again if another process takes it first), `password` (default, and when empty: a
generated one), `args` (more server flags, `starfold --help`; with `--base-path` the `url`
includes it), `env` (added to this process's environment), `binary` and `timeoutMs` (until it
answers, default 30000) are optional. It resolves to a `LocalServer` with `url`, `password`,
`process`, `client()`, `output()` (the last of what the server printed) and `close()`; the
server stops on `close()`, at the end of an `await using` block, when your process exits, and
on Linux and macOS also when your process is killed. If the server exits, doesn't answer in
time or finds its port taken, `startServer()` throws a `ServerStartError` whose `output` is
what the server printed. On Linux it needs a display (`DISPLAY`, e.g. from Xvfb); it doesn't
start one.

`binaryPath()` is the binary: `$STARFOLD_BIN` if set, else the platform package's; without
one it throws a `BinaryNotFoundError`. The `starfold` command runs it (`npx starfold --help`).
These errors, and `MissingLibrariesError` below, are the starfold package's `StarfoldError`s
too.

On Linux the server uses the system's libyuv, Opus and PulseAudio libraries and needs glibc
2.39 or newer (Ubuntu 24.04 or newer):

```sh
sudo apt install libyuv0 libopus0 libpulse0
```

Without them, or on an older glibc, `startServer()` (a `MissingLibrariesError`) and
`starfold` name what is missing, the libraries or the glibc version (`GLIBC_2.39`), with
that line.

## From starfold 0.3.2

Up to 0.3.2 the `starfold` package carried the server too. From 0.4.0:

| 0.3.2 | 0.4.0 |
| --- | --- |
| `import { startServer } from 'starfold'`, and likewise `LocalServer`, `StartServerOptions`, `binaryPath`, `checkLibraries`, `missingLibraries`, `platformKey`, `serverPackage`, `LINUX_LIBRARIES_HINT`, `BinaryNotFoundError`, `MissingLibrariesError`, `ServerStartError` | the same names from `'@starfold/server'` |
| `npx starfold` / `npm i -g starfold` | `npx @starfold/server` / `npm i -g @starfold/server` |
| `Starfold`, `act`, `StarfoldError` and the client's other errors, the types | unchanged, from `starfold` |

## Tests

```sh
bun run test                                                   # unit tests, no server
bun run build && STARFOLD_BIN=../../target/release/starfold node test/integration.mjs   # Linux, Xvfb
```

The integration test also runs under `bun` and `deno run -A`.
