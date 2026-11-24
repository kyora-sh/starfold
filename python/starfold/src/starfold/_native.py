"""The starfold server binary the platform wheels carry, and starting it locally."""

from __future__ import annotations

import atexit
import collections
import os
import platform
import re
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
import weakref
from collections.abc import Mapping, Sequence
from pathlib import Path

import httpx

from ._client import AsyncStarfold, Starfold
from ._types import StarfoldError

SUPPORTED = ("Linux x86_64 (glibc 2.39+)", "Linux aarch64 (glibc 2.39+)", "macOS arm64", "Windows x86_64")
APT_LINE = "sudo apt install libyuv0 libopus0 libpulse0"


class BinaryNotFoundError(StarfoldError):
    """No starfold server binary for this platform: neither $STARFOLD_BIN nor a platform wheel's."""


class MissingLibrariesError(StarfoldError):
    """The server binary's shared libraries don't all resolve on this Linux system."""


def _platform() -> str:
    return f"{platform.system() or sys.platform} {platform.machine() or 'unknown'}"


def _bundled() -> Path:
    name = "starfold.exe" if sys.platform == "win32" else "starfold"
    return Path(__file__).parent / "_bin" / name


def binary_path() -> str:
    """The starfold server binary: $STARFOLD_BIN if set, else the one this platform's wheel carries.

    Raises BinaryNotFoundError when there is neither; the client still works against a server
    running elsewhere.
    """
    env = os.environ.get("STARFOLD_BIN")
    if env:
        if not Path(env).is_file():
            raise BinaryNotFoundError(f"STARFOLD_BIN is {env}, which isn't a file")
        return env
    bundled = _bundled()
    if bundled.is_file():
        return str(bundled)
    raise BinaryNotFoundError(
        f"no starfold server binary for this platform ({_platform()}): the starfold wheels carry one for "
        f"{', '.join(SUPPORTED)}, and this install has none. Set STARFOLD_BIN to a starfold binary to start "
        "one here; the client (Starfold(url)) still works against a server running elsewhere."
    )


def _ldd_missing(output: str) -> list[str]:
    missing = []
    for line in output.splitlines():
        line = line.strip()
        if "=> not found" in line:
            missing.append(line.split("=>")[0].strip())
        elif m := re.search(r"version `([^']+)' not found", line):
            missing.append(m.group(1))
    return list(dict.fromkeys(missing))


def missing_libraries_message(missing: Sequence[str]) -> str:
    return (
        f"the starfold server can't start: these shared libraries are missing: {', '.join(missing)}. "
        f"It needs glibc 2.39+ (Ubuntu 24.04 or newer) and:\n\n    {APT_LINE}\n"
    )


def check_libraries(path: str) -> None:
    """On Linux, raises MissingLibrariesError naming the shared libraries `path` needs and can't find."""
    if not sys.platform.startswith("linux"):
        return
    ldd = shutil.which("ldd")
    if ldd is None:
        return
    try:
        r = subprocess.run([ldd, path], capture_output=True, text=True, timeout=30)
    except (OSError, subprocess.TimeoutExpired):
        return
    missing = _ldd_missing(r.stdout + "\n" + r.stderr)
    if missing:
        raise MissingLibrariesError(missing_libraries_message(missing))


def main() -> None:
    """The `starfold` command: runs the server binary with the given arguments and its exit code."""
    try:
        path = binary_path()
        check_libraries(path)
    except StarfoldError as e:
        print(f"starfold: {e}", file=sys.stderr)
        sys.exit(1)
    if sys.platform == "win32":
        sys.exit(subprocess.call([path, *sys.argv[1:]]))
    os.execv(path, [path, *sys.argv[1:]])


_WATCHDOG = """
import os, signal, sys, time
pid = int(sys.argv[1])
while sys.stdin.buffer.read(65536):
    pass
try:
    os.kill(pid, signal.SIGTERM)
    for _ in range(100):
        time.sleep(0.1)
        os.kill(pid, 0)
    os.kill(pid, signal.SIGKILL)
except ProcessLookupError:
    pass
"""

_live: weakref.WeakSet[LocalServer] = weakref.WeakSet()
_jobs: list[int] = []


@atexit.register
def _close_all() -> None:
    for s in list(_live):
        s.close()


