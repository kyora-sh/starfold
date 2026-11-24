#!/usr/bin/env bash
# npm-server-packages.sh <version> <artifacts dir> <out dir>
# The npm packages that carry the server binary, and the SDK that depends on them, packed into
# <out dir> under the names `npm pack` gives them:
#
#   @starfold/server-linux-x64-gnu, -linux-arm64-gnu, -darwin-arm64, -win32-x64
#     from the default (MIT) archives <artifacts dir>/starfold-<v>-<target>/starfold-<v>-<target>.<ext>,
#     each checked against its .sha256 first. A package holds bin/starfold[.exe], its .sha256,
#     and the archive's LICENSE, THIRD_PARTY.md and licenses/; it has no bin entry and no scripts.
#   starfold
#     packages/starfold, built (its dependencies installed: bun install --frozen-lockfile), with
#     optionalDependencies pinning the four at <version>; its package.json must say <version>.
#
# Every binary is read back out of its packed tarball and compared with the archive's.
set -euo pipefail
version=${1:?usage: npm-server-packages.sh <version> <artifacts dir> <out dir>}
artifacts=$(cd "${2:?artifacts dir}" && pwd)
mkdir -p "${3:?out dir}"
out=$(cd "$3" && pwd)
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
fail() {
	echo "::error::npm-server-packages.sh: $*"
	exit 1
}

python3 -I "$here/versions.py" check package-json-file "$root/packages/starfold/package.json" "$version"
# The default flavour is MIT (THIRD_PARTY.md, "What each release binary carries").
grep -q -F '**Default** (`starfold-<version>-<platform>`), distributed under MIT' "$root/THIRD_PARTY.md" ||
	fail "THIRD_PARTY.md no longer says the default binaries are distributed under MIT; update the packages' licence"
license=MIT

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# target  archive format  package suffix  os  cpu  libc  binary
targets=(
	"x86_64-linux tar.gz linux-x64-gnu linux x64 glibc starfold"
	"aarch64-linux tar.gz linux-arm64-gnu linux arm64 glibc starfold"
	"aarch64-macos tar.gz darwin-arm64 darwin arm64 - starfold"
	"x86_64-windows zip win32-x64 win32 x64 - starfold.exe"
)

# pack <dir>: npm pack into $out, printing the tarball's path.
pack() {
	local name
	name=$(cd "$out" && npm pack --json --pack-destination "$out" "$1" | python3 -I -c 'import json, sys; print(json.load(sys.stdin)[0]["filename"])')
	[ -f "$out/$name" ] || fail "npm pack made no $name"
	echo "$out/$name"
}

optional=()
for t in "${targets[@]}"; do
	read -r target format suffix os cpu libc binary <<<"$t"
	name=starfold-$version-$target
	archive=$artifacts/$name/$name.$format
	[ -f "$archive" ] || fail "no $archive"
	(cd "$(dirname "$archive")" && sha256sum -c --quiet "$name.$format.sha256") || fail "$archive doesn't match its .sha256"
	mkdir -p "$work/unpack/$target"
	case $format in
	tar.gz) tar -xzf "$archive" -C "$work/unpack/$target" ;;
	zip) unzip -q "$archive" -d "$work/unpack/$target" ;;
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
	python3 -I - "$dir" "$pkg" "$version" "$os" "$cpu" "$libc" "$license" "$target" <<'EOF'
import json, sys
dir, name, version, os_, cpu, libc, license, target = sys.argv[1:]
manifest = {
    "name": name,
    "version": version,
    "description": f"The starfold server binary for {target}, used by the starfold package",
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
        "[starfold](https://www.npmjs.com/package/starfold) package as an optional dependency.\n"
        "THIRD_PARTY.md and licenses/ say what the binary carries.\n"
    )
EOF
	tgz=$(pack "$dir")
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
done

# The SDK, built and staged with what its package.json "files" names, plus the pins.
sdk=$root/packages/starfold
[ -x "$sdk/node_modules/.bin/tsc" ] || [ -x "$root/node_modules/.bin/tsc" ] || fail "no tsc: run bun install --frozen-lockfile first"
(cd "$sdk" && bun run build)
stage=$work/sdk
mkdir -p "$stage"
cp -R "$sdk/package.json" "$sdk/README.md" "$sdk/LICENSE" "$sdk/dist" "$sdk/bin" "$stage/"
python3 -I - "$stage/package.json" "$version" "${optional[@]}" <<'EOF'
import json, sys
path, version, *packages = sys.argv[1:]
with open(path, encoding="utf-8") as f:
    manifest = json.load(f)
if "optionalDependencies" in manifest:
    sys.exit(f"{path} lists optionalDependencies already; the pack step writes them")
manifest["optionalDependencies"] = {p: version for p in packages}
manifest.pop("devDependencies", None)
with open(path, "w", encoding="utf-8") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")
EOF
tgz=$(pack "$stage")
python3 -I "$here/versions.py" check package-json "$tgz" "$version"
tar -xOzf "$tgz" package/package.json | python3 -I -c '
import json, sys
m = json.load(sys.stdin)
version, packages = sys.argv[1], sys.argv[2:]
want = {p: version for p in packages}
got = m.get("optionalDependencies")
if got != want:
    sys.exit(f"optionalDependencies are {got}, expected {want}")
' "$version" "${optional[@]}"
listing=$(tar -tzf "$tgz")
for f in package/dist/index.js package/dist/index.d.ts package/dist/native.js package/bin/starfold.js package/LICENSE package/README.md; do
	grep -q -x -F "$f" <<<"$listing" || fail "$tgz lacks $f"
done
echo "$(basename "$tgz") $(stat -c %s "$tgz") bytes"
