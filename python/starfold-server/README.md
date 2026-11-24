# starfold-server (Python)

The starfold server for Python: its binary, the `starfold` command, and `start_server()`, which
starts a server on this machine and hands you a [`starfold`](../starfold/README.md) client
signed in to it. Installing it installs the client too, at the same version.

```sh
uv add starfold-server   # or: pip install starfold-server
starfold --version
```

There are wheels for Linux x86_64 and aarch64 (glibc 2.39+), macOS arm64 and Windows x86_64,
each with the default (MIT) build's binary of that platform's release archive. Elsewhere pip
finds none; install `starfold` for the client alone, which works against a server running
elsewhere.

From a checkout: `uv add --editable path/to/starfold/python/starfold-server` (with `STARFOLD_BIN`
set to a built binary, since a checkout carries none).

```python
from starfold_server import start_server

with start_server() as server:  # loopback, a free port, a generated password
    sf = server.client()  # a starfold.Starfold, signed in already
    sf.screenshot().save("desktop.png")
```

`start_server()` listens on 127.0.0.1 unless you pass `addr=`; `port=`, `password=` (an
empty one counts as none), `args=` (more server flags, `starfold --help`), `env=` (merged onto
this process's environment), `cwd=`, `binary=` (default `binary_path()`) and `timeout=`
(seconds until it answers, default 30) are optional. If the server exits, doesn't answer in
time or can't have its port, it raises `ServerStartError` with the server's last output in
`output`. It returns a `LocalServer` with `url`, `password`, `process`, `stderr_tail` (the
server's last lines of output), `client()`, `async_client()` and `close()`; the server stops on
`close()`, at the end of the `with` block, at exit, and when your process ends, however it
ends. On Linux and macOS `process` is a small `sh` guard whose child is the server. On Linux it
needs a display (`DISPLAY`, e.g. from Xvfb); it doesn't start one.

`binary_path()` is the binary: `$STARFOLD_BIN` if set, else the wheel's. The `starfold` command
runs it (`starfold --help`). `BinaryNotFoundError`, `MissingLibrariesError` and
`ServerStartError` are `starfold.StarfoldError`s.

On Linux the server uses the system's libyuv, Opus and PulseAudio libraries and needs glibc
2.39 or newer (Ubuntu 24.04 or newer):

```sh
sudo apt install libyuv0 libopus0 libpulse0
```

Without them, `start_server()` and `starfold` say which libraries are missing.

## From 0.3.2

Up to 0.3.2 all of this was in the `starfold` package: `pip install starfold` and
`starfold.start_server(...)` become `pip install starfold-server` and
`starfold_server.start_server(...)`. The client's imports (`Starfold`, `act`, the errors) stay
in `starfold`. Staying on `starfold==0.3.2` changes nothing: it still carries the server.

## Tests

```sh
uv run pytest                                      # unit tests, no server
STARFOLD_BIN=../../target/release/starfold uv run pytest -m integration   # Linux, Xvfb + xterm
```
