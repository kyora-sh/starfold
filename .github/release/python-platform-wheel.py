"""python-platform-wheel.py: a Python package's platform wheels, made from its pure wheel and a release archive.

    build <distribution> <version> <target> <archive> <pure wheel> <out dir>
                                     writes the wheel, prints its path
    verify <distribution> <version> <target> <archive> <wheel>
                                     re-reads the wheel against the archive
    licence <target>                 prints the target's SPDX expression (npm-server-packages.sh reads it too)

<distribution> is the wheel file name's form (starfold_server), which is also the import package the
binary goes into. A platform wheel is the pure wheel plus <distribution>/_bin/starfold[.exe] (mode 0755)
and its .sha256, the archive's LICENSE, THIRD_PARTY.md and licenses/ as licence files, a
License-Expression for what the binary
carries (THIRD_PARTY.md, "What each release binary carries"), Root-Is-Purelib: false, the platform tag and
a new RECORD. Everything is read from the archives in memory and written with zipfile, so nothing is
unpacked to disk and the result doesn't depend on a tool's version. Run it with python3 -I.
"""

import base64
import hashlib
import re
import struct
import sys
import tarfile
import zipfile

TARGETS = {
    "x86_64-linux": ("tar.gz", "starfold", "linux"),
    "aarch64-linux": ("tar.gz", "starfold", "linux"),
    "aarch64-macos": ("tar.gz", "starfold", "macos"),
    "x86_64-windows": ("zip", "starfold.exe", "windows"),
}
GLIBC_MAX = (2, 39)
APT_LINE = "sudo apt install libyuv0 libopus0 libpulse0"
GCC_RUNTIME = "GPL-3.0-or-later WITH GCC-exception-3.1"
LICENCES_ALL = [
    "MIT",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Apache-2.0",
    "Unicode-3.0",
    "Zlib",
    "OFL-1.1",
    "CC0-1.0",
    "LicenseRef-AOM-Patent-License-1.0",
]
LICENCES_PLATFORM = {
    "linux": ["LGPL-2.1-or-later", GCC_RUNTIME],
    "macos": [],
    "windows": ["ZPL-2.1", GCC_RUNTIME],
}
EPOCH = (1980, 1, 1, 0, 0, 0)


def fail(msg: str) -> None:
    sys.exit(f"python-platform-wheel.py: {msg}")


def read_archive(path: str, version: str, target: str) -> tuple[bytes, dict[str, bytes]]:
    """The binary and the licence files (LICENSE, THIRD_PARTY.md, licenses/...) of a release archive."""
    fmt, binary, _ = TARGETS[target]
    root = f"starfold-{version}-{target}/"
    files: dict[str, bytes] = {}
    if fmt == "tar.gz":
        with tarfile.open(path) as t:
            for m in t.getmembers():
                if m.isfile() and m.name.startswith(root):
                    files[m.name[len(root) :]] = t.extractfile(m).read()
    else:
        with zipfile.ZipFile(path) as z:
            for i in z.infolist():
                if not i.is_dir() and i.filename.startswith(root):
                    files[i.filename[len(root) :]] = z.read(i)
    if binary not in files:
        fail(f"{path} has no {root}{binary}")
    notices = {n: b for n, b in files.items() if n in ("LICENSE", "THIRD_PARTY.md") or n.startswith("licenses/")}
    for need in ("LICENSE", "THIRD_PARTY.md"):
        if need not in notices:
            fail(f"{path} has no {root}{need}")
    return files[binary], dict(sorted(notices.items()))


def glibc_versions(elf: bytes) -> list[tuple[int, ...]]:
    """The GLIBC_x.y symbol versions an ELF64 little-endian binary needs (its SHT_GNU_verneed section)."""
    if elf[:4] != b"\x7fELF" or elf[4] != 2 or elf[5] != 1:
        fail("not a 64-bit little-endian ELF binary")
    shoff = struct.unpack_from("<Q", elf, 0x28)[0]
    shentsize, shnum = struct.unpack_from("<HH", elf, 0x3A)
    sections = [struct.unpack_from("<IIQQQQIIQQ", elf, shoff + i * shentsize) for i in range(shnum)]
    found = set()
    for _, sh_type, _, _, off, _, link, count, _, _ in sections:
        if sh_type != 0x6FFFFFFE:
            continue
        strtab = sections[link][4]

        def name(o: int, strtab: int = strtab) -> str:
            return elf[strtab + o : elf.index(b"\0", strtab + o)].decode()

        need = off
        for _ in range(count):
            _, cnt, _, aux, nxt = struct.unpack_from("<HHIII", elf, need)
            a = need + aux
            for _ in range(cnt):
                _, _, _, nm, anext = struct.unpack_from("<IHHII", elf, a)
                if m := re.fullmatch(r"GLIBC_(\d+(?:\.\d+)*)", name(nm)):
                    found.add(tuple(int(x) for x in m.group(1).split(".")))
                a += anext
            need += nxt
    return sorted(found)


