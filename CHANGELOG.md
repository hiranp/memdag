# Changelog

All notable changes to memdag. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions follow [Semantic Versioning](https://semver.org/). Releases are cut with
`scripts/bump-version.sh`, which commits the version bump and tags `vX.Y.Z`; pushing the tag runs
`.github/workflows/release.yml`.

## [Unreleased]

Planned as 0.4.0 (new features and a schema bump, no breaking CLI/MCP changes). See
[ADR 0001](docs/adr/0001-evidence-informed-memory-direction.md) and
[ADR 0003](docs/adr/0003-imported-rows-follow-their-sources.md) for the reasoning.

### Added

- **Provenance and staleness.** Memories can cite repo files (`paths`). They are stamped with
  `HEAD` and a content fingerprint of each cited file (a git blob hash; for a directory, a hash
  over its tracked files). Search, `get` and `get_memory` mark a memory **STALE** when a cited
  file's content differs from what it was when the memory was recorded; `memdag stale` lists
  them. Content, not the commit, is the baseline, so recording a decision about a file you just
  edited is not stale, and rebases, amends and shallow clones don't cause false alarms. Available
  via `record --paths/--commit`, the `record_memory` and `consolidate_session` MCP tools, note
  front matter (`paths:`, `commit:`, `fingerprints:`), and the importer. Memories without
  `paths` cost no git calls on write or read.
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
  `Decision:` commit trailers, ADRs in `docs/adr/`, memory note files, and (opt-in with `--gh`)
  `<!-- handoff -->` issue comments. Re-running an import never duplicates. The index refreshes
  when `HEAD` moves: on every CLI `search`, and in a running `serve` before each read tool (at
  most every 2 s).
- **Cheap-model workers.** memdag hands out bulk upkeep as batches with instructions, for the
  calling agent to run on a Haiku/Flash-class subagent. memdag still never calls a model. New
  MCP tools `get_work`/`submit_work`, MCP prompts `expand_keywords`/`verify_stale`, and CLI
  `memdag work next|submit|prompt|install|uninstall`.
  - `expand`: the worker writes search keywords, indexed for full-text search at half weight.
    An alternative to an embedding model.
  - `verify`: the worker checks STALE memories against the code. It confirms them
    (re-baseline), corrects them (a new memory supersedes the old one), or leaves them flagged.
  - `memdag work install --client claude-code|opencode [--global] [--model]` writes a
    `memdag-worker` subagent (Claude Code default `model: haiku`) and a `<!-- MEMDAG_START -->`
    block in `CLAUDE.md`/`AGENTS.md`. Uninstall removes only what memdag wrote.
  - Worker output is treated as untrusted. It is secret-checked and length-capped. The
    requested task wins over the reply. A verdict applies only to an active memory that is
    stale right now, once per id. Corrections are tagged `worker-corrected` and carry the
    worker's reason.
- **Imports prune.** A deleted note or ADR, an amended or rebased-away `Decision:` commit, or a
  deleted handoff comment is removed from the index on the next import (`N removed` in the
  report). Files that fail to parse keep their row. Removing a `supersedes:` line, or the
  superseding note, makes the older memory active again.
- **Resolutions survive a rebuild.** Under write-through, `resolve_memory` on a note-backed
  memory writes an append-only `<id>.resolved.md` sidecar that the importer merges back.
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

- **Schema v4** adds `memories.keywords` and rebuilds the FTS index with a `keywords` column.
- **Schema v3** adds `origin_repo`, so rows are owned per repository: an import never prunes or
  overwrites rows from another repo sharing the database. The migration also drops orphaned
  vectors.
- Switching `MEMDAG_EMBED_CMD` drops vectors computed by the previous command (recorded in
  `memdag_meta`) instead of silently mixing two vector spaces.
- Note files are written to a temp file, fsynced and linked into place, so a crash can't leave
  a truncated note.
- **Schema v2** adds nullable `paths`, `commit_sha`, `fingerprints` and `source` columns
  (`ALTER TABLE ADD COLUMN`, no table rebuild) and triggers that drop a memory's vector when the
  memory is deleted or its title/body is rewritten (appending a resolution note keeps it). Databases migrate on open; existing note files stay valid and notes without
  provenance render byte-for-byte as before.
- MCP `search_memory` now applies the 2000-token default budget (previously unbounded). Pass
  `max_tokens: 0` for the old behaviour.
- `search_vector` is advertised over MCP only when an embedder is configured. Previously it
  required the calling model to supply 384 floats, which a language model cannot do; the tool
  still accepts an explicit `embedding` for programmatic callers.
- Dev profile uses `debug = "line-tables-only"` (dependencies carry no debuginfo) and the
  release profile strips symbols; see [ADR 0002](docs/adr/0002-build-profile-and-low-load-builds.md)
  for measurements.

- Bare `memdag import` now imports only the local sources (git, ADRs, notes). GitHub comments
  need an explicit `--gh`. A note file's `id:` must equal its file name.
- Imports refuse to overwrite a row they don't own: a note, ADR or comment that reuses the id
  of a locally recorded or differently sourced memory is skipped with a warning.
- Secret detection is token-shaped instead of substring-based, so `task-queue` and `risk-based`
  no longer trip `sk-`, and JWTs, GitLab/npm/Slack tokens and `password = ...` assignments are
  caught. Imports and resolution notes are checked too.

### Fixed

- `--tags` no longer collides with a short flag in clap.
- Opening a brand-new database from several processes at once could fail with
  `duplicate column name` or `SQLITE_BUSY`: schema creation is now re-checked under the write
  lock and `busy_timeout` is set before switching to WAL.
- Opening a database written by a newer memdag is refused with a clear error instead of being
  treated as current.
- `--since` no longer marks everything up to `HEAD` as imported, and rejects option-like values.

### Security

- The embed command is read from the environment only, never from `memdag.toml`, so a cloned
  repository cannot make memdag execute a command.
- Imported content is untrusted: GitHub handoffs are accepted only from OWNER/MEMBER/
  COLLABORATOR authors and only when the comment starts with the marker; imported text gets the
  secret check and a 20 KB size cap; search output labels imported rows (`source: note`) and
  escapes heading lines so memory text cannot forge result blocks.
- The embed command is killed as a process group on timeout, and skipped for 60 s after a
  failure so a dead embedder costs one timeout, not one per request.
- Cited paths and commits are validated (repo-relative, no `..`, no leading `-`, hex commits)
  on write and again on import, and git is invoked with `--literal-pathspecs` and `--`, so a
  hostile note file cannot inject git options or pathspec magic.

### Known limitations

See ADR 0001. Staleness spawns git once per cited path per result shown; `gh` import fetches
every comment in the repository; `search_vector` ignores status and kind filters; a commit body
containing the byte `0x1f` confuses the git importer; the embedder's identity is its command
string, so changing the model behind an unchanged command is not detected.

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
