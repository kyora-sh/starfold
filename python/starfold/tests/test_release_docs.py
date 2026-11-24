"""THIRD_PARTY.md, which every release archive, npm server package and platform wheel carries, states the
licence expression each of those packages declares: the one python-platform-wheel.py writes into the
wheel's License-Expression and npm-server-packages.sh into package.json's license."""

import re
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[3]
DOC = ROOT / "THIRD_PARTY.md"
WHEEL = ROOT / ".github" / "release" / "python-platform-wheel.py"
TARGETS = {
    "x86_64-linux": ("@starfold/server-linux-x64-gnu", "manylinux_2_39_x86_64"),
    "aarch64-linux": ("@starfold/server-linux-arm64-gnu", "manylinux_2_39_aarch64"),
    "aarch64-macos": ("@starfold/server-darwin-arm64", "macosx_11_0_arm64"),
    "x86_64-windows": ("@starfold/server-win32-x64", "win_amd64"),
}

pytestmark = pytest.mark.skipif(not (DOC.is_file() and WHEEL.is_file()), reason="needs the repository checkout")


def _stated() -> dict[str, str]:
    """`target` (`npm package`, `wheel tag`): `expression` lines of THIRD_PARTY.md."""
    found = {}
    for line in DOC.read_text().splitlines():
        if m := re.fullmatch(r"\s*- `([a-z0-9_]+-[a-z]+)` \(`([^`]+)`, `([^`]+)`\): `([^`]+)`", line):
            target, npm, tag, expr = m.groups()
            assert TARGETS.get(target) == (npm, tag), f"THIRD_PARTY.md pairs {target} with {npm} and {tag}"
            found[target] = expr
    return found


@pytest.mark.parametrize("target", TARGETS)
def test_third_party_states_the_declared_expression(target):
    declared = subprocess.run(
        [sys.executable, "-I", str(WHEEL), "licence", target], capture_output=True, text=True, check=True
    ).stdout.strip()
    stated = _stated()
    assert target in stated, f"THIRD_PARTY.md states no expression for {target}; its packages declare {declared}"
    assert stated[target] == declared


def test_each_release_names_every_versioned_file():
    """The release's `version` job checks each file `versions.py` reads; RELEASING.md's "Each release"
    has to name them, and bun.lock, which records packages/starfold's version."""
    workflow = (ROOT / ".github" / "workflows" / "release.yml").read_text()
    checked = set(re.findall(r"versions\.py (?:cargo|check \S+) (\S+/\S+\.(?:toml|json))", workflow))
    assert {"crates/starfold-server/Cargo.toml", "packages/starfold/package.json"} <= checked
    releasing = (ROOT / "RELEASING.md").read_text()
    steps = releasing.split("## Each release", 1)[1]
    for f in sorted(checked) + ["bun.lock"]:
        assert f"`{f}`" in steps, f"RELEASING.md's Each release doesn't name {f}"


def test_both_sdks_run_the_same_guard():
    """The TypeScript SDK starts its server through the same sh guard as this one, byte for byte."""
    from starfold._native import GUARD

    ts = (ROOT / "packages" / "starfold" / "src" / "native.ts").read_text()
    m = re.search(r"^const GUARD = `(.*?)`;$", ts, re.S | re.M)
    assert m, "native.ts has no GUARD"
    assert m.group(1).strip("\n") == GUARD.strip("\n"), "native.ts's GUARD differs from _native.py's"


def test_third_party_doesnt_call_the_server_packages_mit():
    text = DOC.read_text()
    assert not re.search(r"`-win32-x64` on\s+npm\*\*, MIT", text), "THIRD_PARTY.md calls the server packages MIT"
