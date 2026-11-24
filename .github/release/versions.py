"""The version a release input declares, read with a parser for its format.

  versions.py <kind> <path>                   prints it
  versions.py check <kind> <path> <expected>  fails unless it is exactly <expected>
  versions.py dsc-sha256 <.dsc>               prints the .dsc's Checksums-Sha256 lines (sha256 size name)

Kinds:
  cargo <Cargo.toml>          [package] version
  pyproject <pyproject.toml>  [project] version
  package-json <tarball>      package/package.json's top-level "version"
  package-json-file <file>    a package.json's top-level "version"
  wheel <.whl>                the first Version header of the .dist-info/METADATA
  sdist <.tar.gz>             the first Version header of <name>-<version>/PKG-INFO
  dsc <.dsc>                  the Version field of the source package's paragraph

A version must be one line of letters, digits and . + ~ : - (anything else, a newline included,
fails), so a value can't hide behind a shell's trimming.
"""

import json
import re
import sys
import tarfile
import tomllib
import zipfile
from email.parser import HeaderParser

VERSION = re.compile(r"[0-9A-Za-z.+~:-]+")


def fail(message):
    print(f"versions.py: {message}", file=sys.stderr)
    sys.exit(1)


def header_version(text, where):
    versions = HeaderParser().parsestr(text).get_all("Version") or []
    if len(versions) != 1:
        fail(f"{where} has {len(versions)} Version headers, not one")
    return versions[0]


def one(names, where):
    if len(names) != 1:
        fail(f"{where}: expected exactly one, found {names}")
    return names[0]


def dsc_paragraph(path):
    lines = open(path, encoding="utf-8").read().splitlines()
    if lines and lines[0].startswith("-----BEGIN PGP SIGNED MESSAGE-----"):
        lines = lines[lines.index("") + 1 :]
    return lines[: lines.index("")] if "" in lines else lines


def read(kind, path):
    if kind == "cargo":
        with open(path, "rb") as f:
            return tomllib.load(f)["package"]["version"]
    if kind == "pyproject":
        with open(path, "rb") as f:
            return tomllib.load(f)["project"]["version"]
    if kind in ("package-json", "package-json-file"):
        if kind == "package-json":
            with tarfile.open(path) as t:
                data = json.load(t.extractfile("package/package.json"))
        else:
            with open(path, encoding="utf-8") as f:
                data = json.load(f)
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
        return header_version("\n".join(dsc_paragraph(path)) + "\n", path)
    fail(f"unknown kind {kind}")


def version(kind, path):
    v = read(kind, path)
    if not isinstance(v, str) or not VERSION.fullmatch(v):
        fail(f"{path}: {kind} version {v!r} isn't one line of letters, digits and . + ~ : -")
    return v


def dsc_sha256(path):
    """The Checksums-Sha256 rows, which must name exactly the files the Files field lists, once each."""
    rows, fields, field = {"checksums-sha256": [], "files": []}, {}, None
    for line in dsc_paragraph(path):
        if line[:1] in (" ", "\t"):
            if field in rows and line.strip():
                rows[field].append(line.split())
        else:
            field = line.split(":", 1)[0].strip().lower()
            fields[field] = fields.get(field, 0) + 1
    lines = rows["checksums-sha256"]
    if not lines or any(len(parts) != 3 or not re.fullmatch(r"[0-9a-f]{64}", parts[0]) for parts in lines):
        fail(f"{path}: no well-formed Checksums-Sha256 field")
    if fields.get("checksums-sha256") != 1 or fields.get("files") != 1:
        fail(f"{path}: Checksums-Sha256 and Files must each appear once")
    named = [parts[2] for parts in lines]
    files = [parts[-1] for parts in rows["files"]]
    if len(set(named)) != len(named) or sorted(named) != sorted(files):
        fail(f"{path}: Checksums-Sha256 names {sorted(named)}, Files lists {sorted(files)}")
    return lines


if __name__ == "__main__":
    args = sys.argv[1:]
    if len(args) == 4 and args[0] == "check":
        got = version(args[1], args[2])
        if got != args[3]:
            fail(f"{args[2]}: {args[1]} version {got!r}, expected {args[3]!r}")
    elif len(args) == 2 and args[0] == "dsc-sha256":
        for sha256, size, name in dsc_sha256(args[1]):
            print(sha256, size, name)
    elif len(args) == 2:
        print(version(args[0], args[1]))
    else:
        fail(__doc__)
