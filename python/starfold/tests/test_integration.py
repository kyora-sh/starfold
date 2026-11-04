"""The SDK against a real server on Xvfb, typing into an xterm.

Needs Linux with Xvfb and xterm, and STARFOLD_BIN pointing at a built `starfold` binary:

    STARFOLD_BIN=target/release/starfold uv run pytest -m integration
"""

import contextlib
import os
import secrets
import select
import shutil
import signal
import socket
import subprocess
import time
from pathlib import Path

import httpx
import pytest

from starfold import Starfold

pytestmark = [
    pytest.mark.integration,
    pytest.mark.skipif(
        not os.environ.get("STARFOLD_BIN") or not shutil.which("Xvfb") or not shutil.which("xterm"),
        reason="set STARFOLD_BIN and install Xvfb and xterm to run",
    ),
]


def _ours(marker: str) -> list[int]:
    """Processes started by this test, found by the marker in their environment."""
    found = []
    for d in Path("/proc").iterdir():
        if not d.name.isdigit():
            continue
        try:
            env = (d / "environ").read_bytes()
        except OSError:
            continue
        if f"STARFOLD_TEST_RUN={marker}".encode() in env.split(b"\0"):
            found.append(int(d.name))
    return found


def _stop(proc: subprocess.Popen) -> None:
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(proc.pid, sig)
        except ProcessLookupError:
            return
        try:
            proc.wait(timeout=5)
            return
        except subprocess.TimeoutExpired:
            continue


@pytest.fixture
def desktop(tmp_path):
    marker = secrets.token_hex(8)
    env = {**os.environ, "STARFOLD_TEST_RUN": marker}
    procs: list[subprocess.Popen] = []

    def start(args, **kw):
        log = open(tmp_path / f"{Path(args[0]).name}.log", "wb")  # noqa: SIM115
        p = subprocess.Popen(args, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True, **kw)
        log.close()
        procs.append(p)
        return p

    try:
        r, w = os.pipe()
        start(["Xvfb", "-displayfd", str(w), "-screen", "0", "1280x800x24", "-nolisten", "tcp"], pass_fds=(w,))
        os.close(w)
        ready, _, _ = select.select([r], [], [], 15)
        assert ready, "Xvfb did not start"
        display = ":" + os.read(r, 32).decode().strip()
        os.close(r)
        env["DISPLAY"] = display

        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            port = s.getsockname()[1]
        password = secrets.token_hex(16)
        env["VOXL_PASSWORD"] = password
        files = tmp_path / "files"
        files.mkdir()
        server = start(
            [
                os.environ["STARFOLD_BIN"],
                f"--port={port}",
                "--addr=127.0.0.1",
                "--audio-enabled=false",
                f"--files-root={files}",
            ]
        )
        sf = Starfold(f"http://127.0.0.1:{port}", password=password, timeout=15)
        deadline = time.monotonic() + 20
        while True:
            assert server.poll() is None, (tmp_path / "starfold.log").read_text()
            try:
                if httpx.get(f"http://127.0.0.1:{port}/healthz", timeout=1).status_code == 200:
                    break
            except Exception:
                pass
            assert time.monotonic() < deadline, "server did not come up"
            time.sleep(0.2)

        before = sf.wait(idle_ms=300).seq
        start(["xterm", "-geometry", "80x24+0+0", "-e", "sh"], cwd=tmp_path)
        assert sf.wait(change=True, since=before, timeout_ms=15_000).changed, "xterm never drew"
        sf.wait(idle_ms=300)
        yield sf, tmp_path, files
        sf.close()
    finally:
        for p in reversed(procs):
            _stop(p)
        left = _ours(marker)
        for _ in range(50):
            stragglers = _ours(marker)
            if not stragglers:
                break
            for pid in stragglers:
                with contextlib.suppress(ProcessLookupError):
                    os.kill(pid, signal.SIGKILL)
            time.sleep(0.1)
        assert not _ours(marker), f"processes survive SIGKILL: {_ours(marker)}"
        assert not left, f"processes left behind: {left}"


def test_type_into_xterm_and_see_it(desktop):
    sf, cwd, files = desktop
    shot = sf.screenshot()
    assert shot.size == (1280, 800)

    sf.click(100, 100)
    r = sf.type_text("printf 'Hello, World!' > typed.txt")
    sf.key("Return")
    w = sf.wait(change=True, since=r.seq, timeout_ms=5000)
    assert w.changed
    assert not w.timed_out
    assert w.seq > r.seq

    typed = cwd / "typed.txt"
    deadline = time.monotonic() + 10
    while not (typed.exists() and typed.read_text() == "Hello, World!"):
        assert time.monotonic() < deadline, typed.read_text() if typed.exists() else "nothing typed"
        time.sleep(0.1)

    assert sf.wait(idle_ms=300, timeout_ms=5000).timed_out is False
    after = sf.screenshot()
    assert after.seq > shot.seq
    assert after.png != shot.png


def test_files_and_recording(desktop):
    sf, _, files = desktop
    up = sf.files_upload("note.txt", b"from the sdk", path="")
    assert up.size == 12
    assert (files / "note.txt").read_bytes() == b"from the sdk"
    assert "note.txt" in [e.name for e in sf.files_list("").entries]
    assert sf.files_download("note.txt") == b"from the sdk"

    rec = sf.record_start()
    sf.type_text("echo recording\n")
    sf.wait(idle_ms=300)
    clip = sf.record_stop(rec)
    assert clip[4:8] == b"ftyp"
