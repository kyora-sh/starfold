"""Drive a starfold desktop from Python: screenshots, waiting for the screen, mouse and keyboard."""

from . import act
from ._client import AsyncStarfold, Starfold
from ._native import (
    BinaryNotFoundError,
    LocalServer,
    MissingLibrariesError,
    binary_path,
    check_libraries,
    start_server,
)
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
    "BinaryNotFoundError",
    "FileEntry",
    "FileList",
    "ForbiddenError",
    "InputResult",
    "LocalServer",
    "MissingLibrariesError",
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
    "binary_path",
    "check_libraries",
    "start_server",
]
