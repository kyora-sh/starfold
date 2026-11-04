from __future__ import annotations

import io
import json
import os
import zipfile
from collections.abc import Callable, Iterable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Generic, TypeVar

import httpx

from . import act
from ._types import (
    AuthError,
    FileList,
    ForbiddenError,
    InputResult,
    NotFoundError,
    ReadOnlyError,
    Recording,
    Screenshot,
    Segments,
    StarfoldConnectionError,
    StarfoldError,
    StarfoldTimeoutError,
    Upload,
    WaitResult,
)

T = TypeVar("T")

DEFAULT_TIMEOUT = 30.0
DEFAULT_URL = "http://localhost:6901"


@dataclass
class _Req(Generic[T]):
    method: str
    path: str
    parse: Callable[[httpx.Response], T]
    params: dict[str, Any] | None = None
    json: Any = None
    content: bytes | None = None
    headers: dict[str, str] = field(default_factory=dict)
    extra_s: float = 0.0
    """Seconds the server may take on purpose (a wait, a recording), on top of the timeout."""


def _error(resp: httpx.Response) -> StarfoldError:
    try:
        body = resp.json()
        message = str(body["error"])
    except Exception:
        body = None
        message = resp.text.strip() or resp.reason_phrase
    code = resp.status_code
    if code == 401:
        return AuthError(message, code)
    if code == 403:
        read_only = isinstance(body, dict) and body.get("read_only") is True
        return (ReadOnlyError if read_only else ForbiddenError)(message, code)
    if code == 404:
        return NotFoundError(message, code)
    return StarfoldError(f"{code}: {message}", code)


def _check(resp: httpx.Response) -> httpx.Response:
    if resp.is_success:
        return resp
    raise _error(resp)


def _transport_error(e: httpx.TransportError) -> StarfoldError:
    if isinstance(e, httpx.TimeoutException):
        return StarfoldTimeoutError(f"no answer in time: {e}")
    return StarfoldConnectionError(f"cannot reach the server: {e}")


def _screenshot() -> _Req[Screenshot]:
    return _Req("GET", "/screenshot", lambda r: Screenshot(png=r.content, seq=int(r.headers["x-voxl-seq"])))


def _wait(
    idle_ms: int | None,
    timeout_ms: int | None,
    change: bool,
    region: tuple[int, int, int, int] | None,
    since: int | None,
) -> _Req[WaitResult]:
    params: dict[str, Any] = {}
    if idle_ms is not None:
        params["idle"] = idle_ms
    if timeout_ms is not None:
        params["timeout"] = timeout_ms
    if change:
        params["change"] = 1
    if region is not None:
        params["region"] = ",".join(str(int(v)) for v in region)
    if since is not None:
        params["since"] = since
    extra = (timeout_ms if timeout_ms is not None else 10_000) / 1000
    return _Req("GET", "/wait", lambda r: WaitResult.from_json(r.json()), params=params, extra_s=extra)


def _input(actions: Iterable[act.Action | list[act.Action]]) -> _Req[InputResult]:
    flat: list[act.Action] = []
    for a in actions:
        flat.extend(a if isinstance(a, list) else [a])
    paused = sum(a.get("ms", 0) for a in flat if a.get("type") == "pause") / 1000
    # The answer comes once the input is done; a character the keymap lacks takes ~10 ms.
    typing = sum(len(a.get("text", "")) for a in flat if a.get("type") == "type") / 100
    return _Req(
        "POST",
        "/input",
        lambda r: InputResult(seq=int(r.json()["seq"]), events=int(r.json()["events"])),
        json=flat,
        extra_s=paused + typing,
    )


def _segments(r: httpx.Response) -> Segments:
    if r.headers.get("content-type", "").split(";")[0].strip().lower() != "application/zip":
        return Segments([r.content])
    with zipfile.ZipFile(io.BytesIO(r.content)) as z:
        manifest = json.loads(z.read("manifest.json"))
        return Segments([z.read(s["file"]) for s in manifest["segments"]], manifest["end_reason"])


def _record(seconds: float) -> _Req[Segments]:
    return _Req("GET", "/record", _segments, params={"seconds": seconds, "format": "zip"}, extra_s=seconds)


def _record_start() -> _Req[Recording]:
    return _Req(
        "POST", "/record/start", lambda r: Recording(id=int(r.json()["id"]), max_seconds=int(r.json()["max_seconds"]))
    )


