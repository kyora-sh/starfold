import os
import select
import time


def read_display(r: int) -> str:
    """The display Xvfb -displayfd names, read up to its newline before the pipe is closed: closing
    it while Xvfb is still writing makes Xvfb exit ("Cannot write display number to fd")."""
    got = b""
    deadline = time.monotonic() + 15
    while not got.endswith(b"\n"):
        ready, _, _ = select.select([r], [], [], max(0.0, deadline - time.monotonic()))
        assert ready, "Xvfb did not start"
        chunk = os.read(r, 32)
        assert chunk, f"Xvfb exited before naming its display ({got!r})"
        got += chunk
    os.close(r)
    return ":" + got.decode().strip()
