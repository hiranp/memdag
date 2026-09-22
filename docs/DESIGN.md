# memdag Technical Design Document

> **Status**: Approved & Implemented
> **Target**: Rust 2024 / SQLite WAL + FTS5 + sqlite-vec
> **Author**: HP
> **Revision**: 1.1.0 (2026-11-24)

---

## 1. Executive Summary & Problem Statement

Modern agentic software engineering workflows frequently suffer from severe context degradation across long multi-turn sessions:

1. **The Zombie Context Problem**: When agents rely on flat markdown notes (`AGENTS.md`, `CLAUDE.md`, or raw commit logs), old or superseded decisions persist alongside new ones without relational semantics. The LLM cannot reliably determine whether a past architectural decision is still active, modified, or repealed.
2. **The Fragile Daemon Problem**: Heavy external vector stores or distributed databases (e.g. Dolt, Chroma, Postgres, Neo4j) introduce complex background daemons, socket connections, container runtimes, and file-locking races that fail in headless CI/CD, subshells, and background Git hooks.
3. **The Pure-Vector Semantic Gap**: Pure embedding search fails on exact syntactic tokens, compiler errors, file paths, and function identifiers, while lacking graph semantics (dependency blocking, architectural hierarchy).

**memdag** solves these challenges by providing an in-process, zero-daemon, relational Directed Acyclic Graph (DAG) memory engine embedded in Rust on top of SQLite Write-Ahead Logging (WAL), full-text search (FTS5 BM25), and embedded vector similarity (`sqlite-vec`).

---

## 2. Architectural Pillars

- **Zero Daemons / In-Process**: Runs entirely embedded within the agent harness process via stdio MCP or as a fast standalone CLI. No background ports, background threads, or Docker containers.
- **Durable Concurrency via SQLite WAL**: Multiple agent processes and subagents can read concurrently while write transactions complete in sub-millisecond ACID transactions with a 5000ms busy timeout.
- **Relational DAG Semantics**: Memories are nodes; dependencies, blockers, references, and supersessions are first-class directed edges. Superseding a decision automatically updates graph state and isolates stale context.
- **Bidirectional 1-Hop Expansion**: Memory search does not merely return disconnected text snippets; it expands 1-hop outgoing (`➔`) and incoming (`◄`) graph edges to provide immediate relational context.
- **Hybrid Retrieval**: Combines BM25 porter-stemmed full-text search with embedded `sqlite-vec` KNN dense embeddings.
- **Clean Protocol Compliance**: Fully compliant with Model Context Protocol (MCP) JSON-RPC 2.0 specifications, including proper handling of client notifications and cancellation frames.
- **Self-Cleaning & Self-Guiding**: Ephemeral memories expire on a TTL sweep even if a session crashes, obvious secrets are rejected at write time, and the MCP `initialize` response tells the calling agent when to use each tool — no external cron, git hook, or prompt-engineering scaffolding required.

---

## 3. System Architecture

```text
       ┌─────────────────────────────────────────────────────────┐
       │               AI Coding Harness / LLM Client            │
       │           (Antigravity, Claude Code, Cursor, VSCode)    │
       └────────────────────────────┬────────────────────────────┘
                                    │
                                    │ Stdio JSON-RPC 2.0
                                    ▼
       ┌─────────────────────────────────────────────────────────┐
       │                      memdag Engine                      │
       │                                                         │
       │  ┌────────────────┐ ┌────────────────┐ ┌──────────────┐  │
       │  │ MCP Controller │ │  CLI Parser    │ │ Store/Graph  │  │
       │  │   (mcp.rs)     │ │   (cli.rs)     │ │  (store.rs)  │  │
       │  └────────┬───────┘ └────────┬───────┘ └──────┬───────┘  │
       └───────────┼──────────────────┼────────────────┼─────────┘
                   │                  │                │
                   └──────────────────┼────────────────┘
                                      ▼
       ┌─────────────────────────────────────────────────────────┐
       │                SQLite Storage Layer (WAL)               │
       │                                                         │
       │  ┌────────────────┐ ┌────────────────┐ ┌──────────────┐  │
       │  │    memories    │ │memory_relations│ │ memories_fts │  │
       │  │   (Core DAG)   │ │ (Edges/Cascade)│ │ (FTS5 BM25)  │  │
       │  └────────────────┘ └────────────────┘ └──────────────┘  │
       │  ┌───────────────────────────────────────────────────┐  │
       │  │              memories_vec (sqlite-vec)             │  │
       │  └───────────────────────────────────────────────────┘  │
       └─────────────────────────────────────────────────────────┘
```