def _record_stop(recording: Recording | int) -> _Req[Segments]:
    rid = recording.id if isinstance(recording, Recording) else recording
    return _Req("POST", "/record/stop", _segments, params={"id": rid, "format": "zip"})


def _files_list(path: str) -> _Req[FileList]:
    return _Req("GET", "/files/list", lambda r: FileList.from_json(r.json()), params={"path": path})


def _files_download(path: str) -> _Req[bytes]:
    return _Req("GET", "/files/download", lambda r: r.content, params={"path": path})


def _files_upload(name: str, data: bytes | str | os.PathLike[str], path: str | None, sub: str | None) -> _Req[Upload]:
    body = data if isinstance(data, bytes) else Path(data).read_bytes()
    params = {"name": name}
    if path is not None:
        params["path"] = path
    if sub is not None:
        params["sub"] = sub
    return _Req(
        "PUT",
        "/files/upload",
        lambda r: Upload(path=r.json()["path"], size=int(r.json()["size"])),
        params=params,
        content=body,
        headers={"X-Voxl-Upload": "1"},
    )


class _Base:
    def __init__(self, url: str, password: str | None, timeout: float) -> None:
        self.url = url.rstrip("/")
        password = password if password is not None else os.environ.get("VOXL_PASSWORD")
        self._headers = {"Authorization": f"Bearer {password}"} if password else {}
        self.timeout = timeout

    def _kwargs(self, req: _Req[Any]) -> dict[str, Any]:
        return {
            "params": req.params,
            "json": req.json,
            "content": req.content,
            "headers": req.headers,
            "timeout": self.timeout + req.extra_s,
        }


class Starfold(_Base):
    """A starfold desktop, for agents: screenshots, waiting for the screen, mouse and keyboard.

    `password` defaults to $VOXL_PASSWORD. `timeout` is in seconds; waits and recordings
    get their own length on top.
    """

    def __init__(
        self,
        url: str = DEFAULT_URL,
        password: str | None = None,
        timeout: float = DEFAULT_TIMEOUT,
        *,
        transport: httpx.BaseTransport | None = None,
    ) -> None:
        super().__init__(url, password, timeout)
        self._http = httpx.Client(base_url=self.url, headers=self._headers, transport=transport)

    def __enter__(self) -> Starfold:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def close(self) -> None:
        self._http.close()

    def _do(self, req: _Req[T]) -> T:
        try:
            resp = self._http.request(req.method, req.path, **self._kwargs(req))
        except httpx.TransportError as e:
            raise _transport_error(e) from e
        return req.parse(_check(resp))

    def screenshot(self) -> Screenshot:
        return self._do(_screenshot())

    def wait(
        self,
        idle_ms: int | None = None,
        timeout_ms: int | None = None,
        change: bool = False,
        region: tuple[int, int, int, int] | None = None,
        since: int | None = None,
    ) -> WaitResult:
        """Until the screen stays still for `idle_ms` (default 300), or with `change` until it changes."""
        return self._do(_wait(idle_ms, timeout_ms, change, region, since))

    def actions(self, actions: Iterable[act.Action | list[act.Action]]) -> InputResult:
        """Sends `starfold.act` actions in one request, applied in order."""
        return self._do(_input(actions))

    def move(self, x: int, y: int) -> InputResult:
        return self.actions([act.move(x, y)])

    def click(
        self, x: int | None = None, y: int | None = None, button: act.Button = "left", count: int = 1
    ) -> InputResult:
        return self.actions([act.click(x, y, button, count)])

    def double_click(self, x: int | None = None, y: int | None = None, button: act.Button = "left") -> InputResult:
        return self.actions([act.click(x, y, button, 2)])

    def mouse_down(self, x: int | None = None, y: int | None = None, button: act.Button = "left") -> InputResult:
        return self.actions([act.down(x, y, button)])

    def mouse_up(self, x: int | None = None, y: int | None = None, button: act.Button = "left") -> InputResult:
        return self.actions([act.up(x, y, button)])

    def drag(
        self, x1: int, y1: int, x2: int, y2: int, button: act.Button = "left", steps: int = 10, step_ms: int = 10
    ) -> InputResult:
        return self.actions(act.drag(x1, y1, x2, y2, button, steps, step_ms))

    def scroll(self, dx: int = 0, dy: int = 0, x: int | None = None, y: int | None = None) -> InputResult:
        return self.actions([act.scroll(dx, dy, x, y)])

    def key(self, combo: str) -> InputResult:
        return self.actions([act.key(combo)])

    def type_text(self, text: str) -> InputResult:
        return self.actions([act.type_text(text)])

    def record(self, seconds: float) -> Segments:
        """An MP4 of the next `seconds`."""
        return self._do(_record(seconds))

    def record_start(self) -> Recording:
        return self._do(_record_start())

    def record_stop(self, recording: Recording | int) -> Segments:
        return self._do(_record_stop(recording))

    def files_list(self, path: str = "") -> FileList:
        return self._do(_files_list(path))

    def files_download(self, path: str) -> bytes:
        """A file's bytes, or a folder as a zip."""
        return self._do(_files_download(path))

    def files_upload(
        self, name: str, data: bytes | str | os.PathLike[str], path: str | None = None, sub: str | None = None
    ) -> Upload:
        """Uploads `data` (bytes or a local file) as `name` into `path` (default: the upload folder)."""
        return self._do(_files_upload(name, data, path, sub))


