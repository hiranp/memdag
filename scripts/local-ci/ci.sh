#!/usr/bin/env bash
# Local CI: the steps of .github/workflows/ci.yml, run natively on this
# machine or on a remote one over SSH, so a push needs no Actions minutes.
#
#   scripts/local-ci/ci.sh                  run here (macOS or Linux)
#   scripts/local-ci/ci.sh --remote <host>  sync the tree to <host> and run there
#                                           (e.g. your Mac: native macOS coverage
#                                           without paying for macOS runners)
#   scripts/local-ci/ci.sh --release        also build the release profile
#   scripts/local-ci/ci.sh --quick          fmt + clippy only
#   scripts/local-ci/ci.sh --install-hook   run before every git push
#
# --remote also reads MEMDAG_CI_REMOTE. The remote path defaults to
# ~/.cache/memdag-ci/src (override with MEMDAG_CI_REMOTE_DIR). The remote keeps its
# own target/, so later runs are incremental.
#
# Skip the hook once with: git push --no-verify
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
remote="${MEMDAG_CI_REMOTE:-}"
remote_dir="${MEMDAG_CI_REMOTE_DIR:-.cache/memdag-ci/src}"
quick=0
release=0
action=run

die() { echo "local-ci: $*" >&2; exit 2; }

while (($#)); do
  case "$1" in
    --remote) (($# >= 2)) || die "--remote needs a host"; remote="$2"; shift ;;
    --here) remote="" ;;
    --quick) quick=1 ;;
    --release) release=1 ;;
    --install-hook) action=hook ;;
    -h|--help) sed -n '2,19p' "$0"; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
  shift
done

# Same settings as ci.yml's env block, so a local pass means a CI pass.
export CARGO_TERM_COLOR=always
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=line-tables-only
export CARGO_PROFILE_TEST_DEBUG=line-tables-only

# On Linux, heavy cargo steps go through scripts/lowload.sh (lowest priority, bounded jobs,
# one heavy build at a time) so a local run doesn't crowd out everything else on the machine.
heavy() {
  if [[ "$(uname -s)" == Linux && -x "$ROOT/scripts/lowload.sh" ]]; then
    "$ROOT/scripts/lowload.sh" "$@"
  else
    cargo "$@"
  fi
}

step() { printf '\n\033[1m== %s\033[0m\n' "$1"; }

need() {
  command -v "$1" >/dev/null 2>&1 && return 0
  case "$(uname -s)" in
    Darwin) die "$1 not found. Install it: brew install $2" ;;
    *) die "$1 not found. Install it: $3" ;;
  esac
}

run_here() {
  cd "$ROOT"
  need cargo rustup "https://rustup.rs"
  need git git "your package manager's git"

  local start=$SECONDS
  echo "local-ci: $(uname -sm), $(rustc --version)"

  step "fmt"
  cargo fmt --all -- --check

  step "clippy"
  heavy clippy --locked --all-targets -- -D warnings

  if ((quick)); then echo "local-ci: --quick PASSED in $((SECONDS - start))s"; return 0; fi

  step "test"
  if cargo nextest --version >/dev/null 2>&1; then
    heavy nextest run --locked --no-fail-fast
    heavy test --locked --doc
  else
    echo "local-ci: cargo-nextest not found, using cargo test (slower)."
    echo "          Install: brew install cargo-nextest, or scripts/ci/install-tool.sh cargo-nextest"
    heavy test --locked --no-fail-fast
  fi

  if ((release)); then
    step "release build"
    heavy build --locked --release
    ls -lh target/release/memdag
  fi

  echo
  echo "local-ci: PASSED in $((SECONDS - start))s on $(uname -n)"
}

# The remote path is meant to expand client-side.
# shellcheck disable=SC2029
run_remote() {
  local incoming="${remote_dir%/}.incoming"
  echo "local-ci: syncing to $remote:~/$remote_dir"
  # Exactly the files git tracks, plus untracked files that aren't ignored.
  # macOS tar would add xattr headers that GNU tar writes out as ._* files,
  # hence COPYFILE_DISABLE and --no-xattrs.
  git -C "$ROOT" ls-files -co --exclude-standard -z \
    | while IFS= read -r -d '' f; do [[ -e "$ROOT/$f" ]] && printf '%s\0' "$f"; done \
    | COPYFILE_DISABLE=1 tar --no-xattrs -C "$ROOT" --null -T - -cf - \
    | ssh "$remote" "rm -rf ~/$incoming && mkdir -p ~/$incoming ~/$remote_dir && tar -x -C ~/$incoming"
  # On the remote: --checksum with no -t means an unchanged file keeps the mtime
  # its cached artifacts were built against, while a changed file gets a new
  # one, so cargo rebuilds exactly what changed. Copying host mtimes could let
  # an older branch reuse a newer build. target/ is never touched. rsync has no
  # .git, so give the run a repo root to resolve.
  ssh "$remote" "rsync -rlp --checksum --delete --exclude /target/ --exclude /.git/ \
      --exclude /.memdag/ ~/$incoming/ ~/$remote_dir/ \
    && rm -rf ~/$incoming && cd ~/$remote_dir && { [ -d .git ] || git init -q; }"

  local args=(--here)
  ((quick)) && args+=(--quick)
  ((release)) && args+=(--release)
  # Login shell, so the remote's PATH (rustup, Homebrew) is loaded.
  # shellcheck disable=SC2029
  ssh -t "$remote" "\$SHELL -lc 'cd ~/$remote_dir && bash scripts/local-ci/ci.sh ${args[*]}'"
}

install_hook() {
  local hook
  hook="$(git -C "$ROOT" rev-parse --path-format=absolute --git-path hooks/pre-push)"
  if [[ -e "$hook" ]] && ! grep -q 'scripts/local-ci/ci.sh' "$hook"; then
    die "$hook exists and is not ours; add this line to it: exec \"\$(git rev-parse --show-toplevel)/scripts/local-ci/ci.sh\" --quick </dev/null"
  fi
  mkdir -p "$(dirname "$hook")"
  cat > "$hook" <<'EOF'
#!/usr/bin/env bash
# Installed by scripts/local-ci/ci.sh --install-hook. Skip once: git push --no-verify
exec "$(git rev-parse --show-toplevel)/scripts/local-ci/ci.sh" --quick </dev/null
EOF
  chmod +x "$hook"
  echo "local-ci: installed pre-push hook (fmt + clippy) at $hook"
}

case "$action" in
  hook) install_hook ;;
  run) if [[ -n "$remote" ]]; then run_remote; else run_here; fi ;;
esac
