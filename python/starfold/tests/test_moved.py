import subprocess
import sys
from importlib.metadata import entry_points

import pytest

import starfold

MOVED = [
    "BinaryNotFoundError",
    "LocalServer",
    "MissingLibrariesError",
    "ServerStartError",
    "binary_path",
    "check_libraries",
    "start_server",
]


def message(name: str) -> str:
    return (
        f"{name} moved to the starfold-server package in 0.4.0: pip install starfold-server, "
        f"then from starfold_server import {name}"
    )


@pytest.mark.parametrize("name", MOVED)
def test_a_moved_name_says_where_it_went(name):
    with pytest.raises(ImportError) as e:
        getattr(starfold, name)
    assert str(e.value) == message(name)


@pytest.mark.parametrize("name", MOVED)
def test_from_import_keeps_the_message(name):
    r = subprocess.run(
        [sys.executable, "-c", f"from starfold import {name}"], capture_output=True, text=True, timeout=60
    )
    assert r.returncode == 1
    assert r.stderr.strip().splitlines()[-1] == f"ImportError: {message(name)}"


def test_moved_names_are_the_ones_exported_no_more():
    assert set(MOVED) == starfold.MOVED_TO_SERVER
    assert not set(MOVED) & set(starfold.__all__)


def test_another_name_is_an_attribute_error():
    with pytest.raises(AttributeError, match="no attribute 'nope'"):
        starfold.nope  # noqa: B018


def test_the_client_has_no_server_module_or_command():
    with pytest.raises(ModuleNotFoundError):
        __import__("starfold._native")
    assert [ep for ep in entry_points(group="console_scripts") if ep.value.startswith("starfold.")] == []
