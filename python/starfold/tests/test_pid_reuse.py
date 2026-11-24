"""close() after the server has exited on its own must not signal whatever process now has its pid.

The test takes the macOS code path (sys.platform is set to darwin) and hands the server's pid out
again on purpose, as root through /proc/sys/kernel/ns_last_pid, so it needs Linux and
passwordless sudo:

    STARFOLD_TEST_PID_REUSE=1 uv run pytest tests/test_pid_reuse.py
"""

import os
import subprocess
import sys
import time
from pathlib import Path

import pytest

import starfold
from starfold import _native

NS_LAST_PID = Path("/proc/sys/kernel/ns_last_pid")

pytestmark = pytest.mark.skipif(
    os.environ.get("STARFOLD_TEST_PID_REUSE") != "1" or not NS_LAST_PID.exists(),
    reason="set STARFOLD_TEST_PID_REUSE=1 on Linux with passwordless sudo to run",
)

FAKE_SERVER = """#!{python}
import http.server, os, pathlib, sys, threading
here = pathlib.Path(sys.argv[0]).parent
(here / "pid").write_text(str(os.getpid()))
port = int(next(a for a in sys.argv if a.startswith("--port=")).split("=")[1])

class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, *a):
        pass

s = http.server.ThreadingHTTPServer(("127.0.0.1", port), H)
threading.Thread(target=s.serve_forever, daemon=True).start()
while not (here / "stop").exists():
    threading.Event().wait(0.05)
"""


def _alive(pid: int) -> bool:
    try:
        return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0] != "Z"
    except FileNotFoundError:
        return False


def _victim_with_pid(pid: int) -> subprocess.Popen[str]:
    """A `sleep` of this user whose pid is `pid` (the kernel hands out ns_last_pid + 1 next)."""
    uid, gid = os.getuid(), os.getgid()
    for _ in range(50):
        script = (
            f"echo {pid - 1} > {NS_LAST_PID}; "
            f"setpriv --reuid={uid} --regid={gid} --clear-groups sleep 60 & echo $!; wait"
        )
        p = subprocess.Popen(["sudo", "-n", "sh", "-c", script], stdout=subprocess.PIPE, text=True)
        got = int(p.stdout.readline())
        if got == pid:
            deadline = time.monotonic() + 10
            while f"Uid:\t{uid}\t" not in Path(f"/proc/{pid}/status").read_text():
                assert time.monotonic() < deadline, f"pid {pid} never became a process of uid {uid}"
                time.sleep(0.01)
            return p
        subprocess.run(["kill", str(got)], check=False)
        p.wait()
    pytest.skip(f"couldn't get pid {pid} handed out again")


def _started_then_exited(tmp_path: Path, monkeypatch) -> tuple[starfold.LocalServer, int]:
    """A server started on the macOS code path that has since exited on its own, and its pid."""
    fake = tmp_path / "starfold"
    fake.write_text(FAKE_SERVER.format(python=sys.executable))
    fake.chmod(0o755)
    monkeypatch.setattr(_native.sys, "platform", "darwin")
    server = starfold.start_server(binary=str(fake), timeout=20)
    pid = int((tmp_path / "pid").read_text())
    (tmp_path / "stop").touch()
    deadline = time.monotonic() + 10
    while _alive(pid) or server.process.poll() is None:
        assert time.monotonic() < deadline, "the fake server didn't exit"
        time.sleep(0.05)
    return server, pid


def test_close_after_the_server_exited_spares_a_process_that_reuses_its_pid(tmp_path, monkeypatch):
    if subprocess.run(["sudo", "-n", "true"], capture_output=True).returncode != 0:
        pytest.skip("needs passwordless sudo")
    server, pid = _started_then_exited(tmp_path, monkeypatch)

    victim = _victim_with_pid(pid)
    try:
        assert _alive(pid), f"the process given pid {pid} didn't start"
        print(f"server pid {pid} exited (process {server.process.pid}); pid {pid} is now a sleep of this user")
        server.close()
        time.sleep(1)
        print(f"after close(): pid {pid} alive: {_alive(pid)}")
        assert _alive(pid), f"close() killed pid {pid}, which another process had after the server exited"
    finally:
        subprocess.run(["kill", str(pid)], check=False)
        victim.wait()
