#!/usr/bin/env bash
# npm-server-packages.sh <version> <artifacts dir> <out dir>
# The npm packages that carry the server binary, the server package that depends on them and the
# client, packed into <out dir> under the names `npm pack` gives them:
#
#   @starfold/server-linux-x64-gnu, -linux-arm64-gnu, -darwin-arm64, -win32-x64
#     from the default flavour's archives <artifacts dir>/starfold-<v>-<target>/starfold-<v>-<target>.<ext>
#     (targets, names and archives from expected-files.sh native), each checked against its .sha256
#     first. A package holds bin/starfold[.exe], its .sha256,
#     and the archive's LICENSE, THIRD_PARTY.md and licenses/; it has no bin entry and no scripts.
#     Its "license" is the SPDX expression of what the binary carries on that platform, the same
#     one its platform wheel states (python-platform-wheel.py licence <target>).
#   starfold (starfold-<version>.tgz)
#     packages/starfold, the client, built (the workspace installed: bun install --frozen-lockfile);
#     no dependencies, no bin, no optionalDependencies.
#   @starfold/server (starfold-server-<version>.tgz, which starfold-server-*-<version>.tgz doesn't match)
#     packages/server, built, with optionalDependencies pinning the four at <version>, the
#     dependency starfold at exactly <version> and the bin starfold.
# Both package.json files must say <version>.
#
# Every tarball gets a .sha256 beside it, and every binary is read back out of its packed tarball
# and compared with the archive's.
set -euo pipefail
version=${1:?usage: npm-server-packages.sh <version> <artifacts dir> <out dir>}
artifacts=$(cd "${2:?artifacts dir}" && pwd)
mkdir -p "${3:?out dir}"
out=$(cd "$3" && pwd)
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
fail() {
	echo "::error::npm-server-packages.sh: $*" >&2
	exit 1
}

python3 -I "$here/versions.py" check package-json-file "$root/packages/starfold/package.json" "$version"
python3 -I "$here/versions.py" check package-json-file "$root/packages/server/package.json" "$version"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# pack <dir> <expected name>: npm pack into $out, printing the tarball's path; the name has to be the
# one expected-files.sh lists.
pack() {
	local name
	name=$(cd "$out" && npm pack --json --pack-destination "$out" "$1" | python3 -I -c 'import json, sys; print(json.load(sys.stdin)[0]["filename"])')
	[ "$name" = "$2" ] || fail "npm pack made $name, expected-files.sh lists $2"
	[ -f "$out/$name" ] || fail "npm pack made no $name"
	(cd "$out" && sha256sum "$name" >"$name.sha256")
	echo "$out/$name"
}

# The targets, packages and archives from expected-files.sh; os, cpu and libc from the package's name.
optional=()
while read -r target suffix archive tgz _ <&3; do
	IFS=- read -r os cpu libc <<<"$suffix"
	case $libc in gnu) libc=glibc ;; '') libc=- ;; *) fail "no libc for $suffix" ;; esac
	case $os in win32) binary=starfold.exe ;; *) binary=starfold ;; esac
	name=$(basename "$(dirname "$archive")")
	format=${archive##*/"$name".}
	archive=$artifacts/$archive
	[ -f "$archive" ] || fail "no $archive"
	(cd "$(dirname "$archive")" && sha256sum -c --quiet "$name.$format.sha256") || fail "$archive doesn't match its .sha256"
	mkdir -p "$work/unpack/$target"
	case $format in
	tar.gz) tar -xzf "$archive" -C "$work/unpack/$target" ;;
	zip) python3 -I -m zipfile -e "$archive" "$work/unpack/$target" ;;
	*) fail "$archive: unknown format $format" ;;
	esac
	src=$work/unpack/$target/$name
	[ -f "$src/$binary" ] || fail "$archive has no $name/$binary"
	pkg=@starfold/server-$suffix
	dir=$work/pkg/server-$suffix
	mkdir -p "$dir/bin"
	cp "$src/$binary" "$dir/bin/$binary"
	chmod 0755 "$dir/bin/$binary"
	(cd "$dir/bin" && sha256sum "$binary" >"$binary.sha256")
	cp -R "$src/LICENSE" "$src/THIRD_PARTY.md" "$src/licenses" "$dir/"
	license=$(python3 -I "$here/python-platform-wheel.py" licence "$target")
	python3 -I - "$dir" "$pkg" "$version" "$os" "$cpu" "$libc" "$license" "$target" <<'EOF'
import json, sys
dir, name, version, os_, cpu, libc, license, target = sys.argv[1:]
manifest = {
    "name": name,
    "version": version,
    "description": f"The starfold server binary for {target}, used by the @starfold/server package",
    "license": license,
    "repository": {"type": "git", "url": "git+https://github.com/kyora-sh/starfold.git"},
    "os": [os_],
    "cpu": [cpu],
}
if libc != "-":
    manifest["libc"] = [libc]
manifest["files"] = ["bin", "LICENSE", "THIRD_PARTY.md", "licenses"]
with open(f"{dir}/package.json", "w", encoding="utf-8") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")
with open(f"{dir}/README.md", "w", encoding="utf-8") as f:
    f.write(
        f"# {name}\n\nThe starfold server binary for {target} (`bin/`), installed by the\n"
        "[@starfold/server](https://www.npmjs.com/package/@starfold/server) package as an optional dependency.\n"
        "THIRD_PARTY.md and licenses/ say what the binary carries.\n"
    )