def _pdeathsig(parent: int):
    """A preexec_fn asking for SIGTERM when the parent dies, with prctl looked up before the fork."""
    import ctypes

    prctl = ctypes.CDLL(None, use_errno=True).prctl

    def preexec() -> None:
        prctl(1, signal.SIGTERM, 0, 0, 0)
        if os.getppid() != parent:
            os._exit(1)

    return preexec


def _free_port(addr: str) -> int:
    family = socket.AF_INET6 if ":" in addr else socket.AF_INET
    with socket.socket(family) as s:
        s.bind((addr, 0))
        return s.getsockname()[1]


def _url(addr: str, port: int) -> str:
    host = {"0.0.0.0": "127.0.0.1", "::": "::1"}.get(addr, addr)
    return f"http://[{host}]:{port}" if ":" in host else f"http://{host}:{port}"


class LocalServer:
    """A starfold server this process started: `url`, `password`, `process`, `client()`, `close()`.

    The server stops on `close()`, when the `with` block ends, at interpreter exit, and when this
    process dies (on Linux through PR_SET_PDEATHSIG, elsewhere through a watchdog).
    """

    def __init__(
        self,
        process: subprocess.Popen[bytes],
        url: str,
        password: str,
        output: _Output,
        watchdog: subprocess.Popen[bytes] | None,
    ) -> None:
        self.process = process
        self.url = url
        self.password = password
        self._output = output
        self._watchdog = watchdog
        self._client: Starfold | None = None
        _live.add(self)

    @property
    def stderr_tail(self) -> str:
        """The server's last lines of output."""
        return self._output.tail()

    def client(self, timeout: float = 30.0) -> Starfold:
        """A Starfold client signed in to this server, closed with it."""
        if self._client is None:
            self._client = Starfold(self.url, password=self.password, timeout=timeout)
        return self._client

    def async_client(self, timeout: float = 30.0) -> AsyncStarfold:
        """A new AsyncStarfold client signed in to this server; close it yourself (`async with`)."""
        return AsyncStarfold(self.url, password=self.password, timeout=timeout)

    def close(self, timeout: float = 10.0) -> None:
        """Stops the server: SIGTERM (on Windows, terminate), then SIGKILL after `timeout` seconds."""
        if self._client is not None:
            self._client.close()
            self._client = None
        p = self.process
        if p.poll() is None:
            p.terminate()
            try:
                p.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()
        if self._watchdog is not None:
            self._watchdog.stdin.close()  # type: ignore[union-attr]
            self._watchdog.wait()
            self._watchdog = None
        _live.discard(self)

    def __enter__(self) -> LocalServer:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


class _Output:
    """The server's output, read as it comes so the pipe never fills, its last lines kept."""

    def __init__(self, stream) -> None:
        self.lines: collections.deque[str] = collections.deque(maxlen=40)
        self._thread = threading.Thread(target=self._read, args=(stream,), daemon=True)
        self._thread.start()

    def _read(self, stream) -> None:
        for line in iter(stream.readline, b""):
            self.lines.append(line.decode(errors="replace"))
        stream.close()

    def tail(self, wait: float = 0.0) -> str:
        self._thread.join(wait)
        return "".join(self.lines)


