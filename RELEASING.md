# Releasing

A pushed `v<version>` tag runs `.github/workflows/release.yml`: it builds everything, creates the
GitHub release and then publishes the default (MIT) build's packages, all at the same version:

| Registry  | Package                                                                                                                                                                   | Jobs                                              |
| --------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------- |
| crates.io | `starfold-aom-sys`, then `starfold` (the server)                                                                                                                          | `crates`, `crates-publish`                        |
| npm       | the four `@starfold/server-*` (the server binary per platform), then `starfold` (the TypeScript client), then `@starfold/server` (the server package, `starfold` command) | `native`, `npm`, `npm-publish`                    |
| npm       | `@starfold/web` (the embeddable client)                                                                                                                                   | `embed`, `npm`, `npm-publish`                     |
| PyPI      | `starfold` (the Python client): the sdist and the pure wheel                                                                                                              | `python`, `pypi`, `pypi-publish`                  |
| PyPI      | `starfold-server` (the server package, `starfold` command): four platform wheels with the binary, no sdist and no pure wheel                                              | `python`, `native`, `pypi`, `pypi-server-publish` |

The four binary packages are `@starfold/server-linux-x64-gnu`, `-linux-arm64-gnu`,
`-darwin-arm64` and `-win32-x64`.

The npm binary packages and `starfold-server`'s platform wheels carry the default build's binary
from the release's own archives, byte for byte (`native` compares each one's sha256 with its
archive's); `@starfold/server` on npm and `starfold-server` on PyPI pick the one for their
platform. The server packages pin the client exactly: `@starfold/server` depends on
`starfold@<version>`, `starfold-server` requires `starfold==<version>`. The clients carry no
binary and no `starfold` command. `starfold-server`'s pure wheel, which its platform wheels are
made from, is attached to the GitHub release but never published.

The x264 (GPL-2.0-or-later) flavour is a GitHub release download only. Each registry job uses
trusted publishing: the registry trusts this repository's `release.yml` and swaps the job's GitHub
OIDC token for a short-lived one. The one stored token is crates.io's bootstrap token for the first
release (below), an environment secret that is deleted once that release is out. The publishing
jobs run on GitHub-hosted runners (`ubuntu-latest`; `ubuntu-24.04` for crates.io, whose packages
the job installs by name), which npm requires for trusted publishing; this
repository is public, so those minutes cost nothing.

A version a registry has already is skipped rather than published again, so a job can be re-run,
and `starfold-aom-sys` keeps its version until libaom or its bindings change. npm's job reads the
package's version list from the registry first and fails if the registry doesn't give one.
`pypi-server-publish` runs after `pypi-publish` and waits until PyPI lists `starfold` at the
version, so `starfold-server` never appears without the client it requires.

## Before the first release (once)

crates.io and npm only let a package that exists trust a workflow. On crates.io the first tag
publishes with a bootstrap token, and trusted publishing is set up afterwards; on npm each of the
six packages (`starfold`, `@starfold/web` and the four `@starfold/server-*`) has a `0.0.1`
placeholder (and a `0.0.0-stage`) for this, and the first tag publishes the real version. PyPI
creates its project from the workflow. Every form takes the same values, with the environment of
its registry:

- owner: `kyora-sh`
- repository: `starfold`
- workflow: `release.yml`
- environment: `crates` (crates.io), `npm` (npm) or `pypi` (PyPI)

In this order:

0. **The environments `crates`, `npm` and `pypi`** (they exist, each limited to tags matching
   `v*`). The registries check the repository, the workflow file and the environment, not the tag:
   without a protected environment, anyone who can push a branch here could change `release.yml`
   on that branch so that it publishes. Each publish job declares its registry's environment
   (`crates-publish` → `crates`, `npm-publish` → `npm`, `pypi-publish` and `pypi-server-publish` →
   `pypi`), and a job reads only its own environment's secrets. Under Repository Settings →
   Environments, each of the three has "Selected branches and tags" with the tag rule `v*` (and no
   branch); optionally add yourself under Required reviewers on each, to approve every publish.
   The tag rule only helps while pushing a `v*` tag is reserved to you: if others can push to this
   repository, also add a tag ruleset (Settings → Rules → Rulesets, target tags `v*`, restrict
   creations) or a required reviewer.
