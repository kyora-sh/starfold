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
    """The release's `version` job checks each file `versions.py` reads, versions and exact pins;
    RELEASING.md's "Each release" has to name them, bun.lock, which records the npm packages' versions,
    and both Python projects' uv.lock."""
    workflow = (ROOT / ".github" / "workflows" / "release.yml").read_text()
    checked = set(re.findall(r"versions\.py (?:cargo|check-pin \S+|check \S+) (\S+/\S+\.(?:toml|json))", workflow))
    assert {
        "crates/starfold-server/Cargo.toml",
        "python/starfold/pyproject.toml",
        "python/starfold-server/pyproject.toml",
        "packages/starfold/package.json",
        "packages/server/package.json",
    } <= checked
    pins = set(re.findall(r"versions\.py check-pin \S+ (\S+) starfold ", workflow))
    assert pins == {"python/starfold-server/pyproject.toml", "packages/server/package.json"}
    releasing = (ROOT / "RELEASING.md").read_text()
    steps = releasing.split("## Each release", 1)[1]
    for f in sorted(checked) + ["bun.lock", "python/starfold/uv.lock", "python/starfold-server/uv.lock"]:
        assert f"`{f}`" in steps, f"RELEASING.md's Each release doesn't name {f}"


def test_third_party_describes_both_npm_packages():
    text = DOC.read_text()
    client = (
        r"\*\*`starfold` on npm\*\*, MIT: the TypeScript client alone\."
        r"\s+It carries no binary and depends on\s+nothing"
    )
    assert re.search(client, text), "THIRD_PARTY.md doesn't describe npm's starfold as the client alone"
    assert "**`@starfold/server` on npm**" in text


def _changelog_040() -> str:
    text = (ROOT / "CHANGELOG.md").read_text()
    return text.split("## 0.4.0", 1)[1].split("\n## ", 1)[0]


def test_changelog_names_every_moved_python_name():
    from starfold import MOVED_TO_SERVER

    entry = _changelog_040()
    missing = [n for n in sorted(MOVED_TO_SERVER) if f"`{n}`" not in entry]
    assert not missing, f"CHANGELOG's 0.4.0 entry doesn't name {missing}"


def test_changelog_names_every_moved_typescript_name():
    index = (ROOT / "packages" / "server" / "src" / "index.ts").read_text()
    names = set(re.findall(r"^\t(\w+),$", index, re.M)) | set(re.findall(r"export type \{ (\w+) \}", index))
    assert {"startServer", "StartServerOptions", "LINUX_LIBRARIES_HINT"} <= names
    entry = _changelog_040()
    missing = [n for n in sorted(names) if f"`{n}`" not in entry]
    assert not missing, f"CHANGELOG's 0.4.0 entry doesn't name {missing}"


def test_third_party_doesnt_call_the_server_packages_mit():
    text = DOC.read_text()
    assert not re.search(r"`-win32-x64` on\s+npm\*\*, MIT", text), "THIRD_PARTY.md calls the server packages MIT"
