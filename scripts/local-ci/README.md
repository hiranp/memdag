# Local CI

`scripts/local-ci/ci.sh` runs the steps of `.github/workflows/ci.yml` (fmt,
clippy, nextest, doctests) natively, so you can check a change
before you push. It can run on this machine or on another one over SSH.

## Run it here

```bash
scripts/local-ci/ci.sh            # everything CI runs
scripts/local-ci/ci.sh --quick    # fmt + clippy only
scripts/local-ci/ci.sh --release  # also the release build
```

## Run it on your Mac

```bash
scripts/local-ci/ci.sh --remote my-mac            # host from ~/.ssh/config
MEMDAG_CI_REMOTE=my-mac scripts/local-ci/ci.sh      # same, from the environment
scripts/local-ci/ci.sh --remote my-mac --release  # check the macOS release build
```

This sends exactly the files git sees (tracked files, plus untracked files that
aren't ignored, including uncommitted edits) to `~/.cache/memdag-ci/src` on the
remote, then runs the same script there in a login shell.

The remote keeps its own `target/`, so later runs are incremental. A file
whose contents haven't changed keeps its old mtime there, so cargo rebuilds
only what changed.

That gives you native macOS coverage, both a ship target and the most
expensive runner on GitHub, without using Actions minutes.

## What the Mac needs (once)

```bash
xcode-select --install          # compiler and linker
curl https://sh.rustup.rs -sSf | sh
brew install cargo-nextest
```

If nextest is missing, the script falls back to `cargo test` and says so. On Linux the
heavy steps run through `scripts/lowload.sh`, so a local run doesn't crowd out the machine.

## Pre-push hook

```bash
scripts/local-ci/ci.sh --install-hook   # fmt + clippy before every git push
git push --no-verify                    # skip it once
```

## Limits

- Release builds for other targets (Windows, Linux on a Mac) are not
  cross-compiled here. The release workflow still builds those.
- The Linux CI runner is x86_64 and an Apple Silicon Mac is arm64. A rare
  arch-specific failure would only show up on one of them.