The MCP controller (`src/mcp.rs`) and CLI parser (`src/cli.rs` + `src/main.rs`) are two thin front ends over one `MemoryStore` (`src/store.rs`); every write path (`record_memory`, `link_entities`, `consolidate_session`, `resolve_memory`) is shared, so the CLI and the MCP server can never drift in behavior.

### 3.1 Database Location & Export

`default_db_path()` (`src/db.rs`) resolves, in order: `$MEMDAG_DB` → `<repo>/.memdag/memdag.db`
for the nearest ancestor of `cwd` containing `.memdag/` or `.git` → the OS application data
directory as a global fallback when no repo is found. Scoping to the project root means each
repo's memories, tasks, and `list_ready` queue never mix with another project's; the walk-up
check for an existing `.memdag/` before `.git` lets a monorepo pin one shared DB for nested
crates. `MemoryStore::export_all()` / `memdag export` serializes every memory and relation to
plain JSON as a read-only sidecar (for committing a snapshot or moving data between machines) —
it is not a sync mechanism and has no corresponding importer.

---

## 4. Data Model & Database Schema

### 4.1 Schema Definition

Source of truth: `src/db.rs`. Every connection open runs `PRAGMA_SQL` (cheap, per-connection
state) unconditionally, then checks `sqlite_master` for the `memories` table and only runs the
`DDL_SQL` batch (`CREATE TABLE`/`INDEX`/`TRIGGER`/`VIRTUAL TABLE`, all idempotent `IF NOT
EXISTS`) if it's missing — see §8.1 for why that guard exists.

```sql
-- PRAGMA_SQL: applied on every open()
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;
PRAGMA synchronous = NORMAL;

-- DDL_SQL: applied once, only if the schema doesn't exist yet
-- Core memory entities (decisions, tasks, bugs, invariant facts)
CREATE TABLE IF NOT EXISTS memories (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK(kind IN ('decision', 'task', 'invariant', 'blocker', 'ephemeral')),
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    tags TEXT,
    status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active', 'superseded', 'resolved', 'ephemeral')),
    session_id TEXT,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- Directed DAG edges connecting memories
CREATE TABLE IF NOT EXISTS memory_relations (
    source_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation_type TEXT NOT NULL CHECK(relation_type IN ('supersedes', 'depends_on', 'blocks', 'references')),
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (source_id, target_id, relation_type)
);

CREATE INDEX IF NOT EXISTS idx_memory_relations_source ON memory_relations(source_id);
CREATE INDEX IF NOT EXISTS idx_memory_relations_target ON memory_relations(target_id);
CREATE INDEX IF NOT EXISTS idx_memories_status ON memories(status);
CREATE INDEX IF NOT EXISTS idx_memories_kind ON memories(kind);
CREATE INDEX IF NOT EXISTS idx_memories_session ON memories(session_id);

-- FTS5 Full-Text Search Virtual Table
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    id UNINDEXED,
    title,
    body,
    tags,
    content='memories',
    content_rowid='rowid',
    tokenize='porter unicode61'
);

-- Automatic FTS5 Synchronization Triggers
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, id, title, body, tags)
    VALUES (new.rowid, new.id, new.title, new.body, new.tags);
END;

CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, id, title, body, tags)
    VALUES ('delete', old.rowid, old.id, old.title, old.body, old.tags);
END;

CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, id, title, body, tags)
    VALUES ('delete', old.rowid, old.id, old.title, old.body, old.tags);
    INSERT INTO memories_fts(rowid, id, title, body, tags)
    VALUES (new.rowid, new.id, new.title, new.body, new.tags);
END;

-- sqlite-vec Virtual Table for Dense Embeddings (default L2/Euclidean distance;
-- no distance_metric override configured)
CREATE VIRTUAL TABLE IF NOT EXISTS memories_vec USING vec0(
    id TEXT PRIMARY KEY,
    embedding float[384]
);
```

> **Note**: there is no index on `memories.created_at`. The ephemeral TTL sweep (§6.4) does a
> full scan filtered by the existing `idx_memories_status`/`idx_memories_kind` index, then a
> `created_at` comparison. Fine at personal/small-project scale; add a composite
> `(status, created_at)` index if a single memdag database ever grows past tens of thousands
> of ephemeral rows.

