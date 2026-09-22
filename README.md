# memdag

> Local-first relational DAG and FTS5 memory engine for AI coding harnesses.

[![Rust](https://img.shields.io/badge/rust-2024%20edition-orange.svg)](https://www.rust-lang.org/)
[![SQLite](https://img.shields.io/badge/sqlite-WAL%20%2B%20FTS5-blue.svg)](https://www.sqlite.org/)
[![sqlite-vec](https://img.shields.io/badge/sqlite--vec-v0.1.9-green.svg)](https://github.com/asg017/sqlite-vec)
[![MCP](https://img.shields.io/badge/protocol-MCP%20Stdio-purple.svg)](https://modelcontextprotocol.io/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)

memdag is a local, durable memory layer for AI coding agents. It stores memories as a directed graph, supports full-text retrieval, keeps decisions and context synchronized, and avoids the operational cost of background daemons and fragile flat-note systems.

## Why memdag

Modern AI coding workflows tend to fail in one of three ways:

1. Heavy external memory systems
   - Require background services, databases, Docker, and extra runtime complexity.
   - Add startup friction and drift across sessions.
2. Flat markdown or key-value stores
   - Cause zombie context and stale decisions to compete with current ones.
   - Create low-signal retrieval and poor dependency tracking.
3. Pure vector-only stores
   - Blur exact technical identifiers and error signatures.
   - Miss the relational semantics that codebases depend on.

memdag combines the useful parts of each approach in a single in-process SQLite-backed system:

- WAL-backed SQLite for safe concurrent access without daemons
- FTS5 BM25 search for deterministic retrieval of symbols, errors, and architectural phrases
- DAG supersession for explicit memory replacement and deprecation
- 1-hop relation expansion to surface dependent context with search results
- Embedded sqlite-vec support for semantic vectors without an external service
- Ephemeral-session cleanup to keep scratch context from polluting long-term memory

## Core architecture

```text
AI coding harness
        │
        │ Stdio MCP JSON-RPC
        ▼
      memdag (Rust)
        │
        ├─ MCP tools: record, link, search, consolidate
        │
        ▼
    SQLite + WAL + foreign keys
        │
        ├─ memories
        ├─ memory_relations
        ├─ memories_fts (BM25)
        └─ memories_vec (sqlite-vec)
```

### SQLite schema

```sql
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;

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

CREATE TABLE IF NOT EXISTS memory_relations (
    source_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation_type TEXT NOT NULL CHECK(relation_type IN ('supersedes', 'depends_on', 'blocks', 'references')),
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (source_id, target_id, relation_type)
);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    id UNINDEXED,
    title,
    body,
    tags,
    content='memories',
    content_rowid='rowid',
    tokenize='porter unicode61'
);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_vec USING vec0(
    id TEXT PRIMARY KEY,
    embedding float[384]
);
```

## MCP interface

memdag exposes a compact standard MCP surface over stdio:

| Tool | Parameters | Description |
| --- | --- | --- |
| `record_memory` | `kind`, `title`, `body`, `tags`, `supersedes_id`, `session_id`, `id` | Inserts a new memory node and can atomically supersede an older one. |
| `link_entities` | `source_id`, `target_id`, `relation_type` | Inserts a directed edge such as `depends_on`, `blocks`, `references`, or `supersedes`. |
| `search_memory` | `query`, `kind`, `include_resolved`, `limit` | Runs BM25 search with 1-hop DAG expansion for richer retrieval. |
| `consolidate_session` | `session_id`, `learnings`, `purge_ephemeral` | Promotes learnings and prunes ephemeral session noise. |

## Installation

### Build from source

```bash
git clone https://github.com/hiranp/memdag.git
cd memdag
cargo build --release
```

The binary is available at `target/release/memdag`.

### Install the MCP server

`memdag mcp install` writes the client config for you so you do not need to hand-edit JSON files.

```bash
# project-local config
memdag mcp install

# user-global config
memdag mcp install --global

# specific config path
memdag mcp install --path ~/.cursor/mcp.json
```

To remove it again:

```bash
memdag mcp uninstall
memdag mcp uninstall --global
memdag mcp uninstall --path ~/.cursor/mcp.json
```

### Manual configuration

Add `memdag` to a client config directly:

```json
{
  "mcpServers": {
    "memdag": {
      "command": "/usr/local/bin/memdag",
      "args": ["serve"],
      "env": {
        "MEMDAG_DB": "/Users/username/.memdag/memdag.db"
      }
    }
  }
}
```

## CLI usage

### 1. Record a decision

```bash
memdag record \
  --kind decision \
  --title "Adopt SQLite WAL + FTS5" \
  --body "Using SQLite in WAL mode gives safe concurrent multi-reader access without daemons." \
  --tags "sqlite wal concurrency architecture" \
  --id "DEC-2026-001"
```

### 2. Supersede an outdated decision

```bash
memdag record \
  --kind decision \
  --title "Replace Flat Markdown with memdag" \
  --body "CLAUDE.md was causing zombie context conflicts. Migrating all project invariants to memdag." \
  --tags "memory dag migration" \
  --supersedes "DEC-2026-001"
```

### 3. Link dependencies and blockers

```bash
memdag link "TSK-002" "DEC-2026-001" "depends_on"
memdag link "BLK-001" "TSK-002" "blocks"
```

### 4. Search with 1-hop DAG expansion

```bash
memdag search "concurrency wal"
```

Example output:

```markdown
Found 1 relevant active memory/DAG entries:

### 1. [DEC-2026-001] Adopt SQLite WAL + FTS5 (kind: decision, status: active)
- Tags: sqlite wal concurrency architecture
- Body: Using SQLite in WAL mode gives safe concurrent multi-reader access without daemons.
- Relations (1-Hop DAG):
  - depends_on → [TSK-002] Verify lock handling (active)
```

### 5. Check database statistics

```bash
memdag stats
```

### 6. Resolve or clean up

```bash
memdag resolve "TSK-002" --note "Fixed by storing JoinSet and aborting on drop."
```

Ephemeral memories older than 24 hours are automatically swept on CLI runs unless you override `MEMDAG_EPHEMERAL_TTL_SECS`.

## Testing

Run the full test suite:

```bash
cargo test
```

This includes checks for:

- SQLite WAL behavior and foreign key integrity
- atomic supersession and DAG state propagation
- FTS5 BM25 search and 1-hop relation joins
- session consolidation and ephemeral cleanup
- sqlite-vec initialization and vector storage
- MCP stdio request and response flows

## License

Licensed under either of:

- MIT License ([LICENSE](LICENSE))
- Apache License, Version 2.0 ([LICENSE](LICENSE))
