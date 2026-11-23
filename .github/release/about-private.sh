#!/usr/bin/env bash
# about-private.sh [cargo metadata args...]: fails if cargo-about would leave a dependency out of
# the licence list. about.toml's `private = { ignore = true, registries = ["crates-io"] }` leaves
# out starfold's own crates (both declare `publish = ["crates-io"]`), but cargo-about applies it
# to every crate in the graph: a dependency that also declared a `publish` list of crates.io only
# would vanish from licenses/rust-crates.txt and skip the licence check. Run before
# `cargo about generate`, with the same target and features.
set -euo pipefail
cargo metadata --locked --format-version 1 "$@" | python3 -I -c '
import json, sys
m = json.load(sys.stdin)
ours = set(m["workspace_members"])
hidden = [p["id"] for p in m["packages"]
          if p["id"] not in ours and p["publish"] is not None and all(r == "crates-io" for r in p["publish"])]
if hidden:
    sys.exit("::error::cargo-about would leave these dependencies out of the licence list "
             "(their publish lists only crates-io; see .github/release/about.toml): " + ", ".join(hidden))
n = len(m["packages"]) - len(ours)
print(f"cargo-about: {n} dependencies, none hidden by about.toml")
'
