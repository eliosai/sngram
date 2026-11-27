# Releasing

A release is a pull request. `sngram`, `elgrep` and the two Python projects share one version,
and nothing but a merged pull request changes `main`.

## Cutting a release

1. Branch `release/vX.Y.Z` from `main` and run `git fetch --tags`; `just release` needs git-cliff.
2. Run `just release X.Y.Z`. It writes the version into `[workspace.package]`, the `sngram`
   dependency, both `pyproject.toml` files and their own entries in the `uv.lock` files, refreshes
   the workspace entries in `Cargo.lock` and writes `CHANGELOG.md` with git-cliff from `cliff.toml`.
3. Commit `chore: release vX.Y.Z` and open the pull request. Keep that commit last: the merge
   publishes and tags the commit which set the version.

On that pull request the `release` check refuses a branch which names another version, a version
which does not follow the last tag, and an API break the version may not carry against the last
tag. The `deep` checks run too: Miri, the careful build, the minimal versions and the links.

## What a merge does

`release.yml` runs on every push to `main` which touches `Cargo.toml`. When the manifest carries a
version no tag names, it publishes `sngram` and then `elgrep` through crates.io trusted
publishing, tags the commit which set the version and creates the GitHub release with the notes
git-cliff writes for that version. A rerun through `workflow_dispatch` skips a crate already on
crates.io, so it finishes a release that stopped half way. `just release-plan` prints what `main`
would publish now.

While `main` carries a version no tag names, every pull request fails `release`. Rerun
`release.yml` on `main` through `workflow_dispatch` before anything else merges.

## What the repository needs

- trusted publishing on crates.io for `sngram` and `elgrep`, naming the `eliosai/sngram`
  repository and the `release.yml` workflow, so the job holds no crates.io secret
- the organization rulesets: `main` takes rebase-merged pull requests which pass `gate`, and a
  `v*` tag never moves or goes away

## Pull requests

The `semver` check compares the `sngram` API against the base branch and fails a breaking change
unless the pull request carries the `semver-major` label. It runs `just semver-check`, so it asks
whether a minor release would be legal rather than whether the manifest version is. The `size`
check counts added lines outside `Cargo.lock`, the `uv.lock` files, `CHANGELOG.md`, `.agents/skills`, the `.tsv` fixtures
and `crates/lib/data`, with a budget of 1000, 3000 under the `mechanical` label, and none under
`size-exempt`. The `bench` workflow runs the library and Python benches on CodSpeed for every pull
request and every push to `main` which touches the code, and the walltime benches on a manual run.