### 4.2 Memory Kinds & Lifecycle States

| Kind | Description | Default Status | Eviction Policy |
| --- | --- | --- | --- |
| `decision` | Architectural or technical decision | `active` | Superseded by newer decisions |
| `task` | Executable work item or milestone | `active` | Transitions to `resolved` via `resolve_memory` |
| `invariant` | Core repository rule, axiom, or standard | `active` | Permanent unless explicitly superseded |
| `blocker` | Blocker issue preventing task progression | `active` | Transitions to `resolved` via `resolve_memory` |
| `ephemeral` | Scratchpad notes, session debug observations | `ephemeral` | Purged/archived by `consolidate_session`, or swept by TTL (§6.4) if the session never consolidates |

### 4.3 Relation Types & Directionality

Edges in `memory_relations` represent directed causal or structural relationships:
- `supersedes`: Node A atomically deprecates Node B, updating B's status to `superseded`.
- `depends_on`: Node A cannot proceed until Node B is resolved.
- `blocks`: Node A prevents Node B from proceeding.
- `references`: Contextual linkage between related decisions or tasks.

---

## 5. Bidirectional 1-Hop Graph Traversal

When an entity is retrieved (via search or ID lookup), presenting isolated text leads to incomplete decisions. `memdag` executes a 1-hop bidirectional graph expansion via `MemoryStore::get_memory_relations` (`src/store.rs`), the single relation-fetching path shared by `get_memory`, `search_memory`, and `search_vector`:

```sql
SELECT r.relation_type, target.id, target.title, target.kind, target.status, 'outgoing' AS direction
FROM memory_relations r
JOIN memories target ON r.target_id = target.id
WHERE r.source_id = ?1

UNION ALL

SELECT r.relation_type, source.id, source.title, source.kind, source.status, 'incoming' AS direction
FROM memory_relations r
JOIN memories source ON r.source_id = source.id
WHERE r.target_id = ?1;
```

`search_memory`/`search_vector` rank the top matches first (BM25 or KNN distance, capped at `limit`, default `100`), then fetch relations per hit — one indexed query per result rather than one large multi-way join, since result sets are small (≤ 100 rows) by construction.

### Inverse Relation Presentation
Incoming relations are presented to the LLM with semantic inverse names (`RelationType::inverse`):
- Outgoing `depends_on` ➔ Incoming `depended_on_by`
- Outgoing `blocks` ➔ Incoming `blocked_by`
- Outgoing `supersedes` ➔ Incoming `superseded_by`
- Outgoing `references` ➔ Incoming `referenced_by`

All markdown rendering of relations (CLI `get`, MCP `get_memory`, and search result formatting) goes through one shared helper, `store::format_relations`, so the arrow/label logic exists exactly once:

```markdown
### [TSK-002] Verify lock handling (kind: task, status: active)
- **Body**: Run concurrent read/write test under load.
- **Relations (1-Hop DAG)**:
  - `depended_on_by` ◄ [DEC-2026-001] Adopt SQLite WAL (active)
  - `blocked_by` ◄ [BLK-004] macOS sandbox permission failure (active)
```

---

## 6. Search Engine & Retrieval Pipeline

`memdag` employs a multi-strategy retrieval pipeline:

### 6.1 FTS5 BM25 Keyword Search
Uses SQLite's native BM25 ranking over `title`, `body`, and `tags`. Punctuation and identifiers are normalized using the `porter unicode61` tokenizer. `sanitize_fts5_query` strips each whitespace-delimited token down to `[A-Za-z0-9_-]` and appends a `*` prefix-match suffix before it reaches SQLite.

```sql
SELECT m.id, m.kind, m.title, m.body, m.tags, m.status, m.session_id, m.created_at, m.updated_at,
       bm25(memories_fts) AS rank
FROM memories_fts f
JOIN memories m ON f.rowid = m.rowid
WHERE memories_fts MATCH :query
  AND (:include_resolved = 1 OR m.status = 'active')
  AND (:kind IS NULL OR m.kind = :kind)
ORDER BY rank
LIMIT :limit;
```

### 6.2 Empty Query Fallback
If the query sanitizes down to nothing (empty string, or only punctuation/whitespace), rather than failing or returning zero items, `memdag` falls back to `list_memories`, returning the most recently updated items (`ORDER BY updated_at DESC`) — an immediate session starter with `rank = 0.0`.