def elf_machine(elf: bytes) -> int:
    return struct.unpack_from("<H", elf, 0x12)[0]


def macos_minos(macho: bytes) -> tuple[int, int, int]:
    """The minimum macOS version in a thin arm64 Mach-O's LC_BUILD_VERSION (or LC_VERSION_MIN_MACOSX)."""
    magic, cputype = struct.unpack_from("<II", macho, 0)
    if magic != 0xFEEDFACF or cputype != 0x0100000C:
        fail(f"not a thin arm64 Mach-O (magic {magic:#x}, cputype {cputype:#x})")
    ncmds = struct.unpack_from("<I", macho, 16)[0]
    off = 32
    for _ in range(ncmds):
        cmd, size = struct.unpack_from("<II", macho, off)
        if cmd == 0x32:
            plat, v = struct.unpack_from("<II", macho, off + 8)
            if plat != 1:
                fail(f"LC_BUILD_VERSION's platform is {plat}, not macOS")
            return v >> 16, (v >> 8) & 0xFF, v & 0xFF
        if cmd == 0x24:
            v = struct.unpack_from("<I", macho, off + 8)[0]
            return v >> 16, (v >> 8) & 0xFF, v & 0xFF
        off += size
    fail("the Mach-O has neither LC_BUILD_VERSION nor LC_VERSION_MIN_MACOSX")
    raise AssertionError


def platform_tag(target: str, binary: bytes) -> str:
    if target.endswith("-linux"):
        arch = target.split("-")[0]
        want = {"x86_64": 0x3E, "aarch64": 0xB7}[arch]
        if elf_machine(binary) != want:
            fail(f"the {target} binary's ELF machine is {elf_machine(binary):#x}, not {want:#x}")
        versions = glibc_versions(binary)
        top = max(versions) if versions else (0,)
        print(f"{target}: highest GLIBC_ symbol version needed: {'.'.join(map(str, top))}", file=sys.stderr)
        if top > GLIBC_MAX:
            fail(f"the {target} binary needs GLIBC_{'.'.join(map(str, top))}, above manylinux_2_39")
        return f"manylinux_{GLIBC_MAX[0]}_{GLIBC_MAX[1]}_{arch}"
    if target == "aarch64-macos":
        major, minor, patch = macos_minos(binary)
        print(f"{target}: LC_BUILD_VERSION minos {major}.{minor}.{patch}", file=sys.stderr)
        if major >= 11:
            major, minor = (major, 0) if minor == 0 and patch == 0 else (major + 1, 0)
        elif patch:
            minor += 1
        return f"macosx_{major}_{minor}_arm64"
    if target == "x86_64-windows":
        pe = struct.unpack_from("<I", binary, 0x3C)[0]
        if (
            binary[:2] != b"MZ"
            or binary[pe : pe + 4] != b"PE\0\0"
            or struct.unpack_from("<H", binary, pe + 4)[0] != 0x8664
        ):
            fail("the x86_64-windows binary isn't an x86-64 PE")
        return "win_amd64"
    fail(f"unknown target {target}")
    raise AssertionError


def record_hash(data: bytes) -> str:
    return "sha256=" + base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()


def licence(platform: str) -> str:
    """What a platform's default binary carries, as one SPDX expression."""
    return " AND ".join(LICENCES_ALL + LICENCES_PLATFORM[platform])


def metadata(pure: str, platform: str, files: list[str]) -> str:
    head, sep, body = pure.partition("\n\n")
    lines = [ln for ln in head.split("\n") if not ln.startswith(("License-Expression:", "License-File:"))]
    at = next(i for i, ln in enumerate(lines) if ln.startswith("Requires-Dist:") or ln.startswith("Requires-Python:"))
    expr = licence(platform)
    lines[at:at] = [f"License-Expression: {expr}", *(f"License-File: {f}" for f in files)]
    if platform == "linux" and APT_LINE not in body:
        fail(f"the wheel's description (README.md) doesn't name the system libraries ({APT_LINE})")
    return "\n".join(lines) + sep + body


def wheel_file(info: zipfile.ZipInfo | str, mode: int = 0o644) -> zipfile.ZipInfo:
    name = info.filename if isinstance(info, zipfile.ZipInfo) else info
    z = zipfile.ZipInfo(name, EPOCH)
    z.compress_type = zipfile.ZIP_DEFLATED
    z.external_attr = (0o100000 | mode) << 16
    return z


def check_distribution(distribution: str) -> None:
    if not re.fullmatch(r"[a-z0-9]+(?:_[a-z0-9]+)*", distribution):
        fail(f"{distribution!r} isn't a wheel file name's distribution (lower case, _ between words)")


