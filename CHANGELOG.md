# Changelog

All notable changes to memdag. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions follow [Semantic Versioning](https://semver.org/). Releases are cut with
`scripts/bump-version.sh`, which commits the version bump and tags `vX.Y.Z`; pushing the tag runs
`.github/workflows/release.yml`.

## [Unreleased]

Planned as 0.4.0 (new features and a schema bump, no breaking CLI/MCP changes). See
[ADR 0001](docs/adr/0001-evidence-informed-memory-direction.md) for the reasoning.

### Added

- **Provenance and staleness.** Memories can cite repo files (`paths`) and are stamped with the
  git commit they were recorded at (`commit`, defaulting to `HEAD`). Search, `get` and
  `get_memory` mark a memory **STALE** when a cited file changed since (committed, uncommitted or
  deleted); `memdag stale` lists them. Available via `record --paths/--commit`, the
  `record_memory` and `consolidate_session` MCP tools, note front matter (`paths:`, `commit:`),
  and the importer. Memories without `paths` cost no git calls.
- **Token-budgeted recall.** `search_memory` / `memdag search` pack results in rank order into an
  estimated token budget (`max_tokens` / `--max-tokens`, default 2000, `0` = unlimited). The top
  result is always returned (shortened if needed), relations are capped at six per result, and
  the reply reports how many results were omitted.
- **Hybrid search with an optional embedder.** Set `MEMDAG_EMBED_CMD` to any command that maps
  stdin text to a 384-float JSON vector. `search_memory` then fuses BM25 and vector hits with
  Reciprocal Rank Fusion (k=60); new memories are embedded on write (best effort) and
  `memdag embed` backfills existing ones. `search_vector` now accepts plain `query` text.
  No model is bundled and no dependency was added.
- **Git-backed memory:** `memdag import` rebuilds the SQLite index from git-tracked text:
  `Decision:` commit trailers, ADRs in `docs/adr/`, memory note files, and (optionally, via
  `gh`) `<!-- handoff -->` issue comments. The database is a disposable cache; re-running an
  import never duplicates. The index refreshes automatically on `serve` and `search` when
  `HEAD` moves.
- **Write-through notes.** With `[write_through] dir = "docs/memory"` in `memdag.toml`, durable
  memories (decision, invariant, learning, blocker) are first written as append-only note files
  and indexed from them, so a rebuild reproduces identical rows.
- New memory kind `learning`; `mcp install --client` presets for Claude Code, OpenCode and
  Antigravity; `--key` for VS Code's `servers` key; JSON export sidecar (`memdag export`);
  default database path scoped to the project (`<repo>/.memdag/memdag.db`).
- `scripts/lowload.sh`: run any cargo command at lowest CPU/IO priority with bounded jobs and a
  machine-wide lock, for development on a shared or busy machine.
- ADRs in `docs/adr/` (memdag imports its own ADRs as memories).

### Changed

- **Schema v2** adds nullable `paths` and `commit_sha` columns (`ALTER TABLE ADD COLUMN`, no
  table rebuild). Databases migrate on open; existing note files stay valid and notes without
  provenance render byte-for-byte as before.
- MCP `search_memory` now applies the 2000-token default budget (previously unbounded). Pass
  `max_tokens: 0` for the old behaviour.
- `search_vector` is advertised over MCP only when an embedder is configured. Previously it
  required the calling model to supply 384 floats, which a language model cannot do; the tool
  still accepts an explicit `embedding` for programmatic callers.
- Dev profile uses `debug = "line-tables-only"` (dependencies carry no debuginfo) and the
  release profile strips symbols; see [ADR 0002](docs/adr/0002-build-profile-and-low-load-builds.md)
  for measurements.

### Fixed

- `--tags` no longer collides with a short flag in clap.

### Security

- The embed command is read from the environment only, never from `memdag.toml`, so a cloned
  repository cannot make memdag execute a command.
- Cited paths and commits are validated (repo-relative, no `..`, no leading `-`, hex commits)
  on write and again on import, and git is invoked with `--literal-pathspecs` and `--`, so a
  hostile note file cannot inject git options or pathspec magic.

## [0.3.0] - 2026-09-21

### Added

- Ready queue: `memdag ready` / `list_ready` returns active tasks and blockers with no incoming
  active `blocks` edge, computed in one SQL query.
- Warning when `mcp install` registers a binary from an unstable location (not on `PATH`, or a
  temp/build/downloads directory); a bundled `memdag` Skill.
- Task-tracking guidance surfaced to non-Claude MCP clients.

### Fixed

- `SQLITE_BUSY` under many concurrent short-lived processes: the schema DDL batch now runs once
  per database lifetime instead of on every start.

## [0.2.0] - 2026-09-21

First tagged release.

### Added

- SQLite (WAL) memory store with FTS5 BM25 search, a `sqlite-vec` vector table, directed-graph
  relations with atomic supersession and bidirectional 1-hop expansion, a stdio MCP server, and
  a CLI.
- Agent-facing `instructions` in the MCP `initialize` response.
- Secret denylist for recorded content and an ephemeral-memory TTL sweep.
- Cross-platform release workflow (Linux, Windows, macOS Intel and Apple Silicon) and
  version-bump automation (`scripts/bump-version.sh`, `bump-version.yml`).
- Technical design document (`docs/DESIGN.md`).

[Unreleased]: https://github.com/hiranp/memdag/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/hiranp/memdag/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/hiranp/memdag/releases/tag/v0.2.0