EOF
	tgz=$(pack "$dir" "$(basename "$tgz")")
	python3 -I "$here/versions.py" check package-json "$tgz" "$version"
	want=$(sha256sum <"$src/$binary" | cut -d' ' -f1)
	got=$(tar -xOzf "$tgz" "package/bin/$binary" | sha256sum | cut -d' ' -f1)
	[ "$want" = "$got" ] || fail "$tgz: bin/$binary is $got, the archive's is $want"
	listed=$(tar -xOzf "$tgz" "package/bin/$binary.sha256" | cut -d' ' -f1)
	[ "$listed" = "$want" ] || fail "$tgz: bin/$binary.sha256 says $listed, the binary is $want"
	mode=$(tar -tvzf "$tgz" "package/bin/$binary" | cut -c1-10)
	[ "$mode" = "-rwxr-xr-x" ] || fail "$tgz: bin/$binary has mode $mode"
	listing=$(tar -tzf "$tgz")
	for f in package/LICENSE package/THIRD_PARTY.md package/licenses/rust-crates.txt; do
		grep -q -x -F "$f" <<<"$listing" || fail "$tgz lacks $f"
	done
	tar -xOzf "$tgz" package/package.json | python3 -I -c 'import json, sys; sys.exit(bool({"bin", "scripts"} & set(json.load(sys.stdin))))' ||
		fail "$tgz: package.json has a bin or scripts entry"
	echo "$(basename "$tgz") $(stat -c %s "$tgz") bytes; bin/$binary sha256 $got"
	optional+=("$pkg")
done 3< <("$here/expected-files.sh" native "$version")
[ ${#optional[@]} = 4 ] || fail "expected-files.sh native named ${#optional[@]} packages, not 4"

# stage <package dir> <what to copy...>: the built package staged with what its package.json "files"
# names, without devDependencies; optionalDependencies, if given after --, pinned at <version>.
stage() {
	local src=$1 dir
	shift
	dir=$work/stage/$(basename "$src")
	mkdir -p "$dir"
	cmp "$src/LICENSE" "$root/LICENSE" || fail "${src#"$root/"}/LICENSE isn't the repository's LICENSE"
	while [ $# -gt 0 ] && [ "$1" != -- ]; do
		cp -R "$src/$1" "$dir/"
		shift
	done
	[ $# -gt 0 ] && shift
	python3 -I - "$dir/package.json" "$version" "$@" <<'EOF'
import json, sys
path, version, *packages = sys.argv[1:]
with open(path, encoding="utf-8") as f:
    manifest = json.load(f)
if "optionalDependencies" in manifest:
    sys.exit(f"{path} lists optionalDependencies already; the pack step writes them")
if packages:
    manifest["optionalDependencies"] = {p: version for p in packages}
manifest.pop("devDependencies", None)
with open(path, "w", encoding="utf-8") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")
EOF
	echo "$dir"
}

# manifest <tgz> <version> <bin JSON> <dependencies JSON> <optional package...>: the packed
# package.json has exactly that bin, those dependencies and the optional packages at <version>
# (null for none of each).
manifest() {
	tar -xOzf "$1" package/package.json | python3 -I -c '
import json, sys
m = json.load(sys.stdin)
tgz, version, bin, deps, packages = sys.argv[1], sys.argv[2], json.loads(sys.argv[3]), json.loads(sys.argv[4]), sys.argv[5:]
optional = {p: version for p in packages} or None
for key, want in (("bin", bin), ("dependencies", deps), ("optionalDependencies", optional)):
    if m.get(key) != want:
        sys.exit(f"{tgz}: {key} is {m.get(key)}, expected {want}")
for key in ("devDependencies", "peerDependencies", "bundleDependencies", "bundledDependencies"):
    if key in m:
        sys.exit(f"{tgz}: has {key}")
' "$1" "$2" "$3" "$4" "${@:5}" || fail "$1: package.json isn't as expected"
}

# lists <tgz> <must> -- <must not>: files the tarball has, and has not.
lists() {
	local tgz=$1 listing f
	shift
	listing=$(tar -tzf "$tgz")
	while [ $# -gt 0 ] && [ "$1" != -- ]; do
		grep -q -x -F "package/$1" <<<"$listing" || fail "$tgz lacks $1"
		shift
	done
	[ $# -gt 0 ] && shift
	for f in "$@"; do
		! grep -q -x -F "package/$f" <<<"$listing" || fail "$tgz has $f"
	done
}

client=$root/packages/starfold
server=$root/packages/server
for p in "$client" "$server"; do
	[ -x "$p/node_modules/.bin/tsc" ] || [ -x "$root/node_modules/.bin/tsc" ] || fail "no tsc for ${p#"$root/"}: run bun install --frozen-lockfile first"
done
(cd "$server" && bun run build)

# The client: no dependencies, no command, no server package.
dir=$(stage "$client" package.json README.md LICENSE dist)
tgz=$(pack "$dir" "starfold-$version.tgz")
python3 -I "$here/versions.py" check package-json "$tgz" "$version"
manifest "$tgz" "$version" null null
lists "$tgz" dist/index.js dist/index.d.ts LICENSE README.md -- dist/native.js bin/starfold.js
echo "$(basename "$tgz") $(stat -c %s "$tgz") bytes"

# The server: the command, the client at exactly this version and the four platform packages.
dir=$(stage "$server" package.json README.md LICENSE dist bin -- "${optional[@]}")
tgz=$(pack "$dir" "starfold-server-$version.tgz")
case $(basename "$tgz") in "starfold-server-"*"-$version.tgz") fail "$tgz would be taken for a platform package" ;; esac
python3 -I "$here/versions.py" check package-json "$tgz" "$version"
manifest "$tgz" "$version" '{"starfold": "bin/starfold.js"}' "{\"starfold\": \"$version\"}" "${optional[@]}"
lists "$tgz" dist/index.js dist/index.d.ts dist/native.js bin/starfold.js LICENSE README.md -- dist/client.js
echo "$(basename "$tgz") $(stat -c %s "$tgz") bytes"
