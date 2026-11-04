from __future__ import annotations

import io
import struct
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from collections.abc import Iterable

    from PIL.Image import Image


class StarfoldError(Exception):
    """A request the server refused or could not answer."""

    def __init__(self, message: str, status: int | None = None) -> None:
        super().__init__(message)
        self.message = message
        self.status = status


class AuthError(StarfoldError):
    """Missing or wrong password (401)."""


class ForbiddenError(StarfoldError):
    """The server refuses this (403), e.g. file transfer is turned off."""


class ReadOnlyError(ForbiddenError):
    """The server runs with --read-only and takes no input."""


class NotFoundError(StarfoldError):
    """No such file, folder or recording (404)."""


class StarfoldTimeoutError(StarfoldError):
    """The server did not answer in time."""


class StarfoldConnectionError(StarfoldError):
    """The server could not be reached."""


@dataclass(frozen=True)
class Screenshot:
    png: bytes
    seq: int
    """Pass as `since` to `wait(change=True)` for changes after this screenshot."""

    @property
    def size(self) -> tuple[int, int]:
        width, height = struct.unpack(">II", self.png[16:24])
        return width, height

    def save(self, path: str | Path) -> Path:
        path = Path(path)
        path.write_bytes(self.png)
        return path

    def image(self) -> Image:
        """The screenshot as a Pillow image (needs `pillow` installed)."""
        try:
            from PIL import Image as PILImage
        except ImportError as e:
            raise ImportError("Screenshot.image() needs Pillow: pip install pillow") from e
        return PILImage.open(io.BytesIO(self.png))


@dataclass(frozen=True)
class WaitResult:
    changed: bool
    timed_out: bool
    seq: int
    waited_ms: int

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> WaitResult:
        return cls(
            changed=bool(d["changed"]),
            timed_out=bool(d["timed_out"]),
            seq=int(d["seq"]),
            waited_ms=int(d["waited_ms"]),
        )


@dataclass(frozen=True)
class InputResult:
    seq: int
    """The screen's change count before the input: `wait(change=True, since=seq)` returns once it changed."""
    events: int


@dataclass(frozen=True)
class Recording:
    id: int
    max_seconds: int


class Segments(list[bytes]):
    """A recording's MP4s, in order: one, unless another encoder took over mid-recording."""

    def __init__(self, parts: Iterable[bytes], end_reason: str | None = None) -> None:
        super().__init__(parts)
        self.end_reason = end_reason
        """Why it ended: stopped, time_limit, resized, segment_cap, bad_parameter_sets or closed."""


@dataclass(frozen=True)
class FileEntry:
    name: str
    dir: bool
    size: int
    mtime_ms: int


@dataclass(frozen=True)
class FileList:
    path: str
    entries: list[FileEntry]

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> FileList:
        return cls(
            path=d["path"],
            entries=[
                FileEntry(name=e["name"], dir=bool(e["dir"]), size=int(e["size"]), mtime_ms=int(e["mtime"]))
                for e in d["entries"]
            ],
        )


@dataclass(frozen=True)
class Upload:
    path: str
    size: int
