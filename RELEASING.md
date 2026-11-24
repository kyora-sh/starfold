# Releasing

A pushed `v<version>` tag runs `.github/workflows/release.yml`: it builds everything, creates the
GitHub release and then publishes the default (MIT) build's packages:

| Registry  | Package                                          | Jobs                             |
| --------- | ------------------------------------------------ | -------------------------------- |
| crates.io | `starfold-aom-sys`, then `starfold` (the server) | `crates`, `crates-publish`       |
| npm       | `@starfold/web` (the embeddable client)          | `embed`, `npm`, `npm-publish`    |
| PyPI      | `starfold` (the Python SDK)                      | `python`, `pypi`, `pypi-publish` |

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

## Before the first release (once)

crates.io and npm only let a package that exists trust a workflow. On crates.io the first tag
publishes with a bootstrap token, and trusted publishing is set up afterwards; on npm a `0.0.1`
placeholder of `@starfold/web` exists for this, and the first tag publishes the real version. PyPI
creates its project from the workflow. Every form takes the same values:

- owner: `kyora-sh`
- repository: `starfold`
- workflow: `release.yml`
- environment: `release`

In this order, at the commit the first tag will name:

0. **The `release` environment.** The registries check the repository, the workflow file and the
   environment, not the tag: without a protected environment, anyone who can push a branch here
   could change `release.yml` on that branch so that it publishes. The three publish jobs declare
   the environment `release`; protect it before the first tag. Repository Settings → Environments
   → New environment, name `release`; under Deployment branches and tags choose "Selected branches
   and tags" and add the tag rule `v*` (and no branch); optionally add yourself under Required
   reviewers, to approve every publish. The tag rule only helps while pushing a `v*` tag is
   reserved to you: if others can push to this repository, also add a tag ruleset (Settings →
   Rules → Rulesets, target tags `v*`, restrict creations) or a required reviewer. The environment
   also holds crates.io's bootstrap token (step 1), which its tag rule keeps from any branch run.
1. **crates.io, the bootstrap token.** Sign in at crates.io and create an API token under Account
   Settings → API Tokens with the scopes `publish-new` and `publish-update`, limited to the crates
   `starfold` and `starfold-aom-sys`. Add it as a secret of the `release` environment (Settings →
   Environments → `release` → Environment secrets) named `CRATES_IO_BOOTSTRAP_TOKEN`, not as a
   repository secret. Only the `crates-publish` job names it, and as an environment secret only a
   job that declares `release`, on a `v*` tag, can read it. While it is set, `crates-publish`
   uses it (its log says "crates.io: bootstrap token"); without it, trusted publishing ("crates.io:
   trusted publishing").
2. **npm, the scope and the placeholder.** The organisation `starfold` (its name is the
   `@starfold` scope) and a `0.0.1` placeholder of `@starfold/web` exist already; the release
   versions come from the workflow, and the real tarball is never uploaded by hand.
3. **PyPI.** pypi.org → Your account → Publishing → Add a new pending publisher → GitHub, enter the
   values above and the project name `starfold`, then Add. Nothing is uploaded by hand: the first
   tag creates the project. A pending publisher reserves nothing: if someone registers `starfold`
   first, it is no longer valid.
4. **npm, trusted publishing**, when the first tag is ready, since npm expires a trusted publisher
   that hasn't published within **two days** of its creation:
   1. Merge the first release's version (see [Each release](#each-release)).
   2. npmjs.com → `@starfold/web` → Settings → Trusted Publisher → GitHub Actions; enter the
      values above.
   3. Under the allowed actions, tick **npm publish** (a new configuration allows only
      `npm stage publish` until you do; the workflow runs `npm publish`), and save.
   4. Push the tag (step 5) within two days. If the two days pass first, delete the configuration
      and repeat 2 and 3.
5. Push the first tag (see [Each release](#each-release)). crates.io gets both crates through the
   bootstrap token, `starfold-aom-sys` first; npm publishes the release beside the placeholder;
   PyPI's project is created.
6. **crates.io, after the first release.** For each of the two crates: crates.io → the crate →
   Settings → Trusted Publishing → Add → GitHub, enter the values above, save. Then delete the
   environment secret `CRATES_IO_BOOTSTRAP_TOKEN` and revoke the token on crates.io; from the
   next release on, `crates-publish` uses trusted publishing.
7. **npm, after the first release.** Deprecate the placeholder so that it points at the release:

   ```sh
   npm deprecate @starfold/web@0.0.1 "a placeholder; install @starfold/web@latest"
   ```

## Each release

1. On main, set the same `version` in `crates/starfold-server/Cargo.toml` and
   `python/starfold/pyproject.toml`. Bump `crates/starfold-aom-sys/Cargo.toml` (and the `=`
   version `crates/starfold-server/Cargo.toml` asks of it) only when it changed. Refresh the
   lock files the builds read with `--locked`: `cargo update --workspace` and, in
   `python/starfold`, `uv lock`. Merge.
2. Optional: run the Release workflow by hand for a dry run of everything, the registry jobs'
   dry runs included.
3. Tag main's head and push the tag:

   ```sh
   git tag v<version> && git push origin v<version>
   ```

The workflow checks that the tag names the version, builds, creates the GitHub release, then
publishes to crates.io, npm and PyPI. Each registry has a dry-run job (`crates`, `npm`, `pypi`)
and a publish job after it (`crates-publish`, `npm-publish`, `pypi-publish`), the only jobs that
can obtain a registry token. If a publish job fails after the release exists, re-run it from the
run's page.
