import os
import stat
import subprocess
import sys
from importlib.metadata import entry_points
from pathlib import Path

import pytest

import starfold
from starfold import BinaryNotFoundError, MissingLibrariesError, StarfoldError, _native

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
    assert starfold.binary_path() == str(mine)


def test_bundled_binary_without_env(no_bundled):
    no_bundled.parent.mkdir()
    no_bundled.write_bytes(b"bundled")
    assert starfold.binary_path() == str(no_bundled)


def test_empty_env_is_unset(no_bundled, monkeypatch):
    no_bundled.parent.mkdir()
    no_bundled.write_bytes(b"bundled")
    monkeypatch.setenv("STARFOLD_BIN", "")
    assert starfold.binary_path() == str(no_bundled)


def test_env_naming_no_file_is_an_error(no_bundled, tmp_path, monkeypatch):
    monkeypatch.setenv("STARFOLD_BIN", str(tmp_path / "nope"))
    with pytest.raises(BinaryNotFoundError, match="STARFOLD_BIN is .*nope, which isn't a file"):
        starfold.binary_path()


def test_no_binary_names_platform_supported_list_and_remote_use(no_bundled, monkeypatch):
    monkeypatch.setattr(_native.platform, "system", lambda: "Linux")
    monkeypatch.setattr(_native.platform, "machine", lambda: "riscv64")
    with pytest.raises(BinaryNotFoundError) as e:
        starfold.binary_path()
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
        starfold.check_libraries("/x/starfold")
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
    starfold.check_libraries("/x/starfold")
    assert calls == []
    monkeypatch.setattr(_native.sys, "platform", "linux")
    starfold.check_libraries("/x/starfold")
    assert calls == [(["/usr/bin/ldd", "/x/starfold"],)]


def test_console_script_is_declared():
    (ep,) = entry_points(group="console_scripts", name="starfold")
    assert ep.value == "starfold._native:main"


def _fake_binary(tmp_path: Path) -> Path:
    fake = tmp_path / "starfold"
    fake.write_text('#!/bin/sh\nprintf "%s|" "$@"\nexit 7\n')
    fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
    return fake


def _run_main(*args: str, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    code = "from starfold._native import main; main()"
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
    with pytest.raises(StarfoldError, match=r"exited with 2:\nbad flag: --addr=127\.0\.0\.1 --port=\d+ --x"):
        starfold.start_server(binary=str(fake), args=["--x"], timeout=10, env={"PATH": os.environ["PATH"]})


@posix_only
def test_start_server_times_out_with_the_output(tmp_path):
    fake = tmp_path / "starfold"
    fake.write_text('#!/bin/sh\necho "still starting"\nexec sleep 30\n')
    fake.chmod(0o755)
    with pytest.raises(StarfoldError, match="didn't answer at http://127.0.0.1:\\d+ in time:\nstill starting"):
        starfold.start_server(binary=str(fake), timeout=1, env={"PATH": os.environ["PATH"]})


def test_url_for_wildcard_and_ipv6_addresses():
    assert _native._url("127.0.0.1", 5) == "http://127.0.0.1:5"
    assert _native._url("0.0.0.0", 5) == "http://127.0.0.1:5"
    assert _native._url("::", 5) == "http://[::1]:5"
    assert _native._url("::1", 5) == "http://[::1]:5"
