# Releasing

Every merge to `main` may release. `release.yml` runs on each push to `main`, skips its own
`chore(release)` commits, reruns `just check` and `just test-ci`, and hands the rest to
`scripts/release.sh`. `just release-plan` prints the version the next merge would release without
touching anything.

## Picking the version

The script reads the version from the workspace `Cargo.toml` and the last `v*` tag.

1. With no tag at all, it releases the `Cargo.toml` version as it stands, and takes the release
   notes from the commits since the commit that set the newest version on crates.io, so the first
   tagged release does not replay the whole history. That is how 1.0.0 ships over the 0.6.0 that is
   the last release of either crate.
2. With a tag that matches `Cargo.toml` that both crates have not published, it publishes that
   version again, which is how a run that failed after the push, or between the two crates, is
   retried through `workflow_dispatch`.
3. With a `Cargo.toml` version past the last tag, it releases that version, so a bump merged by
   hand wins.
4. Otherwise it reads the commits since the last tag. A subject with `!` or a public API break
   that cargo-semver-checks reports on `sngram` against the last tag bumps the major, a `feat` the
   minor, a `fix`, `perf` or `refactor` the patch, and anything else (`docs`, `chore`, `ci`, `test`)
   releases nothing. Both the script and the gate pass `--release-type minor`, because
   cargo-semver-checks reads the manifest version first and skips every check once that version
   already carries the bump, which would hide the break on exactly the pull requests that cause one. Before 1.0 a major bump moves the minor, the way cargo reads `0.x`.

## What one release does

The script writes the new version into the workspace `Cargo.toml`, the python `pyproject.toml` and
`__init__.py` and the trainer's `pyproject.toml`, refreshes `Cargo.lock` and both `uv.lock` files, prepends the release to
`CHANGELOG.md` with git-cliff from `cliff.toml`, commits `chore(release): vX.Y.Z` as
`github-actions[bot]`, tags `vX.Y.Z`, pushes both over the release deploy key, runs
`cargo publish --locked` for `sngram` and then `elgrep`, and creates the GitHub release with the
notes for that version. The Python package does not publish from this workflow.

## What the repository needs

- trusted publishing on crates.io for `sngram` and `elgrep`, naming the `eliosai/sngram`
  repository and the `release.yml` workflow, so the job mints a short-lived token through GitHub's
  OIDC and holds no crates.io secret
- the `RELEASE_DEPLOY_KEY` secret, the private half of the `release` deploy key with write access,
  which `actions/checkout` installs so the push comes from the deploy key
- the `main` ruleset, which requires a pull request with one approval and the `gate`, `msrv`,
  `semver` and `size` checks, and names the deploy key as its only bypass
- the `v*` tag ruleset, which lets only the deploy key create, move or delete a release tag

## Pull requests

The `semver` check compares the `sngram` API against the base branch and fails a breaking change
unless the pull request carries the `semver-major` label. It runs `just semver-check`, so it asks
whether a minor release would be legal rather than whether the manifest version is. The `size` check counts added lines
outside `Cargo.lock`, the `uv.lock` files, `CHANGELOG.md`, `.agents/skills`, the `.tsv` fixtures
and `crates/lib/data`, with a budget of 1000, 3000 under the `mechanical` label, and none under
`size-exempt`. The `bench` workflow runs the library and python benches on CodSpeed for every pull request and
push to `main`. Every other job runs a `just` recipe, and `just ci` runs all of them together.
