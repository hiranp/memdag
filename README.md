# memdag

> **Local-First Relational DAG & FTS5 Memory Engine for AI Coding Harnesses**
> Built 100% in Rust with SQLite WAL, BM25 Full-Text Search, DAG supersession, embedded `sqlite-vec` vector support, and standard Model Context Protocol (MCP) over Stdio.

[![Rust](https://img.shields.io/badge/rust-2024%20edition-orange.svg)](https://www.rust-lang.org/)
[![SQLite](https://img.shields.io/badge/sqlite-WAL%20%2B%20FTS5-blue.svg)](https://www.sqlite.org/)
[![sqlite-vec](https://img.shields.io/badge/sqlite--vec-v0.1.9-green.svg)](https://github.com/asg017/sqlite-vec)
[![MCP](https://img.shields.io/badge/protocol-MCP%20Stdio-purple.svg)](https://modelcontextprotocol.io/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)

---

## 💡 Why memdag?

Modern AI coding harnesses (Antigravity, Claude Code, VSCode, Cursor) struggle with two extremes of agent memory:

1. **Heavy External Daemons (Letta / Mem0 / Zep)**:
   - Require background services, PostgreSQL, Docker containers, or Python sidecars.
   - Suffer from orphan processes, port conflicts, cold-start latency, and workspace switch breakage.
2. **Naive Flat Stores (CLAUDE.md / flat key-value)**:
   - Suffer from **Zombie Context**: outdated architectural decisions conflict with new ones.
   - Suffer from token bloat and lack relational dependencies.
3. **Pure Dense Vector Stores**:
   - Embeddings often hallucinate or blur semantic lines between exact technical identifiers (e.g. `mistral.rs`, `sqlite-vec`, `E0502`, `usearch`), whereas codebases and bug traces require deterministic recall.

### The memdag Solution:

- ⚡ **Zero Background Daemons**: Single-file SQLite database configured with `PRAGMA journal_mode = WAL;` and `PRAGMA busy_timeout = 5000;`, giving multiple concurrent agent processes safe, high-speed multi-reader / single-writer access.
- 🎯 **Deterministic FTS5 BM25 Recall**: Tokenized with `porter unicode61` stemming to match exact compiler errors, crate names, symbols, and architectural phrases instantly.
- 🕸️ **Tackling Zombie Context via DAG Supersession**: Explicitly tracks directed relationships (`supersedes`, `depends_on`, `blocks`, `references`). When an agent updates a decision, the superseded memory is atomically marked as superseded and pruned from default context retrieval.
- 🔍 **1-Hop DAG Context Expansion**: Searching retrieves matching active entities plus their immediate dependencies, blockers, and parent decisions in a single relational join.
- 🧬 **Embedded `sqlite-vec`**: Native in-process vector embeddings support without external C extensions or separate daemon processes.
- 🧹 **Ephemeral Session Cleanup**: Observations from aborted or scratch sessions are marked `ephemeral` by default and automatically purged or consolidated into permanent invariants during session wrap-up.

---

## 🏛️ Architecture & Database Schema

```
┌────────────────────────────────────────────────────────┐
│               AI Coding Harness (Claude/Cursor)       │
└───────────────────────────┬────────────────────────────┘
                            │ Stdio MCP JSON-RPC
┌───────────────────────────▼────────────────────────────┐
│                    memdag (Rust)                       │
│  ┌──────────────────────────────────────────────────┐  │
│  │   MCP Tools: record, link, search, consolidate   │  │
│  └──────────────────────────┬───────────────────────┘  │
│                             │                          │
│  ┌──────────────────────────▼───────────────────────┐  │
│  │   SQLite (WAL + busy_timeout=5000 + FKs ON)      │  │
│  │                                                  │  │
│  │   [ memories ] ◄──────► [ memory_relations ]    │  │
│  │        ▲                 (supersedes, depends,   │  │
│  │        │                  blocks, references)    │  │
│  │   Sync Triggers                                  │  │
│  │        │                                         │  │
│  │   [ memories_fts ] (FTS5 BM25 + Porter)          │  │
│  │   [ memories_vec ] (sqlite-vec float[384])       │  │
│  └──────────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────────┘
```

### SQLite Schema

```sql
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;

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

-- Relational Directed Graph (Supersession, Dependency, Blocking)
CREATE TABLE IF NOT EXISTS memory_relations (
    source_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation_type TEXT NOT NULL CHECK(relation_type IN ('supersedes', 'depends_on', 'blocks', 'references')),
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (source_id, target_id, relation_type)
);

-- FTS5 Virtual Table for BM25 Search
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    id UNINDEXED,
    title,
    body,
    tags,
    content='memories',
    content_rowid='rowid',
    tokenize='porter unicode61'
);

-- Sync Triggers (AFTER INSERT, DELETE, UPDATE)
-- Optional Vector Table with sqlite-vec
CREATE VIRTUAL TABLE IF NOT EXISTS memories_vec USING vec0(
    id TEXT PRIMARY KEY,
    embedding float[384]
);
```

---

## 🛠️ MCP Tool Interface

`memdag` implements the standard Model Context Protocol (MCP) over Stdio, providing 4 core endpoints:

| Tool | Parameters | Description |
|---|---|---|
| `record_memory` | `kind`, `title`, `body`, `tags`, `supersedes_id`, `session_id`, `id` | Inserts a new memory node. If `supersedes_id` is supplied, wraps the write in an atomic transaction: sets `memories.status = 'superseded'` on the previous ID, inserts the new memory as `active`, and creates a `supersedes` relation edge. |
| `link_entities` | `source_id`, `target_id`, `relation_type` | Inserts a directed edge (`depends_on`, `blocks`, `references`, `supersedes`) into the DAG. |
| `search_memory` | `query`, `kind` (optional), `include_resolved` (bool), `limit` (int) | Runs BM25 FTS5 search with 1-hop DAG expansion, formatting the output for immediate, high-density LLM context injection. |
| `consolidate_session` | `session_id`, `learnings`, `purge_ephemeral` (bool) | Atomically registers permanent learnings while purging or archiving ephemeral observations from the session. |

---

## 🚀 Installation & Setup

### Build from Source

```bash
git clone https://github.com/hiranp/memdag.git
cd memdag
cargo build --release
```

The optimized binary will be located at `target/release/memdag`.

### Configure with Claude Code

Add `memdag` to your Claude Code MCP configuration (`~/.claude.json` or run `claude mcp add`):

```bash
claude mcp add memdag -- /path/to/memdag serve
```

Or configure directly in `claude.json`:

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

### Configure with Cursor / VSCode MCP

Add to your `mcp.json`:

```json
{
  "mcpServers": {
    "memdag": {
      "command": "memdag",
      "args": ["serve"]
    }
  }
}
```

---

## 💻 CLI Usage

You can also interact directly with `memdag` from the terminal:

### 1. Record a Decision
```bash
memdag record \
  --kind decision \
  --title "Adopt SQLite WAL + FTS5" \
  --body "Using SQLite in WAL mode gives safe concurrent multi-reader access without daemons." \
  --tags "sqlite wal concurrency architecture" \
  --id "DEC-2026-001"
```

### 2. Supersede an Outdated Decision
```bash
memdag record \
  --kind decision \
  --title "Replace Flat Markdown with memdag" \
  --body "CLAUDE.md was causing zombie context conflicts. Migrating all project invariants to memdag." \
  --tags "memory dag migration" \
  --supersedes "DEC-2026-001"
```

### 3. Link Dependencies & Blockers
```bash
memdag link "TSK-002" "DEC-2026-001" "depends_on"
memdag link "BLK-001" "TSK-002" "blocks"
```

### 4. Search with 1-Hop DAG Expansion
```bash
memdag search "concurrency wal"
```
**Output:**
```markdown
Found 1 relevant active memory/DAG entries:

### 1. [DEC-2026-001] Adopt SQLite WAL + FTS5 (kind: decision, status: active)
- **Tags**: sqlite wal concurrency architecture
- **Body**: Using SQLite in WAL mode gives safe concurrent multi-reader access without daemons.
- **Relations (1-Hop DAG)**:
  - `depends_on` ➔ [TSK-002] Verify lock handling (active)
```

### 5. Check Database Statistics
```bash
memdag stats
```

---

## 🧪 Testing

Run the full integration test suite:

```bash
cargo test
```

Includes tests for:
- SQLite WAL pragmas and foreign key constraints
- Atomic supersession and DAG status propagation
- FTS5 BM25 search and 1-hop relation joining
- Session consolidation and ephemeral memory purging
- `sqlite-vec` extension initialization and vector storage
- MCP Stdio JSON-RPC request/response protocol

---

## 📄 License

Licensed under either of:
- MIT License ([LICENSE-MIT](LICENSE) or http://opensource.org/licenses/MIT)
- Apache License, Version 2.0 (http://www.apache.org/licenses/LICENSE-2.0)