### 6.3 Dense Vector Search (`sqlite-vec`)
For semantic similarity, `memdag` loads the native `sqlite-vec` extension and queries the `memories_vec` virtual table using KNN matching, then attaches the same 1-hop relations as keyword search:

```sql
SELECT m.id, m.kind, m.title, m.body, m.tags, m.status, m.session_id, m.created_at, m.updated_at,
       v.distance
FROM memories_vec v
JOIN memories m ON v.id = m.id
WHERE v.embedding MATCH ?1 AND v.k = ?2
ORDER BY v.distance ASC;
```

`search_vector` and `search_memory` both return `Vec<MemorySearchResult>` (`{ memory, rank, relations }`), so the CLI/MCP formatter (`format_search_results_for_llm`) renders either result set identically; `rank` is BM25 score for keyword search and raw vector distance (lower = closer) for KNN search.

### 6.4 Ephemeral TTL Sweep
`MemoryStore::purge_expired_ephemeral(max_age_secs)` deletes rows where `status = 'ephemeral' OR kind = 'ephemeral'` and `created_at` is older than `max_age_secs`. It runs once at the start of every CLI invocation (`main.rs`, before dispatching a subcommand) with a default of 24h, overridable via `MEMDAG_EPHEMERAL_TTL_SECS`. This exists specifically to cover sessions that crash or are killed before calling `consolidate_session`, which is the normal (purge/archive) ephemeral cleanup path.

### 6.5 Secret Denylist
`check_for_secrets` (`src/store.rs`) runs on `title`/`body`/`tags` inside `record_memory` and `consolidate_session`, before any row is inserted. It's a case-insensitive substring match against a fixed list of common credential markers (`sk-`, `ghp_`/`gho_`/`github_pat_`, `xoxb-`/`xoxp-`, `AKIA`, `AIza`, `-----BEGIN `, `api_key=`, `Authorization: Bearer`, etc.). A match aborts the write with an error naming the matched pattern (never the matched secret itself). This is a denylist, not a scanner — it catches obvious accidental paste-ins, not deliberately obfuscated secrets; extend `SECRET_PATTERNS` if a new credential shape shows up in the wild.

### 6.6 Ready Queue (Multi-Agent Coordination)
`MemoryStore::list_ready` returns `kind IN ('task', 'blocker')` rows with `status = 'active'` and no incoming active `blocks` edge, in one indexed query:

```sql
SELECT {MEMORY_COLS} FROM memories m
WHERE m.status = 'active'
  AND m.kind IN ('task', 'blocker')
  AND NOT EXISTS (
      SELECT 1 FROM memory_relations r
      JOIN memories b ON r.source_id = b.id
      WHERE r.target_id = m.id
        AND r.relation_type = 'blocks'
        AND b.status = 'active'
  )
ORDER BY m.created_at ASC
LIMIT :limit;
```

Exposed as CLI `memdag ready` and MCP `list_ready`. It exists so multiple agents/subagents working the same project can each ask "what's claimable right now" in one call instead of `list_memories` + `get_memory`-per-row to check `blocked_by` relations. It is **not** an atomic claim: two agents can both read the same ready item before either resolves it (no `assignee`/`claimed_by` column). See README §Scope & Limitations for the comparison against tools (e.g. beads) that do provide atomic claiming.

---

## 7. Model Context Protocol (MCP) Interface

`memdag` implements the Model Context Protocol (MCP) over Stdio using JSON-RPC 2.0 (`src/mcp.rs`).

### 7.1 Protocol Lifecycle & Silent Notification Rule
Per JSON-RPC 2.0 §4.1:
- Requests with an `id` receive a structured `JsonRpcResponse` (`result` or `error`).
- Client notifications (no `id` field) **MUST NOT receive a response frame**, including unrecognized ones. `handle_request`'s catch-all arm checks `req_id.as_ref()?` before building an error response, so any notification — known or not — is silently dropped instead of triggering a `-32601 Method not found` reply that would violate the spec.

