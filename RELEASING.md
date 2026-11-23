# Releasing

A pushed `v<version>` tag runs `.github/workflows/release.yml`: it builds everything, creates the
GitHub release and then publishes the default (MIT) build's packages:

| Registry  | Package                                          | Jobs             |
| --------- | ------------------------------------------------ | ---------------- |
| crates.io | `starfold-aom-sys`, then `starfold` (the server) | `crates`         |
| npm       | `@starfold/web-embed` (the embeddable client)    | `embed`, `npm`   |
| PyPI      | `starfold` (the Python SDK)                      | `python`, `pypi` |

The x264 (GPL-2.0-or-later) flavour is a GitHub release download only. Each registry job uses
trusted publishing: the registry trusts this repository's `release.yml` and swaps the job's GitHub
OIDC token for a short-lived one, so no registry token is stored in the repository. The publishing
jobs run on GitHub-hosted runners (`ubuntu-latest`; `ubuntu-24.04` for crates.io, whose packages
the job installs by name), which npm requires for trusted publishing; this
repository is public, so those minutes cost nothing.

A version a registry has already is skipped rather than published again, so a job can be re-run,
and `starfold-aom-sys` keeps its version until libaom or its bindings change.

## Before the first release (once)

crates.io and npm only let a package that exists trust a workflow, so a maintainer publishes the
first version of those by hand; PyPI creates its project from the workflow. Every form takes the
same values:

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
   reviewers, to approve every publish. If you skip this step, GitHub creates `release` the first
   time a tag runs a publish job, with no rules: publishing works, but the protection doesn't
   exist until you add the rules above, and the registries' publishers must still name `release`.
1. **crates.io, first upload.** Sign in at crates.io, create an API token under Account Settings
   → API Tokens (scopes `publish-new` and `publish-update`), and run `cargo login` with it. From a
   clean checkout with its submodule (`git submodule update --init --force`), Rust 1.90 or newer
   (it packages both crates in one command) and the build tools from the README's Build section,
   on Linux:

   ```sh
   .github/release/crates.sh dry-run && .github/release/crates.sh publish
   ```

   This stages the web client into `crates/starfold-server/web`, packages both crates, builds each
   from its package alone, and uploads `starfold-aom-sys` before `starfold`.

2. **crates.io, trusted publishing.** For each of the two crates: crates.io → the crate → Settings
   → Trusted Publishing → Add → GitHub, enter the values above, save. Then revoke the API token.
3. **npm, the scope.** On npmjs.com, create the organisation `starfold` on the free plan (Unlimited
   public packages); its name is the `@starfold` scope.
4. **npm, first upload.** Run the Release workflow by hand at that commit (Actions → Release → Run
   workflow), download its `starfold-web-embed-<version>` artifact (a `.zip`: unzip it for the
   `.tgz`), then:

   ```sh
   npm login
   npm publish ./starfold-web-embed-<version>.tgz --access public
   npm logout
   ```

5. **PyPI.** pypi.org → Your account → Publishing → Add a new pending publisher → GitHub, enter the
   values above and the project name `starfold`, then Add. Nothing is uploaded by hand: the first
   tag creates the project. A pending publisher reserves nothing: if someone registers `starfold`
   first, it is no longer valid.
6. Push the first tag (see [Each release](#each-release)). crates.io and npm have that version
   already and are skipped; PyPI's project is created.

### npm's trusted publisher, before the second release

npm expires a trusted publisher that hasn't published within **two days** of its creation, so it
is added only when the second release is ready:

1. Merge the second release's version bump (see [Each release](#each-release)).
2. npmjs.com → `@starfold/web-embed` → Settings → Trusted Publisher → GitHub Actions; enter the
   values above.
3. Under the allowed actions, tick **npm publish** (a new configuration allows only
   `npm stage publish` until you do; the workflow runs `npm publish`), and save.
4. Push the release tag within two days. If the two days pass first, delete the configuration and
   repeat steps 2 and 3.

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
