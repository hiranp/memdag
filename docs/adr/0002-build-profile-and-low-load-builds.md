# 0002. Trim debuginfo, strip release binaries, and provide a low-load build wrapper

- Status: accepted
- Date: 2026-10-30
- Refs: ADR-0001

## Context

Development happens on a machine shared with other builds; the one-minute load average
reached about 12.8 on 20 cores at the start of the session that produced this ADR. We wanted
faster builds where that is cheap and a way to build without crowding everything else out.

Baseline (commit 63ac8b1 source, clean target dir, `nice -n 19`, `-j4`, Rust 1.99.1, Linux
x86_64, which links with rust-lld by default so a faster linker would add nothing):

| Step | Result |
| --- | --- |
| Clean dev build of all tests (`cargo test --no-run`) | 8.4 s wall |
| Incremental dev build after a real edit | 0.56 s |
| Clean release build | 41.9 s wall, 4.33 MB binary |

The dev loop was already fast; there was little to win there. Most of the cost is compiling
bundled SQLite and sqlite-vec once, which no setting avoids.

## Decision

1. `[profile.dev] debug = "line-tables-only"` and `[profile.dev.package."*"] debug = false`.
   Backtraces and panic locations stay readable; variable-level debuginfo, which nobody here
   uses, is dropped.
2. `[profile.release] strip = true`.
3. `scripts/lowload.sh` runs any cargo command at `nice -n 19` / `ionice -c3`, with jobs set to a
   quarter of the cores and an `flock` so heavy builds from different repos run one at a time.
   It is opt-in; CI calls cargo directly and is unthrottled.
4. The repo-local `.cargo/config.toml` (`jobs = 4`) stays untracked. A committed job limit would
   throttle CI and contributors' machines for a problem that exists on one workstation.

## Measured effect

Same source (the 63ac8b1 baseline), old versus new profile, alternating runs, CPU time summed
over all processes so that contention from other work does not distort it:

| Step | Old profile | New profile | Change |
| --- | --- | --- | --- |
| Clean dev test build, CPU | 25.9 s, 26.8 s | 22.2 s, 22.9 s | about -14% |
| Clean dev test build, wall | 7.8 s, 7.9 s | 6.9 s, 7.1 s | about -12% |
| `target/` after dev build | 419 MB | 237 MB | -43% |
| Release binary | 4,328,904 B | 3,684,952 B | -15% |
| Clean release build, CPU | 65.4 s | 69.9 s | +7% |

Caveats, stated plainly: two dev runs and one release run per side on a busy machine. The
release build is **not** faster (stripping adds a little work and the single wall-clock sample,
37.9 s versus 45.5 s, is within what contention can produce); the win there is the smaller
binary. The dev improvement is real but modest in seconds because the baseline was already
fast; the bigger practical gain is a target directory 43% smaller.

## Alternatives considered

- **mold/lld or a linker flag.** Rust 1.90+ already links x86_64 Linux with rust-lld; nothing to
  gain, and it would add a tool requirement on every machine.
- **sccache or cargo-nextest.** Not installed here, and installing them compiles hundreds of
  crates, the opposite of the goal. Reconsider if CI cache misses become the bottleneck.
- **Merging the three integration test binaries into one** to save links. Roughly one link per
  binary at this size; not worth the churn yet.
- **`lto = "thin"` or `codegen-units = 1` in release.** Smaller or faster binaries, but longer
  release builds across four CI targets, for a CLI that is not CPU-bound.
- **Dropping clap's `color`/`suggestions` features.** Saves a few small crates but removes
  "did you mean" hints for a gain too small to measure.

## Consequences

- Debugging with a variable inspector in dev builds is degraded. Set `CARGO_PROFILE_DEV_DEBUG=2`
  for a session when it is needed.
- Release binaries carry no symbols; use a non-stripped local build to symbolicate a crash.
