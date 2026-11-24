"""start_server() against the real server on Xvfb.

Needs Linux with Xvfb and STARFOLD_BIN pointing at a built `starfold` binary:

    STARFOLD_BIN=target/release/starfold uv run pytest -m integration
"""

import contextlib
import os
import shutil
import signal
import subprocess
import sys
import textwrap
import threading
import time
from pathlib import Path

import httpx
import pytest
from _xvfb import read_display

import starfold

pytestmark = [
    pytest.mark.integration,
    pytest.mark.skipif(
        not os.environ.get("STARFOLD_BIN") or not shutil.which("Xvfb") or not sys.platform.startswith("linux"),
        reason="set STARFOLD_BIN and install Xvfb (Linux) to run",
    ),
]

ARGS = ["--audio-enabled=false"]


@pytest.fixture
def display(monkeypatch):
    r, w = os.pipe()
    xvfb = subprocess.Popen(
        ["Xvfb", "-displayfd", str(w), "-screen", "0", "1024x768x24", "-nolisten", "tcp"],
        pass_fds=(w,),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    os.close(w)
    d = read_display(r)
    monkeypatch.setenv("DISPLAY", d)
    yield d
    xvfb.terminate()
    xvfb.wait(10)


def _listening(port: int) -> list[str]:
    """Local addresses of the TCP sockets listening on `port` (hex, as /proc/net/tcp{,6} has them)."""
    found = []
    for name in ("tcp", "tcp6"):
        for line in Path(f"/proc/net/{name}").read_text().splitlines()[1:]:
            local, _, st = line.split()[1], line.split()[2], line.split()[3]
            addr, p = local.split(":")
            if int(p, 16) == port and st == "0A":
                found.append(addr)
    return found


def test_loopback_by_default(display):
    with starfold.start_server(args=ARGS) as server:
        port = int(server.url.rsplit(":", 1)[1])
        assert server.url == f"http://127.0.0.1:{port}"
        assert _listening(port) == ["0100007F"]
        assert server.client().screenshot().size == (1024, 768)


def test_base_path_is_part_of_the_url(display):
    with starfold.start_server(args=[*ARGS, "--base-path=/desktop"]) as server:
        assert server.url.endswith("/desktop")
        assert server.client().screenshot().size == (1024, 768)


def test_generated_password_held_by_the_client(display, monkeypatch):
    monkeypatch.setenv("STARFOLD_PASSWORD", "inherited-not-used")
    with starfold.start_server(args=ARGS) as a, starfold.start_server(args=ARGS) as b:
        assert len(a.password) >= 32
        assert a.password != b.password
        assert a.password != "inherited-not-used"
        assert a.client().screenshot().png.startswith(b"\x89PNG")
        r = httpx.get(f"{a.url}/screenshot")
        assert r.status_code == 401
        with pytest.raises(starfold.AuthError):
            starfold.Starfold(a.url, password=b.password).screenshot()
        with pytest.raises(starfold.AuthError):
            starfold.Starfold(a.url, password="inherited-not-used").screenshot()


def _alive(pid: int) -> bool:
    try:
        state = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0]
    except FileNotFoundError:
        return False
    return state != "Z"


def _tree(pid: int) -> list[int]:
    """`pid` and its descendants (the server, and on Linux and macOS the guard above it)."""
    found = [pid]
    for p in found:
        with contextlib.suppress(FileNotFoundError):
            found += [int(c) for c in Path(f"/proc/{p}/task/{p}/children").read_text().split()]
    return found


def test_no_orphan_after_the_parent_is_killed(display):
    script = textwrap.dedent(
        f"""
        import sys, time, starfold
        s = starfold.start_server(args={ARGS!r})
        print(s.process.pid, s.url, flush=True)
        time.sleep(600)
        """
    )
    parent = subprocess.Popen([sys.executable, "-c", script], stdout=subprocess.PIPE, text=True)
    line = parent.stdout.readline().split()
    assert line, "the parent didn't start a server"
    pid, url = int(line[0]), line[1]
    assert httpx.get(f"{url}/healthz").status_code == 200
    pids = _tree(pid)
    assert all(_alive(p) for p in pids)
    parent.send_signal(signal.SIGKILL)
    parent.wait()
    deadline = time.monotonic() + 15
    while left := [p for p in pids if _alive(p)]:
        assert time.monotonic() < deadline, f"{left} of {pids} outlived their parent"
        time.sleep(0.1)
    with pytest.raises(httpx.TransportError):
        httpx.get(f"{url}/healthz", timeout=2)


def test_server_outlives_the_thread_that_started_it(display):
    started: list[starfold.LocalServer] = []
    t = threading.Thread(target=lambda: started.append(starfold.start_server(args=ARGS)))
    t.start()
    t.join()
    (server,) = started
    try:
        time.sleep(2)
        assert server.process.poll() is None, (
            f"the server stopped when the thread that started it ended (exit {server.process.returncode}):\n"
            f"{server.stderr_tail}"
        )
        assert server.client().screenshot().size == (1024, 768)
    finally:
        server.close()


def test_close_stops_the_server_and_its_guard(display):
    server = starfold.start_server(args=ARGS)
    pids = _tree(server.process.pid)
    server.close()
    assert not [p for p in pids if _alive(p)]
    with pytest.raises(httpx.TransportError):
        httpx.get(f"{server.url}/healthz", timeout=2)
