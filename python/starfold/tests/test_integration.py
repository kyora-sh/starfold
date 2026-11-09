"""The SDK against a real server on Xvfb, typing into an xterm.

Needs Linux with Xvfb and xterm, and STARFOLD_BIN pointing at a built `starfold` binary:

    STARFOLD_BIN=target/release/starfold uv run pytest -m integration

The encoder fallback test also needs ffprobe and a binary built with `--features x264`
(STARFOLD_X264=1): x264 is the only other H.264 encoder a machine without a hardware one has.
"""

import contextlib
import json
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
def server_args():
    return []


@pytest.fixture
def desktop(tmp_path, server_args):
    marker = secrets.token_hex(8)
    env = {**os.environ, "STARFOLD_TEST_RUN": marker, "VOXL_TEST_HOOKS": "1"}
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
                *server_args,
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
    clips = sf.record_stop(rec)
    assert len(clips) == 1
    assert clips[0][4:8] == b"ftyp"
    assert clips.end_reason == "stopped"


def _probe(path: Path) -> dict:
    """Decodes every frame of the MP4; returns its video stream."""
    r = subprocess.run(
        [
            "ffprobe",
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,profile,width,height,nb_read_frames",
            "-of",
            "json",
            str(path),
        ],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert r.returncode == 0, r.stderr
    assert r.stderr == "", r.stderr
    return json.loads(r.stdout)["streams"][0]


def _avcc(mp4: bytes) -> bytes:
    """The MP4's SPS and PPS as its track stores them."""
    at = mp4.index(b"avcC")
    return mp4[at + 4 : at + int.from_bytes(mp4[at - 4 : at], "big") - 4]


def _activity(sf: Starfold, text: str) -> None:
    r = sf.type_text(text)
    assert sf.wait(change=True, since=r.seq, timeout_ms=5000).changed
    sf.wait(idle_ms=300)


@pytest.mark.skipif(
    not os.environ.get("STARFOLD_X264") or not shutil.which("ffprobe"),
    reason="set STARFOLD_X264 (a binary with x264) and install ffprobe to run",
)
@pytest.mark.parametrize("server_args", [["--encoder=x264"]])
def test_encoder_fallback_rolls_the_recording(desktop):
    sf, cwd, _ = desktop
    sf.click(100, 100)
    rec = sf.record_start()
    _activity(sf, "echo before\n")

    assert sf._http.post("/debug/fail-h264").status_code == 204
    deadline = time.monotonic() + 15
    while True:
        chosen = sf._http.get("/stats").json()["encoder"]["chosen"]
        if any(c["codec"] == "h264" and c["backend"] == "x264" for c in chosen):
            break
        assert time.monotonic() < deadline, f"no fallback: {chosen}"
        time.sleep(0.1)
    _activity(sf, "echo after\n")

    clips = sf.record_stop(rec)
    assert len(clips) == 2
    assert clips.end_reason == "stopped"
    streams = []
    for i, clip in enumerate(clips):
        path = cwd / f"segment-{i + 1}.mp4"
        path.write_bytes(clip)
        streams.append(_probe(path))
    for s in streams:
        assert s["codec_name"] == "h264"
        assert (s["width"], s["height"]) == (1280, 800)
        assert int(s["nb_read_frames"]) >= 1
    assert _avcc(clips[0]) != _avcc(clips[1])