def _spawn(
    path: str, args: list[str], env: dict[str, str], cwd: str | Path | None, watchdog: bool
) -> tuple[subprocess.Popen[bytes], _Output, subprocess.Popen[bytes] | None]:
    kw: dict = {}
    if sys.platform == "win32":
        kw["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    elif sys.platform.startswith("linux") and not watchdog:
        kw["preexec_fn"] = _pdeathsig(os.getpid())
    p = subprocess.Popen(
        [path, *args],
        env=env,
        cwd=cwd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        **kw,
    )
    out = _Output(p.stdout)
    dog = None
    if sys.platform == "win32":
        _kill_on_close(p)
    elif watchdog:
        dog = subprocess.Popen(
            [sys.executable, "-I", "-c", _WATCHDOG, str(p.pid)],
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
    return p, out, dog


def _kill_on_close(p: subprocess.Popen[bytes]) -> None:
    """Windows: puts the server in a job object that ends with this process."""
    import ctypes
    from ctypes import wintypes

    k32 = ctypes.WinDLL("kernel32", use_last_error=True)  # type: ignore[attr-defined]

    class Basic(ctypes.Structure):
        _fields_ = [
            ("PerProcessUserTimeLimit", ctypes.c_int64),
            ("PerJobUserTimeLimit", ctypes.c_int64),
            ("LimitFlags", wintypes.DWORD),
            ("MinimumWorkingSetSize", ctypes.c_size_t),
            ("MaximumWorkingSetSize", ctypes.c_size_t),
            ("ActiveProcessLimit", wintypes.DWORD),
            ("Affinity", ctypes.c_size_t),
            ("PriorityClass", wintypes.DWORD),
            ("SchedulingClass", wintypes.DWORD),
        ]

    class Io(ctypes.Structure):
        _fields_ = [(n, ctypes.c_uint64) for n in ("r", "w", "o", "rb", "wb", "ob")]

    class Extended(ctypes.Structure):
        _fields_ = [
            ("BasicLimitInformation", Basic),
            ("IoInfo", Io),
            ("ProcessMemoryLimit", ctypes.c_size_t),
            ("JobMemoryLimit", ctypes.c_size_t),
            ("PeakProcessMemoryUsed", ctypes.c_size_t),
            ("PeakJobMemoryUsed", ctypes.c_size_t),
        ]

    k32.CreateJobObjectW.restype = wintypes.HANDLE
    k32.OpenProcess.restype = wintypes.HANDLE
    job = k32.CreateJobObjectW(None, None)
    info = Extended()
    info.BasicLimitInformation.LimitFlags = 0x2000
    k32.SetInformationJobObject(job, 9, ctypes.byref(info), ctypes.sizeof(info))
    proc = k32.OpenProcess(0x0100 | 0x0001, False, p.pid)
    k32.AssignProcessToJobObject(job, proc)
    k32.CloseHandle(proc)
    _jobs.append(job)


def start_server(
    *,
    addr: str = "127.0.0.1",
    port: int | None = None,
    password: str | None = None,
    args: Sequence[str] = (),
    env: Mapping[str, str] | None = None,
    cwd: str | Path | None = None,
    timeout: float = 30.0,
    binary: str | None = None,
    _watchdog: bool | None = None,
) -> LocalServer:
    """Starts a starfold server on this machine and waits until it answers.

    It listens on `addr` (loopback unless you pass another address) and `port` (a free one by
    default), with `password` (generated by default; the client from `client()` holds it). `args`
    are further server flags (`starfold --help`), `env` replaces the environment it inherits.
    On Linux it needs a display (DISPLAY, e.g. from Xvfb); it doesn't start one. Raises
    StarfoldError with the server's last output if it exits or doesn't answer within `timeout`
    seconds.
    """
    path = binary or binary_path()
    check_libraries(path)
    password = password or secrets.token_urlsafe(24)
    child_env = dict(os.environ if env is None else env)
    child_env["STARFOLD_PASSWORD"] = password
    child_env.pop("SELKIES_BASIC_AUTH_PASSWORD", None)
    watchdog = _watchdog if _watchdog is not None else not sys.platform.startswith("linux")
    deadline = time.monotonic() + timeout
    for attempt in range(3):
        chosen = port if port is not None else _free_port(addr)
        flags = [f"--addr={addr}", f"--port={chosen}", *args]
        p, out, dog = _spawn(path, flags, child_env, cwd, watchdog)
        server = LocalServer(p, _url(addr, chosen), password, out, dog)
        try:
            _wait_ready(server, deadline)
            return server
        except _PortTaken:
            server.close()
            if port is not None or attempt == 2:
                raise StarfoldError(f"port {chosen} is taken:\n{server.stderr_tail}") from None
        except BaseException:
            server.close()
            raise
    raise AssertionError("unreachable")


class _PortTaken(Exception):
    pass


def _wait_ready(server: LocalServer, deadline: float) -> None:
    with httpx.Client(timeout=1.0) as http:
        while True:
            if server.process.poll() is not None:
                tail = server._output.tail(wait=2.0)
                if "Address already in use" in tail or "address in use" in tail.lower():
                    raise _PortTaken()
                raise StarfoldError(f"the starfold server exited with {server.process.returncode}:\n{tail}")
            try:
                if http.get(f"{server.url}/healthz").status_code == 200:
                    return
            except httpx.TransportError:
                pass
            if time.monotonic() > deadline:
                raise StarfoldError(f"the starfold server didn't answer at {server.url} in time:\n{server.stderr_tail}")
            time.sleep(0.1)