def build(distribution: str, version: str, target: str, archive: str, pure_wheel: str, out_dir: str) -> None:
    check_distribution(distribution)
    binary, notices = read_archive(archive, version, target)
    _, binname, platform = TARGETS[target]
    tag = platform_tag(target, binary)
    dist = f"{distribution}-{version}.dist-info"
    with zipfile.ZipFile(pure_wheel) as z:
        pure = {i.filename: z.read(i) for i in z.infolist() if not i.is_dir()}
    if f"{dist}/WHEEL" not in pure or f"{distribution}/__init__.py" not in pure:
        fail(f"{pure_wheel} isn't {distribution} {version}'s wheel (no {dist}/WHEEL or {distribution}/__init__.py)")
    if pure.get(f"{dist}/licenses/LICENSE") != notices["LICENSE"]:
        fail(f"{pure_wheel}'s LICENSE isn't the archive's")
    wheel = pure.pop(f"{dist}/WHEEL").decode()
    if "Root-Is-Purelib: true" not in wheel or "Tag: py3-none-any" not in wheel:
        fail(f"{pure_wheel} isn't a pure py3-none-any wheel")
    wheel = wheel.replace("Root-Is-Purelib: true", "Root-Is-Purelib: false").replace(
        "Tag: py3-none-any", f"Tag: py3-none-{tag}"
    )
    meta = metadata(pure.pop(f"{dist}/METADATA").decode(), platform, list(notices))
    pure.pop(f"{dist}/RECORD")
    sha = hashlib.sha256(binary).hexdigest()
    entries: list[tuple[str, bytes, int]] = []
    for name, data in pure.items():
        if not name.startswith(f"{dist}/licenses/"):
            entries.append((name, data, 0o644))
    entries.append((f"{distribution}/_bin/{binname}", binary, 0o755))
    entries.append((f"{distribution}/_bin/{binname}.sha256", f"{sha}  {binname}\n".encode(), 0o644))
    for name, data in notices.items():
        entries.append((f"{dist}/licenses/{name}", data, 0o644))
    entries.append((f"{dist}/METADATA", meta.encode(), 0o644))
    entries.append((f"{dist}/WHEEL", wheel.encode(), 0o644))
    if len({n for n, _, _ in entries}) != len(entries):
        fail("two files with the same name in the wheel")
    record = "".join(f"{n},{record_hash(d)},{len(d)}\n" for n, d, _ in entries) + f"{dist}/RECORD,,\n"
    path = f"{out_dir}/{distribution}-{version}-py3-none-{tag}.whl"
    with zipfile.ZipFile(path, "w") as z:
        for name, data, mode in entries:
            z.writestr(wheel_file(name, mode), data)
        z.writestr(wheel_file(f"{dist}/RECORD"), record.encode())
    print(path)


def verify(distribution: str, version: str, target: str, archive: str, wheel: str) -> None:
    check_distribution(distribution)
    binary, notices = read_archive(archive, version, target)
    _, binname, _ = TARGETS[target]
    want = hashlib.sha256(binary).hexdigest()
    dist = f"{distribution}-{version}.dist-info"
    with zipfile.ZipFile(wheel) as z:
        info = z.getinfo(f"{distribution}/_bin/{binname}")
        got = hashlib.sha256(z.read(info)).hexdigest()
        if got != want:
            fail(f"{wheel}: the binary's sha256 {got} isn't the archive's {want}")
        if (info.external_attr >> 16) & 0o777 != 0o755:
            fail(f"{wheel}: the binary's mode is {(info.external_attr >> 16) & 0o777:o}, not 755")
        if z.read(f"{distribution}/_bin/{binname}.sha256").decode() != f"{want}  {binname}\n":
            fail(f"{wheel}: {distribution}/_bin/{binname}.sha256 doesn't name the binary's sha256")
        for name, data in notices.items():
            if z.read(f"{dist}/licenses/{name}") != data:
                fail(f"{wheel}: {name} isn't the archive's")
        record = z.read(f"{dist}/RECORD").decode().splitlines()
        names = {i.filename for i in z.infolist()}
        listed = set()
        for line in record:
            name, h, size = line.rsplit(",", 2)
            listed.add(name)
            if name == f"{dist}/RECORD":
                continue
            data = z.read(name)
            if h != record_hash(data) or int(size) != len(data):
                fail(f"{wheel}: RECORD's entry for {name} doesn't match it")
        if listed != names:
            fail(f"{wheel}: RECORD and the wheel's files differ: {sorted(listed ^ names)}")
    print(f"{wheel}: binary sha256 {got} (the archive's), {len(notices)} licence files, RECORD checked")


if __name__ == "__main__":
    cmd, *args = sys.argv[1:] or [""]
    if cmd == "build" and len(args) == 6:
        build(*args)
    elif cmd == "verify" and len(args) == 5:
        verify(*args)
    elif cmd == "licence" and len(args) == 1 and args[0] in TARGETS:
        print(licence(TARGETS[args[0]][2]))
    else:
        sys.exit(__doc__)
