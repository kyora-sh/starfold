import asyncio
import json
import struct

import httpx
import pytest

from starfold import (
    AsyncStarfold,
    AuthError,
    ForbiddenError,
    NotFoundError,
    ReadOnlyError,
    Recording,
    Starfold,
    StarfoldConnectionError,
    StarfoldError,
    StarfoldTimeoutError,
    act,
)

PNG = b"\x89PNG\r\n\x1a\n" + b"\x00\x00\x00\rIHDR" + struct.pack(">II", 1280, 800) + b"\x08\x02\x00\x00\x00"


class Server:
    """Answers like the starfold server and keeps the requests it saw."""

    def __init__(self, password: str = "pw") -> None:
        self.password = password
        self.seen: list[httpx.Request] = []
        self.read_only = False

    def __call__(self, req: httpx.Request) -> httpx.Response:
        self.seen.append(req)
        if req.headers.get("authorization") != f"Bearer {self.password}":
            return httpx.Response(401, text="voxl needs its password")
        path, q = req.url.path, req.url.params
        if path == "/screenshot":
            return httpx.Response(200, content=PNG, headers={"x-voxl-seq": "41"})
        if path == "/wait":
            return httpx.Response(200, json={"changed": "since" in q, "timed_out": False, "seq": 42, "waited_ms": 7})
        if path == "/input":
            if self.read_only:
                return httpx.Response(403, json={"error": "this server is read-only (--read-only): input is refused"})
            return httpx.Response(200, json={"seq": 40, "events": len(json.loads(req.content))})
        if path == "/record":
            return httpx.Response(200, content=b"mp4:" + q["seconds"].encode())
        if path == "/record/start":
            return httpx.Response(200, json={"id": 3, "max_seconds": 300})
        if path == "/record/stop":
            if q["id"] != "3":
                return httpx.Response(404, json={"error": "no such recording"})
            return httpx.Response(200, content=b"mp4")
        if path == "/files/list":
            entries = [{"name": "a.txt", "dir": False, "size": 3, "mtime": 1700000000000}]
            return httpx.Response(200, json={"path": q["path"], "entries": entries})
        if path == "/files/download":
            return httpx.Response(403, json={"error": "downloads are disabled"})
        if path == "/files/upload":
            assert req.headers["x-voxl-upload"] == "1"
            return httpx.Response(200, json={"path": f"Downloads/{q['name']}", "size": len(req.content)})
        return httpx.Response(404, json={"error": "not found"})

    def last_json(self):
        return json.loads(self.seen[-1].content)


@pytest.fixture
def server():
    return Server()


@pytest.fixture
def sf(server):
    with Starfold("http://desk:6901", password="pw", transport=httpx.MockTransport(server)) as c:
        yield c


def test_screenshot(sf):
    shot = sf.screenshot()
    assert shot.png == PNG
    assert shot.seq == 41
    assert shot.size == (1280, 800)


def test_screenshot_save(sf, tmp_path):
    assert sf.screenshot().save(tmp_path / "s.png").read_bytes() == PNG


def test_wait_params_and_result(sf, server):
    r = sf.wait(change=True, since=41, region=(0, 0, 800, 600), timeout_ms=2000)
    assert (r.changed, r.timed_out, r.seq, r.waited_ms) == (True, False, 42, 7)
    q = server.seen[-1].url.params
    assert dict(q) == {"change": "1", "since": "41", "region": "0,0,800,600", "timeout": "2000"}
    sf.wait(idle_ms=500)
    assert dict(server.seen[-1].url.params) == {"idle": "500"}


def test_wait_gets_its_timeout_on_top(sf, server):
    sf.wait(timeout_ms=60_000)
    assert server.seen[-1].extensions["timeout"]["read"] == 90.0


def test_input_methods_send_actions(sf, server):
    r = sf.click(10, 20)
    assert (r.seq, r.events) == (40, 1)
    assert server.last_json() == [{"type": "click", "button": "left", "count": 1, "x": 10, "y": 20}]
    sf.double_click(button="right")
    assert server.last_json() == [{"type": "click", "button": "right", "count": 2}]
    sf.move(1, 2)
    assert server.last_json() == [{"type": "move", "x": 1, "y": 2}]
    sf.scroll(dy=3)
    assert server.last_json() == [{"type": "scroll", "dx": 0, "dy": 3}]
    sf.key("ctrl+c")
    assert server.last_json() == [{"type": "key", "key": "ctrl+c"}]
    sf.type_text("hi\n")
    assert server.last_json() == [{"type": "type", "text": "hi\n"}]
    sf.mouse_down(5, 5)
    assert server.last_json() == [{"type": "down", "button": "left", "x": 5, "y": 5}]
    sf.mouse_up()
    assert server.last_json() == [{"type": "up", "button": "left"}]
    assert server.seen[-1].headers["content-type"] == "application/json"


