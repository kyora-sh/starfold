"""Drive a starfold desktop from Python: screenshots, waiting for the screen, mouse and keyboard."""

from typing import NoReturn

from . import act
from ._client import AsyncStarfold, Starfold
from ._types import (
    AuthError,
    FileEntry,
    FileList,
    ForbiddenError,
    InputResult,
    NotFoundError,
    ReadOnlyError,
    Recording,
    RecordingOffError,
    Screenshot,
    Segments,
    StarfoldConnectionError,
    StarfoldError,
    StarfoldTimeoutError,
    Upload,
    WaitResult,
)

__all__ = [
    "AsyncStarfold",
    "AuthError",
    "FileEntry",
    "FileList",
    "ForbiddenError",
    "InputResult",
    "NotFoundError",
    "ReadOnlyError",
    "Recording",
    "RecordingOffError",
    "Screenshot",
    "Segments",
    "Starfold",
    "StarfoldConnectionError",
    "StarfoldError",
    "StarfoldTimeoutError",
    "Upload",
    "WaitResult",
    "act",
]

MOVED_TO_SERVER = frozenset(
    {
        "BinaryNotFoundError",
        "LocalServer",
        "MissingLibrariesError",
        "ServerStartError",
        "binary_path",
        "check_libraries",
        "start_server",
    }
)


def __getattr__(name: str) -> NoReturn:
    if name in MOVED_TO_SERVER:
        raise ImportError(
            f"{name} moved to the starfold-server package in 0.4.0: pip install starfold-server, "
            f"then from starfold_server import {name}"
        )
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
