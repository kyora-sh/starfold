"""Start a starfold server on this machine from Python; the client it hands back is the starfold package's."""

from ._native import (
    BinaryNotFoundError,
    LocalServer,
    MissingLibrariesError,
    ServerStartError,
    binary_path,
    check_libraries,
    start_server,
)

__all__ = [
    "BinaryNotFoundError",
    "LocalServer",
    "MissingLibrariesError",
    "ServerStartError",
    "binary_path",
    "check_libraries",
    "start_server",
]
