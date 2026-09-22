# memdag

![memdag logo](memdag-logo.svg)

> Local-first relational DAG and FTS5 memory engine for AI coding harnesses.

[![Rust](https://img.shields.io/badge/rust-2024%20edition-orange.svg)](https://www.rust-lang.org/)
[![SQLite](https://img.shields.io/badge/sqlite-WAL%20%2B%20FTS5-blue.svg)](https://www.sqlite.org/)
[![sqlite-vec](https://img.shields.io/badge/sqlite--vec-v0.1.9-green.svg)](https://github.com/asg017/sqlite-vec)
[![MCP](https://img.shields.io/badge/protocol-MCP%20Stdio-purple.svg)](https://modelcontextprotocol.io/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)

memdag is a local, durable memory layer for AI coding agents. It stores memories as a directed graph, supports full-text retrieval, keeps decisions and context synchronized, and avoids the operational cost of background daemons and fragile flat-note systems.

## Why memdag

Modern AI coding workflows tend to fail in one of three ways:

1. **Heavy external memory systems**: Require background services, databases, Docker, and extra runtime complexity, introducing startup friction and drift across sessions.
2. **Flat markdown or key-value stores**: Cause zombie context and stale decisions to compete with current ones, resulting in low-signal retrieval and poor dependency tracking.
3. **Pure vector-only stores**: Blur exact technical identifiers and compiler error signatures while missing the relational semantics codebases depend on.

memdag combines the strengths of each approach into a single in-process, SQLite-backed engine:

- **WAL-backed SQLite**: Safe concurrent access across multiple agent processes without background daemons.
- **FTS5 BM25 search**: Deterministic retrieval of symbols, error signatures, and architectural phrases.
- **Relational DAG semantics**: Explicit memory supersession and causal dependency tracking (`depends_on`, `blocks`, `references`).
- **Bidirectional 1-hop expansion**: Automatically retrieves related incoming (`◄`) and outgoing (`➔`) graph edges with search results.
- **Embedded vector similarity**: Native `sqlite-vec` KNN embeddings without an external vector database.
- **Ephemeral session lifecycle**: Automatic TTL sweeping keeps scratch context from polluting long-term memory.

---

## Architecture Overview

```text
AI coding harness (Antigravity / Claude / Cursor / VSCode)
        │
        │ Stdio MCP JSON-RPC 2.0
        ▼
      memdag (Rust In-Process Engine)
        │
        ├─ MCP Tools & CLI Controllers
        │
        ▼
    SQLite Storage Layer (WAL + Busy Timeout 5000ms)
        ├─ memories (Nodes: decisions, tasks, invariants, blockers)
        ├─ memory_relations (Edges: supersedes, depends_on, blocks, references)
        ├─ memories_fts (FTS5 BM25 with porter unicode61 tokenizer)
        └─ vec_memories (sqlite-vec float[384] cosine distance)
```

> 📖 **Deep Dive**: For the full database schema, SQLite triggers, graph traversal algorithms, and concurrency benchmarks, see the **[Architecture & Technical Design Document](docs/DESIGN.md)**.

---

## MCP Tool Interface

memdag exposes a standard Model Context Protocol (MCP) surface over stdio:

| Tool | Parameters | Description |
| --- | --- | --- |
| `record_memory` | `kind`, `title`, `body`, `tags`, `supersedes_id`, `session_id`, `id` | Inserts a new memory node and atomically supersedes outdated decisions. |
| `link_entities` | `source_id`, `target_id`, `relation_type` | Inserts a directed edge (`depends_on`, `blocks`, `references`, `supersedes`). |
| `search_memory` | `query`, `kind`, `include_resolved`, `limit` | BM25 full-text search with 1-hop bidirectional DAG expansion. |
| `search_vector` | `embedding`, `limit` | Dense vector KNN similarity search with 1-hop DAG expansion. |
| `get_memory` | `id` | Fetches a specific memory node with all incoming and outgoing relations. |
| `list_memories` | `kind`, `status`, `limit` | Lists active, superseded, or resolved memories. |
| `resolve_memory` | `id`, `note` | Marks tasks or blockers as resolved with a completion note. |
| `consolidate_session` | `session_id`, `learnings`, `purge_ephemeral` | Promotes session learnings and prunes ephemeral scratchpads. |

---

## Installation

### Build from source

```bash
git clone https://github.com/hiranp/memdag.git
cd memdag
cargo build --release
```

The optimized binary is built at `target/release/memdag`.

### Install MCP Client Configuration

`memdag mcp install` automatically registers the server in your client configuration:

```bash
# Project-local (.mcp.json for Claude Code, Cursor, Windsurf)
memdag mcp install

# User-global (~/.claude.json)
memdag mcp install --global

# Custom path (e.g., Antigravity, VSCode)
memdag mcp install --path ~/.gemini/config/mcp_config.json
```

To uninstall:

```bash
memdag mcp uninstall --global
```

### Manual Configuration

To configure manually, add `memdag` to your client's `mcpServers` block:

```json
{
  "mcpServers": {
    "memdag": {
      "command": "/path/to/memdag",
      "args": ["serve"],
      "env": {
        "MEMDAG_DB": "/Users/username/.config/quantized-tracking/memdag.db"
      }
    }
  }
}
```

---

## CLI Usage

### 1. Record an architectural decision
```bash
memdag record \
  --kind decision \
  --title "Adopt SQLite WAL + FTS5" \
  --body "Using SQLite in WAL mode provides safe concurrent multi-reader access without daemons." \
  --tags "sqlite wal concurrency architecture" \
  --id "DEC-2026-001"
```

### 2. Atomically supersede an outdated decision
```bash
memdag record \
  --kind decision \
  --title "Replace Flat Markdown Context with memdag" \
  --body "CLAUDE.md caused zombie context conflicts. Migrating project invariants to memdag." \
  --tags "memory dag migration" \
  --supersedes "DEC-2026-001"
```

### 3. Link dependencies and blockers
```bash
memdag link "TSK-002" "DEC-2026-001" "depends_on"
memdag link "BLK-001" "TSK-002" "blocks"
```

### 4. Search with 1-hop bidirectional DAG expansion
```bash
memdag search "concurrency wal"
```

Example output:
```markdown
Found 1 relevant active memory/DAG entries:

### 1. [DEC-2026-001] Adopt SQLite WAL + FTS5 (kind: decision, status: active)
- **Tags**: sqlite wal concurrency architecture
- **Body**: Using SQLite in WAL mode provides safe concurrent multi-reader access without daemons.
- **Relations (1-Hop DAG)**:
  - `depended_on_by` ◄ [TSK-002] Verify lock handling (active)
```

### 5. Resolve tasks and check stats
```bash
memdag resolve "TSK-002" --note "Verified concurrent read/write test under load."
memdag stats
```

Ephemeral memories older than 24 hours are swept automatically on CLI runs (customizable via `MEMDAG_EPHEMERAL_TTL_SECS`).

---

## Testing

Run the full integration test suite:

```bash
cargo test
```

Verifies:
- SQLite WAL pragmas and foreign key cascading constraints
- Atomic supersession transactions and DAG state propagation
- FTS5 BM25 search and bidirectional 1-hop relation joining
- Session consolidation and ephemeral memory TTL purging
- `sqlite-vec` extension registration and KNN search
- MCP JSON-RPC 2.0 handshake, tool dispatch, and silent notification compliance

---

## License

Licensed under either of:

- MIT License ([LICENSE](LICENSE))
- Apache License, Version 2.0 ([LICENSE](LICENSE))