### 7.2 Agent-Facing Instructions
The `initialize` response includes an `instructions` string (`AGENT_INSTRUCTIONS` in `mcp.rs`) telling the calling model to call `search_memory` before starting a task, `record_memory`/`link_entities` at decision points, `list_ready` before claiming task/blocker work (especially when other agents may be on the same project), and `consolidate_session` before ending a session. Clients that surface MCP server instructions to the model (Claude Code, Claude Desktop) inject this automatically — no project-level hooks or prompt scaffolding required. Clients that don't honor the field can get the same guidance by copying it into `AGENTS.md`/`CLAUDE.md` (see README).

### 7.3 MCP Tool Surface

| Tool Name | Key Parameters | Return Format | Purpose |
| --- | --- | --- | --- |
| `record_memory` | `kind`, `title`, `body`, `tags`, `supersedes_id`, `session_id`, `id` | Markdown string | Inserts node & handles atomic supersession; rejected if content matches the secret denylist |
| `link_entities` | `source_id`, `target_id`, `relation_type` | Markdown string | Creates directed DAG edge |
| `search_memory` | `query`, `kind`, `include_resolved`, `limit` | Markdown list + DAG | BM25 search with 1-hop expansion, empty-query fallback |
| `search_vector` | `embedding`, `limit` | Markdown list + DAG | KNN vector search with 1-hop expansion |
| `get_memory` | `id` | Markdown detail + DAG | Direct entity lookup with all incoming/outgoing edges |
| `list_memories` | `kind`, `status`, `limit` | Markdown list | Filtered list of memory nodes |
| `list_ready` | `limit` | Markdown list | Claimable tasks/blockers: active, no incoming active `blocks` edge (§6.6) |
| `resolve_memory` | `id`, `note` | Markdown confirmation | Transitions a memory to `resolved`, appending an optional note to its body |
| `consolidate_session` | `session_id`, `learnings`, `purge_ephemeral` | Markdown summary | Records durable learnings; purges or archives the session's ephemeral nodes |

### 7.4 Client Registration
The CLI's `mcp install`/`mcp uninstall` subcommands (`src/main.rs`) merge or remove a `mcpServers.memdag` entry in a target JSON file — project-local `./.mcp.json` by default, `~/.claude.json` with `--global`, or any file via `--path` (only the `mcpServers` key is touched, so this is safe on a client's shared config file). This only targets Claude Code's known config locations directly; other clients (Cursor, VS Code, Windsurf) use their own file path and, in VS Code's case, a different top-level key (`servers` instead of `mcpServers`) — use `--path` to point at those.

---

## 8. Concurrency & Performance Benchmarks

### 8.1 SQLite WAL Concurrency
- `PRAGMA journal_mode = WAL`: Readers do not block writers; writers do not block readers.
- `PRAGMA busy_timeout = 5000`: Under contention, queries automatically retry for up to 5 seconds before returning a busy error.
- `PRAGMA synchronous = NORMAL`: Guarantees durability across application crashes while minimizing disk sync overhead.
- **DDL contention under concurrent short-lived processes**: `CREATE TABLE/INDEX IF NOT EXISTS` is idempotent in effect, but each statement still takes a schema lock to check. A 20-process × 5-record concurrency stress test (multiple `memdag record` CLI invocations against one db file, simulating concurrent subagents) surfaced sporadic `SQLITE_BUSY` ("database is locked") when the full DDL batch ran on every process start, even with `busy_timeout` set. Fixed by gating `DDL_SQL` behind a `sqlite_master` existence check (§4.1) so it only runs once per database's lifetime; verified with the same stress test at higher volume afterward with zero errors.

### 8.2 Execution Latency
- Full in-memory integration test suite (16 tests across 4 binaries): **< 0.1s** total execution time.
- FTS5 query + 1-hop DAG expansion and atomic supersession transactions both complete in low-single-digit milliseconds against an in-memory database; no dedicated on-disk benchmark harness exists yet — treat these as order-of-magnitude, not measured SLAs.

---

## 9. Verification & Quality Gates

Every commit to `memdag` should pass:
1. `cargo build`
2. `cargo test` (all unit + integration tests, currently 16)
3. `cargo clippy --all-targets` (zero warnings)
4. `cargo fmt --all -- --check`

CI (`.github/workflows/release.yml`) builds release binaries for Linux (x86_64-gnu), Windows (x86_64-msvc), and macOS (Intel + Apple Silicon) on every `vX.Y.Z` tag push. `.github/workflows/bump-version.yml` + `scripts/bump-version.sh` automate the version bump, commit, and tag that trigger it.