class AsyncStarfold(_Base):
    """`Starfold` for asyncio: the same methods, awaited."""

    def __init__(
        self,
        url: str = DEFAULT_URL,
        password: str | None = None,
        timeout: float = DEFAULT_TIMEOUT,
        *,
        transport: httpx.AsyncBaseTransport | None = None,
    ) -> None:
        super().__init__(url, password, timeout)
        self._http = httpx.AsyncClient(base_url=self.url, headers=self._headers, transport=transport)

    async def __aenter__(self) -> AsyncStarfold:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.close()

    async def close(self) -> None:
        await self._http.aclose()

    async def _do(self, req: _Req[T]) -> T:
        try:
            resp = await self._http.request(req.method, req.path, **self._kwargs(req))
        except httpx.TransportError as e:
            raise _transport_error(e) from e
        return req.parse(_check(resp))

    async def screenshot(self) -> Screenshot:
        return await self._do(_screenshot())

    async def wait(
        self,
        idle_ms: int | None = None,
        timeout_ms: int | None = None,
        change: bool = False,
        region: tuple[int, int, int, int] | None = None,
        since: int | None = None,
    ) -> WaitResult:
        return await self._do(_wait(idle_ms, timeout_ms, change, region, since))

    async def actions(self, actions: Iterable[act.Action | list[act.Action]]) -> InputResult:
        return await self._do(_input(actions))

    async def move(self, x: int, y: int) -> InputResult:
        return await self.actions([act.move(x, y)])

    async def click(
        self, x: int | None = None, y: int | None = None, button: act.Button = "left", count: int = 1
    ) -> InputResult:
        return await self.actions([act.click(x, y, button, count)])

    async def double_click(
        self, x: int | None = None, y: int | None = None, button: act.Button = "left"
    ) -> InputResult:
        return await self.actions([act.click(x, y, button, 2)])

    async def mouse_down(self, x: int | None = None, y: int | None = None, button: act.Button = "left") -> InputResult:
        return await self.actions([act.down(x, y, button)])

    async def mouse_up(self, x: int | None = None, y: int | None = None, button: act.Button = "left") -> InputResult:
        return await self.actions([act.up(x, y, button)])

    async def drag(
        self, x1: int, y1: int, x2: int, y2: int, button: act.Button = "left", steps: int = 10, step_ms: int = 10
    ) -> InputResult:
        return await self.actions(act.drag(x1, y1, x2, y2, button, steps, step_ms))

    async def scroll(self, dx: int = 0, dy: int = 0, x: int | None = None, y: int | None = None) -> InputResult:
        return await self.actions([act.scroll(dx, dy, x, y)])

    async def key(self, combo: str) -> InputResult:
        return await self.actions([act.key(combo)])

    async def type_text(self, text: str) -> InputResult:
        return await self.actions([act.type_text(text)])

    async def record(self, seconds: float) -> Segments:
        return await self._do(_record(seconds))

    async def record_start(self) -> Recording:
        return await self._do(_record_start())

    async def record_stop(self, recording: Recording | int) -> Segments:
        return await self._do(_record_stop(recording))

    async def files_list(self, path: str = "") -> FileList:
        return await self._do(_files_list(path))

    async def files_download(self, path: str) -> bytes:
        return await self._do(_files_download(path))

    async def files_upload(
        self, name: str, data: bytes | str | os.PathLike[str], path: str | None = None, sub: str | None = None
    ) -> Upload:
        return await self._do(_files_upload(name, data, path, sub))
