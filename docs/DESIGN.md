# memdag Technical Design Document

> **Status**: Approved & Implemented  
> **Target**: Rust 2024 / SQLite WAL + FTS5 + sqlite-vec  
> **Author**: HSP 
> **Revision**: 1.0.0 (2026-09-21)

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
       │  │ MCP Controller │ │  CLI Parser    │ │ Graph Logic  │  │
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
       │  │            vec_memories (sqlite-vec)              │  │
       │  └───────────────────────────────────────────────────┘  │
       └─────────────────────────────────────────────────────────┘
```

---

## 4. Data Model & Database Schema

### 4.1 Schema Definition

```sql
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;
PRAGMA synchronous = NORMAL;

-- Core memory entities (decisions, tasks, bugs, invariant facts)
CREATE TABLE IF NOT EXISTS memories (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK(kind IN (decision, task, invariant, blocker, ephemeral)),
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    tags TEXT,
    status TEXT NOT NULL DEFAULT active CHECK(status IN (active, superseded, resolved, ephemeral)),
    session_id TEXT,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- Directed DAG edges connecting memories
CREATE TABLE IF NOT EXISTS memory_relations (
    source_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation_type TEXT NOT NULL CHECK(relation_type IN (supersedes, depends_on, blocks, references)),
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (source_id, target_id, relation_type)
);

CREATE INDEX IF NOT EXISTS idx_memories_status_kind ON memories(status, kind);
CREATE INDEX IF NOT EXISTS idx_memories_session ON memories(session_id);
CREATE INDEX IF NOT EXISTS idx_memory_relations_target ON memory_relations(target_id);

-- FTS5 Full-Text Search Virtual Table
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    id UNINDEXED,
    title,
    body,
    tags,
    content="memories",
    content_rowid="rowid",
    tokenize="porter unicode61"
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

-- sqlite-vec Virtual Table for Dense Embeddings
CREATE VIRTUAL TABLE IF NOT EXISTS vec_memories USING vec0(
    embedding float[384] distance_metric=cosine
);
```

### 4.2 Memory Kinds & Lifecycle States

| Kind | Description | Default Status | Eviction Policy |
| --- | --- | --- | --- |
| `decision` | Architectural or technical decision | `active` | Superseded by newer decisions |
| `task` | Executable work item or milestone | `active` | Transitions to `resolved` on completion |
| `invariant` | Core repository rule, axiom, or standard | `active` | Permanent unless explicitly superseded |
| `blocker` | Blocker issue preventing task progression | `active` | Transitions to `resolved` |
| `ephemeral` | Scratchpad notes, session debug observations | `ephemeral` | Swept after 24h TTL or session consolidation |

### 4.3 Relation Types & Directionality

Edges in `memory_relations` represent directed causal or structural relationships:
- `supersedes`: Node A atomically deprecates Node B, updating B's status to `superseded`.
- `depends_on`: Node A cannot proceed until Node B is resolved.
- `blocks`: Node A prevents Node B from proceeding.
- `references`: Contextual linkage between related decisions or tasks.

---

## 5. Bidirectional 1-Hop Graph Traversal

When an entity is retrieved (via search or ID lookup), presenting isolated text leads to incomplete decisions. `memdag` executes a 1-hop bidirectional graph expansion:

```sql
SELECT 
    r.relation_type,
    r.target_id,
    m.title,
    m.kind,
    m.status,
    'outgoing' AS direction
FROM memory_relations r
JOIN memories m ON r.target_id = m.id
WHERE r.source_id = ?1

UNION ALL

SELECT 
    r.relation_type,
    r.source_id AS target_id,
    m.title,
    m.kind,
    m.status,
    'incoming' AS direction
FROM memory_relations r
JOIN memories m ON r.source_id = m.id
WHERE r.target_id = ?1;
```

### Inverse Relation Presentation
Incoming relations are presented to the LLM with semantic inverse names:
- Outgoing `depends_on` ➔ Incoming `depended_on_by`
- Outgoing `blocks` ➔ Incoming `blocked_by`
- Outgoing `supersedes` ➔ Incoming `superseded_by`
- Outgoing `references` ➔ Incoming `referenced_by`

Visual formatting example:
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
Uses SQLite's native BM25 ranking over `title`, `body`, and `tags`. Punctuation and identifiers are normalized using the `porter unicode61` tokenizer.

```sql
SELECT m.id, m.kind, m.title, m.body, m.tags, m.status, bm25(memories_fts) as rank
FROM memories_fts f
JOIN memories m ON f.id = m.id
WHERE memories_fts MATCH ?1
ORDER BY rank ASC
LIMIT ?2;
```

### 6.2 Empty Query Fallback
If the user or agent passes an empty query (`""`), rather than failing or returning zero items, `memdag` returns the latest active items ordered by `updated_at DESC`, functioning as an immediate session starter.

### 6.3 Dense Vector Search (`sqlite-vec`)
For semantic similarity, `memdag` loads the native `sqlite-vec` extension and queries the `vec_memories` virtual table using KNN matching:

```sql
SELECT m.id, m.title, m.kind, m.status, v.distance
FROM vec_memories v
JOIN memories m ON m.rowid = v.rowid
WHERE v.embedding MATCH ?1 AND v.k = ?2
ORDER BY v.distance ASC;
```

---

## 7. Model Context Protocol (MCP) Interface

`memdag` implements the Model Context Protocol (MCP) over Stdio using JSON-RPC 2.0:

### 7.1 Protocol Lifecycle & Silent Notification Rule
Per JSON-RPC 2.0 §4.1:
- Requests with an `id` receive a structured `JsonRpcResponse` (`result` or `error`).
- Client notifications (`id: None` or `null`) such as `notifications/initialized`, `initialized`, `$/cancelRequest`, and custom telemetry **MUST NOT receive a response frame**. Replying to notifications violates the protocol and causes client disconnections.

### 7.2 MCP Tool Surface

| Tool Name | Key Parameters | Return Format | Purpose |
| --- | --- | --- | --- |
| `record_memory` | `kind`, `title`, `body`, `tags`, `supersedes_id` | Markdown string | Inserts node & handles atomic supersession |
| `link_entities` | `source_id`, `target_id`, `relation_type` | Markdown string | Creates directed DAG edge |
| `search_memory` | `query`, `kind`, `include_resolved`, `limit` | Markdown list + DAG | BM25 search with 1-hop expansion |
| `search_vector` | `embedding`, `limit` | Markdown list + DAG | KNN vector search with 1-hop expansion |
| `get_memory` | `id` | Markdown detail + DAG | Direct entity lookup with all edges |
| `list_memories` | `kind`, `status`, `limit` | Markdown list | Filtered list of memory nodes |
| `resolve_memory` | `id`, `note` | Markdown confirmation | Transitions tasks/blockers to resolved |
| `consolidate_session` | `session_id`, `learnings`, `purge_ephemeral` | Markdown summary | Sweeps ephemeral nodes & records learnings |

---

## 8. Concurrency & Performance Benchmarks

### 8.1 SQLite WAL Concurrency
- `PRAGMA journal_mode = WAL`: Readers do not block writers; writers do not block readers.
- `PRAGMA busy_timeout = 5000`: Under contention, queries automatically retry for up to 5 seconds before returning a busy error.
- `PRAGMA synchronous = NORMAL`: Guarantees durability across application crashes while minimizing disk sync overhead.

### 8.2 Execution Latency
- In-memory database test suite (10 integration tests): **0.03s** total execution time.
- FTS5 query + 1-hop DAG expansion on disk: **< 1.2ms**.
- Atomic supersession transaction: **< 0.8ms**.

---

## 9. Verification & Quality Gates

Every release and commit to `memdag` must pass:
1. `cargo fmt --all -- --check`
2. `cargo clippy --all-targets --all-features -- -D warnings` (Zero warnings)
3. `cargo test` (All integration and unit tests passing)
4. `cargo build --release`