def test_drag_moves_in_steps(sf, server):
    sf.drag(0, 0, 100, 50, steps=2, step_ms=5)
    assert server.last_json() == [
        {"type": "down", "button": "left", "x": 0, "y": 0},
        {"type": "pause", "ms": 5},
        {"type": "move", "x": 50, "y": 25},
        {"type": "pause", "ms": 5},
        {"type": "move", "x": 100, "y": 50},
        {"type": "up", "button": "left"},
    ]


def test_actions_batch_flattens_and_extends_timeout(sf, server):
    sf.actions([act.key("ctrl+l"), act.pause(20_000), act.drag(0, 0, 1, 1, steps=1), act.type_text("x")])
    body = server.last_json()
    assert [a["type"] for a in body] == ["key", "pause", "down", "pause", "move", "up", "type"]
    assert server.seen[-1].extensions["timeout"]["read"] == pytest.approx(30 + 20.01 + 0.01)


def test_half_a_position_is_refused():
    with pytest.raises(ValueError):
        act.click(x=1)


def test_read_only(sf, server):
    server.read_only = True
    with pytest.raises(ReadOnlyError) as e:
        sf.key("Enter")
    assert e.value.status == 403
    assert "read-only" in e.value.message


def test_wrong_password():
    with (
        Starfold("http://desk:6901", password="nope", transport=httpx.MockTransport(Server())) as c,
        pytest.raises(AuthError),
    ):
        c.screenshot()


def test_password_from_environment(monkeypatch):
    monkeypatch.setenv("VOXL_PASSWORD", "pw")
    with Starfold("http://desk:6901", transport=httpx.MockTransport(Server())) as c:
        assert c.screenshot().seq == 41


def test_recording(sf, server):
    assert sf.record(2.5) == b"mp4:2.5"
    assert server.seen[-1].extensions["timeout"]["read"] == 32.5
    rec = sf.record_start()
    assert rec == Recording(id=3, max_seconds=300)
    assert sf.record_stop(rec) == b"mp4"
    with pytest.raises(NotFoundError):
        sf.record_stop(4)


def test_files(sf, server, tmp_path):
    listing = sf.files_list("Documents")
    assert listing.path == "Documents"
    assert [(e.name, e.dir, e.size, e.mtime_ms) for e in listing.entries] == [("a.txt", False, 3, 1700000000000)]
    with pytest.raises(ForbiddenError) as e:
        sf.files_download("a.txt")
    assert not isinstance(e.value, ReadOnlyError)
    local = tmp_path / "up.bin"
    local.write_bytes(b"12345")
    up = sf.files_upload("up.bin", local, sub="x")
    assert (up.path, up.size) == ("Downloads/up.bin", 5)
    assert dict(server.seen[-1].url.params) == {"name": "up.bin", "sub": "x"}
    assert sf.files_upload("b.bin", b"ab", path="Desktop").size == 2


def test_other_errors_keep_status():
    def handler(req):
        return httpx.Response(400, json={"error": "region is off screen"})

    with Starfold(password="pw", transport=httpx.MockTransport(handler)) as c, pytest.raises(StarfoldError) as e:
        c.wait(region=(9999, 0, 1, 1))
    assert e.value.status == 400
    assert "off screen" in str(e.value)


def test_transport_errors_are_wrapped():
    def timeout(req):
        raise httpx.ReadTimeout("slow", request=req)

    def refused(req):
        raise httpx.ConnectError("refused", request=req)

    with Starfold(transport=httpx.MockTransport(timeout)) as c, pytest.raises(StarfoldTimeoutError):
        c.screenshot()
    with Starfold(transport=httpx.MockTransport(refused)) as c, pytest.raises(StarfoldConnectionError):
        c.screenshot()


def test_other_403s_on_input_are_not_read_only():
    def handler(req):
        return httpx.Response(403, text="host other.example is not allowed (--allowed-hosts)")

    with Starfold(transport=httpx.MockTransport(handler)) as c, pytest.raises(ForbiddenError) as e:
        c.click()
    assert not isinstance(e.value, ReadOnlyError)


def test_url_prefix_is_kept():
    paths = []

    def handler(req):
        paths.append(req.url.path)
        return httpx.Response(403, json={"error": "this server is read-only (--read-only): input is refused"})

    with Starfold("http://proxy/desk/", transport=httpx.MockTransport(handler)) as c, pytest.raises(ReadOnlyError):
        c.key("a")
    assert paths == ["/desk/input"]


def test_async_client(server):
    async def run():
        async with AsyncStarfold("http://desk:6901", password="pw", transport=httpx.MockTransport(server)) as c:
            shot = await c.screenshot()
            r = await c.type_text("ok")
            w = await c.wait(change=True, since=r.seq)
            server.read_only = True
            with pytest.raises(ReadOnlyError):
                await c.click()
            return shot, r, w

    shot, r, w = asyncio.run(run())
    assert shot.seq == 41
    assert r.seq == 40
    assert w.changed
