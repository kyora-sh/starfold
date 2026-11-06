"""Drive a starfold desktop from Python: screenshots, waiting for the screen, mouse and keyboard."""

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
