#!/usr/bin/env bash
# Run a cargo command without hogging the machine: lowest CPU/IO priority, bounded
# parallelism, and one heavy build at a time machine-wide (so builds from several repos
# don't pile up and push the load average through the roof).
#
# Usage: scripts/lowload.sh test            # any cargo args work
#        MEMDAG_JOBS=2 scripts/lowload.sh clippy --all-targets
#
# Default jobs = a quarter of the cores (min 1). CI should call cargo directly.
set -euo pipefail

cores=$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)
jobs="${MEMDAG_JOBS:-$(( cores / 4 > 0 ? cores / 4 : 1 ))}"
export CARGO_BUILD_JOBS="$jobs"

cmd=(cargo "$@")
command -v ionice >/dev/null 2>&1 && cmd=(ionice -c3 "${cmd[@]}")
command -v nice >/dev/null 2>&1 && cmd=(nice -n 19 "${cmd[@]}")

lock="${CARGO_HEAVY_LOCK:-${TMPDIR:-/tmp}/cargo-heavy.lock}"
if command -v flock >/dev/null 2>&1; then
    echo "[lowload] jobs=$jobs, waiting for $lock if another heavy build is running" >&2
    exec flock "$lock" "${cmd[@]}"
fi
echo "[lowload] jobs=$jobs (flock not installed; no cross-project lock)" >&2
exec "${cmd[@]}"
