import os
import re
import stat
import subprocess
import sys
from importlib.metadata import entry_points
from pathlib import Path

import pytest
import starfold
from starfold import StarfoldError

import starfold_server
from starfold_server import BinaryNotFoundError, MissingLibrariesError, _native

ROOT = Path(__file__).resolve().parents[3]

posix_only = pytest.mark.skipif(sys.platform == "win32", reason="a shell script stands in for the binary")


@pytest.fixture
def no_bundled(monkeypatch, tmp_path):
    monkeypatch.delenv("STARFOLD_BIN", raising=False)
    monkeypatch.setattr(_native, "_bundled", lambda: tmp_path / "_bin" / "starfold")
    return tmp_path / "_bin" / "starfold"


def test_env_wins_over_the_bundled_binary(no_bundled, tmp_path, monkeypatch):
    no_bundled.parent.mkdir()
    no_bundled.write_bytes(b"bundled")
    mine = tmp_path / "mine"
    mine.write_bytes(b"mine")
    monkeypatch.setenv("STARFOLD_BIN", str(mine))
    assert starfold_server.binary_path() == str(mine)


def test_bundled_binary_without_env(no_bundled):
    no_bundled.parent.mkdir()
    no_bundled.write_bytes(b"bundled")
    assert starfold_server.binary_path() == str(no_bundled)


def test_empty_env_is_unset(no_bundled, monkeypatch):
    no_bundled.parent.mkdir()
    no_bundled.write_bytes(b"bundled")
    monkeypatch.setenv("STARFOLD_BIN", "")
    assert starfold_server.binary_path() == str(no_bundled)


def test_env_naming_no_file_is_an_error(no_bundled, tmp_path, monkeypatch):
    monkeypatch.setenv("STARFOLD_BIN", str(tmp_path / "nope"))
    with pytest.raises(BinaryNotFoundError, match="STARFOLD_BIN is .*nope, which isn't a file"):
        starfold_server.binary_path()


def test_no_binary_names_platform_supported_list_and_remote_use(no_bundled, monkeypatch):
    monkeypatch.setattr(_native.platform, "system", lambda: "Linux")
    monkeypatch.setattr(_native.platform, "machine", lambda: "riscv64")
    with pytest.raises(BinaryNotFoundError) as e:
        starfold_server.binary_path()
    msg = str(e.value)
    assert "(Linux riscv64)" in msg
    for p in _native.SUPPORTED:
        assert p in msg
    assert "STARFOLD_BIN" in msg
    assert "still works against a server running elsewhere" in msg
    assert isinstance(e.value, StarfoldError)


def test_the_bundled_path_is_in_the_package():
    name = "starfold.exe" if sys.platform == "win32" else "starfold"
    assert _native._bundled() == Path(_native.__file__).parent / "_bin" / name


LDD_MISSING = """\
\tlinux-vdso.so.1 (0x00007ffd)
\tlibyuv.so.0 => not found
\tlibopus.so.0 => /lib/x86_64-linux-gnu/libopus.so.0 (0x00007f)
\tlibpulse.so.0 => not found
\tlibc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x00007f)
"""
LDD_OLD_GLIBC = "./starfold: /lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.39' not found (required by ./starfold)\n"


def test_ldd_output_parsing():
    assert _native._ldd_missing(LDD_MISSING) == ["libyuv.so.0", "libpulse.so.0"]
    assert _native._ldd_missing(LDD_OLD_GLIBC + LDD_OLD_GLIBC) == ["GLIBC_2.39"]
    assert _native._ldd_missing("\tlibc.so.6 => /lib/libc.so.6 (0x1)\n") == []


def test_missing_libraries_error_text(monkeypatch):
    monkeypatch.setattr(_native.sys, "platform", "linux")
    monkeypatch.setattr(_native.shutil, "which", lambda _: "/usr/bin/ldd")
    monkeypatch.setattr(
        _native.subprocess,
        "run",
        lambda *a, **k: subprocess.CompletedProcess(a[0], 1, LDD_MISSING, ""),
    )
    with pytest.raises(MissingLibrariesError) as e:
        starfold_server.check_libraries("/x/starfold")
    assert str(e.value) == (
        "the starfold server can't start: these shared libraries are missing: libyuv.so.0, libpulse.so.0. "
        "It needs glibc 2.39+ (Ubuntu 24.04 or newer) and:\n\n    sudo apt install libyuv0 libopus0 libpulse0\n"
    )


def test_library_check_passes_and_is_linux_only(monkeypatch):
    calls = []
    monkeypatch.setattr(_native.shutil, "which", lambda _: "/usr/bin/ldd")
    monkeypatch.setattr(
        _native.subprocess,
        "run",
        lambda *a, **k: calls.append(a) or subprocess.CompletedProcess(a[0], 0, "\tlibc.so.6 => /l (0x1)\n", ""),
    )
    monkeypatch.setattr(_native.sys, "platform", "darwin")
    starfold_server.check_libraries("/x/starfold")
    assert calls == []
    monkeypatch.setattr(_native.sys, "platform", "linux")
    starfold_server.check_libraries("/x/starfold")
    assert calls == [(["/usr/bin/ldd", "--", "/x/starfold"],)]