1. **crates.io, the bootstrap token.** Sign in at crates.io and create an API token under Account
   Settings → API Tokens with the scopes `publish-new` and `publish-update`, limited to the crates
   `starfold` and `starfold-aom-sys`. Add it as a secret of the `crates` environment (Settings →
   Environments → `crates` → Environment secrets) named `CRATES_IO_BOOTSTRAP_TOKEN`, not as a
   repository secret. Only the `crates-publish` job names it, and as a secret of `crates` only a
   job that declares `crates`, on a `v*` tag, can read it. While it is set, `crates-publish`
   uses it (its log says "crates.io: bootstrap token"); without it, trusted publishing ("crates.io:
   trusted publishing").
2. **npm, the scope and the placeholders.** The organisation `starfold` (its name is the
   `@starfold` scope) and the six placeholders exist already; the release versions come from the
   workflow, and no real tarball is ever uploaded by hand.
3. **PyPI.** pypi.org → Your account → Publishing → Add a new pending publisher → GitHub, enter the
   values above with environment `pypi` and the project name `starfold`, then Add. Nothing is
   uploaded by hand: the first tag creates the project. A pending publisher reserves nothing: if
   someone registers `starfold` first, it is no longer valid.
4. **npm, trusted publishing**, when the first tag is ready, since npm expires a trusted publisher
   that hasn't published within **two days** of its creation:
   1. Merge the first release's version (see [Each release](#each-release)).
   2. For each of the six packages: npmjs.com → the package → Settings → Trusted Publisher →
      GitHub Actions; enter the values above, environment `npm`.
   3. Under the allowed actions, tick **npm publish** (a new configuration allows only
      `npm stage publish` until you do; the workflow runs `npm publish`), and save. Instead of 2
      and 3, npm 11.15.0 or newer does both for all six from a shell signed in to npm with
      two-factor authentication:

      ```sh
      for p in starfold @starfold/web @starfold/server-linux-x64-gnu @starfold/server-linux-arm64-gnu \
        @starfold/server-darwin-arm64 @starfold/server-win32-x64; do
        npm trust github "$p" --file release.yml --repo kyora-sh/starfold --env npm --allow-publish --yes
      done
      ```

   4. Push the tag (step 5) within two days. If the two days pass first, delete the configurations
      and repeat 2 and 3 (the loop redoes them all).
5. Push the first tag (see [Each release](#each-release)). crates.io gets both crates through the
   bootstrap token, `starfold-aom-sys` first; npm publishes the four server packages, then
   `starfold` (only once all four are there), then `@starfold/web`, each beside its placeholder,
   and `latest` moves to the release; PyPI's project is created with all six files.
6. **crates.io, after the first release.** For each of the two crates: crates.io → the crate →
   Settings → Trusted Publishing → Add → GitHub, enter the values above with environment `crates`,
   save. Then delete the `crates` environment's secret `CRATES_IO_BOOTSTRAP_TOKEN` and revoke the
   token on crates.io; from the next release on, `crates-publish` uses trusted publishing.
7. **npm, after the first release.** Deprecate every placeholder so that it points at the
   release, from a shell signed in with `npm login`. A range takes both `0.0.0-stage` and `0.0.1`
   (`npm deprecate` matches prereleases too):

   ```sh
   for p in starfold @starfold/web @starfold/server-linux-x64-gnu @starfold/server-linux-arm64-gnu \
     @starfold/server-darwin-arm64 @starfold/server-win32-x64; do
     npm deprecate "$p@<0.3.2" "a placeholder; install $p@latest"
   done
   ```

## Before the first release of a new package

The same steps, for that package alone. For 0.4.0, which split each SDK into a client and a server
package, this was done for `@starfold/server` (npm) and `starfold-server` (PyPI):

1. **npm, the placeholder.** `@starfold/server` exists as a `0.0.0-stage` placeholder, so that it
   can trust a workflow. Nothing else is uploaded by hand.
2. **npm, trusted publishing**, within two days before the tag (as in step 4 above), from a shell
   signed in to npm with two-factor authentication and npm 11.15.0 or newer:

   ```sh
   npm trust github @starfold/server --file release.yml --repo kyora-sh/starfold --env npm --allow-publish --yes
   ```

3. **PyPI, the pending publisher.** pypi.org → Your account → Publishing → Add a new pending
   publisher → GitHub, the values above with environment `pypi` and the project name
   `starfold-server`. The first tag's `pypi-server-publish` creates the project. As before, a
   pending publisher reserves nothing.
4. **npm, after the release.** Deprecate the placeholder, so that it points at the release:

   ```sh
   npm deprecate "@starfold/server@<0.4.0" "a placeholder; install @starfold/server@latest"
   ```

`starfold@0.3.2` on npm and on PyPI keeps the server (the `starfold` command and `startServer()` /
`start_server()`); it is not deprecated, and staying on it changes nothing. From 0.4.0 on the
server is in `@starfold/server` and `starfold-server` (CHANGELOG.md has the table of what moved).

## Each release

1. On main, set the same `version` in `crates/starfold-server/Cargo.toml`,
   `python/starfold/pyproject.toml`, `python/starfold-server/pyproject.toml`,
   `packages/starfold/package.json` and `packages/server/package.json`, and the server packages'
   exact pin on the client to it: `"starfold==<version>"` in
   `python/starfold-server/pyproject.toml`'s `dependencies` and `"starfold": "<version>"` in
   `packages/server/package.json`'s `dependencies` (the release's `version` job fails if one
   differs), and `default.nix`'s `version`. Bump `crates/starfold-aom-sys/Cargo.toml` (and the `=` version
   `crates/starfold-server/Cargo.toml` asks of it) only when it changed. Refresh the lock files
   the builds read with `--locked` or `--frozen-lockfile`: `cargo update --workspace` (`Cargo.lock`);
   `uv lock` in each Python project (`python/starfold/uv.lock`, `python/starfold-server/uv.lock`);
   and `bun.lock`, which records the npm packages' versions, with `bun install` at the repository
   root, using bun 1.4.4 (CI's). Merge.
2. Optional: run the Release workflow by hand for a dry run of everything, the registry jobs'
   dry runs included.
3. Tag main's head and push the tag:

   ```sh
   git tag v<version> && git push origin v<version>
   ```

The workflow checks that the tag names the version, builds, creates the GitHub release, then
publishes to crates.io, npm and PyPI. Each registry has a dry-run job (`crates`, `npm`, `pypi`)
and publish jobs after it (`crates-publish`, `npm-publish`, `pypi-publish` then
`pypi-server-publish`), the only jobs that can obtain a registry token. If a job fails, use
**Re-run failed jobs** on the tag's run: it publishes what the run built, and what a registry
already has is skipped. Never start a new run or push the tag again for this, since that builds
the binaries again.
