"""Prints the version a release input declares, read with a parser for its format.

  versions.py cargo <Cargo.toml>          [package] version
  versions.py pyproject <pyproject.toml>  [project] version
  versions.py package-json <tarball>      package/package.json's top-level "version"
  versions.py wheel <.whl>                the first Version header of the .dist-info/METADATA
  versions.py sdist <.tar.gz>             the first Version header of <name>-<version>/PKG-INFO
  versions.py dsc <.dsc>                  the Version field of the source package's paragraph
"""

import json
import sys
import tarfile
import tomllib
import zipfile
from email.parser import HeaderParser


def fail(message):
    print(f"versions.py: {message}", file=sys.stderr)
    sys.exit(1)


def header_version(text, where):
    version = HeaderParser().parsestr(text).get("Version")
    if not version:
        fail(f"{where} has no Version header")
    return version


def one(names, where):
    if len(names) != 1:
        fail(f"{where}: expected exactly one, found {names}")
    return names[0]


def main(kind, path):
    if kind == "cargo":
        with open(path, "rb") as f:
            return tomllib.load(f)["package"]["version"]
    if kind == "pyproject":
        with open(path, "rb") as f:
            return tomllib.load(f)["project"]["version"]
    if kind == "package-json":
        with tarfile.open(path) as t:
            data = json.load(t.extractfile("package/package.json"))
        if not isinstance(data.get("version"), str):
            fail(f"{path}: package.json has no top-level version")
        return data["version"]
    if kind == "wheel":
        with zipfile.ZipFile(path) as z:
            name = one([n for n in z.namelist() if n.count("/") == 1 and n.endswith(".dist-info/METADATA")], path)
            return header_version(z.read(name).decode(), f"{path}:{name}")
    if kind == "sdist":
        with tarfile.open(path) as t:
            name = one([n for n in t.getnames() if n.count("/") == 1 and n.endswith("/PKG-INFO")], path)
            return header_version(t.extractfile(name).read().decode(), f"{path}:{name}")
    if kind == "dsc":
        lines = open(path, encoding="utf-8").read().splitlines()
        if lines and lines[0].startswith("-----BEGIN PGP SIGNED MESSAGE-----"):
            lines = lines[lines.index("") + 1 :]
        paragraph = lines[: lines.index("")] if "" in lines else lines
        return header_version("\n".join(paragraph) + "\n", path)
    fail(f"unknown kind {kind}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        fail(__doc__)
    print(main(sys.argv[1], sys.argv[2]))