def test_console_script_is_declared():
    (ep,) = entry_points(group="console_scripts", name="starfold")
    assert ep.value == "starfold_server._native:main"


def test_the_errors_are_the_clients():
    for e in (BinaryNotFoundError, MissingLibrariesError, starfold_server.ServerStartError):
        assert issubclass(e, starfold.StarfoldError)


def test_the_client_is_the_starfold_packages():
    server = _native.LocalServer.__new__(_native.LocalServer)
    server.url, server.password, server._client = "http://127.0.0.1:1", "pw", None
    sf = server.client()
    try:
        assert type(sf) is starfold.Starfold
    finally:
        sf.close()


def test_the_typescript_server_runs_the_same_guard():
    """packages/server starts its server through the same sh guard as this package, byte for byte."""
    ts_file = ROOT / "packages" / "server" / "src" / "native.ts"
    if not ts_file.is_file():
        pytest.skip("needs the repository checkout")
    m = re.search(r"^const GUARD = `(.*?)`;$", ts_file.read_text(), re.S | re.M)
    assert m, "native.ts has no GUARD"
    assert m.group(1).strip("\n") == _native.GUARD.strip("\n"), "native.ts's GUARD differs from _native.py's"


def _fake_binary(tmp_path: Path) -> Path:
    fake = tmp_path / "starfold"
    fake.write_text('#!/bin/sh\nprintf "%s|" "$@"\nexit 7\n')
    fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
    return fake


def _run_main(*args: str, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    code = "from starfold_server._native import main; main()"
    return subprocess.run([sys.executable, "-c", code, *args], env=env, capture_output=True, text=True)


@posix_only
def test_console_script_runs_the_binary_with_args_and_exit_code(tmp_path):
    env = {**os.environ, "STARFOLD_BIN": str(_fake_binary(tmp_path)), "PATH": ""}
    r = _run_main("--port=1", "two words", env=env)
    assert r.stdout == "--port=1|two words|"
    assert r.returncode == 7


def test_console_script_without_binary_says_why(tmp_path):
    env = {**os.environ, "STARFOLD_BIN": str(tmp_path / "missing")}
    r = _run_main("--version", env=env)
    assert r.returncode == 1
    assert r.stdout == ""
    assert r.stderr.startswith("starfold: STARFOLD_BIN is ")


@posix_only
def test_start_server_reports_an_early_exit_with_the_output(tmp_path):
    fake = tmp_path / "starfold"
    fake.write_text('#!/bin/sh\necho "bad flag: $*" >&2\nexit 2\n')
    fake.chmod(0o755)
    exited = r"exited with 2:\nbad flag: --addr=127\.0\.0\.1 --port=\d+ --x"
    with pytest.raises(starfold_server.ServerStartError, match=exited) as e:
        starfold_server.start_server(binary=str(fake), args=["--x"], timeout=10)
    assert e.value.output.startswith("bad flag: --addr=127.0.0.1 --port=")
    assert isinstance(e.value, StarfoldError)


@posix_only
def test_start_server_times_out_with_the_output(tmp_path):
    fake = tmp_path / "starfold"
    fake.write_text('#!/bin/sh\necho "still starting"\nexec sleep 30\n')
    fake.chmod(0o755)
    late = "didn't answer at http://127.0.0.1:\\d+ in time:\nstill starting"
    with pytest.raises(starfold_server.ServerStartError, match=late):
        starfold_server.start_server(binary=str(fake), timeout=1)


@posix_only
def test_env_is_merged_onto_the_environment(tmp_path):
    seen = tmp_path / "seen"
    fake = tmp_path / "starfold"
    fake.write_text(f'#!/bin/sh\nprintf "%s|%s" "$PATH" "$X" > {seen}\nexit 3\n')
    fake.chmod(0o755)
    with pytest.raises(StarfoldError):
        starfold_server.start_server(binary=str(fake), env={"X": "1"}, timeout=10)
    assert seen.read_text() == f"{os.environ['PATH']}|1", "env={'X': '1'} dropped the rest of the environment"


def test_base_path_from_the_server_flags():
    assert _native._base_path([]) == ""
    assert _native._base_path(["--base-path=/desktop/"]) == "/desktop"
    assert _native._base_path(["--title", "x", "--base-path", "/a/b"]) == "/a/b"
    assert _native._base_path(["--base_path=/u"]) == "/u"
    assert _native._base_path(["--base-path", "--title", "x"]) == ""


def test_url_for_wildcard_and_ipv6_addresses():
    assert _native._url("127.0.0.1", 5) == "http://127.0.0.1:5"
    assert _native._url("0.0.0.0", 5) == "http://127.0.0.1:5"
    assert _native._url("::", 5) == "http://[::1]:5"
    assert _native._url("::1", 5) == "http://[::1]:5"
