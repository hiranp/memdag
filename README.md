# memdag

![memdag logo](memdag-logo.svg)

> Local-first relational DAG memory engine and dependency-aware task tracker for AI coding harnesses.

[![Rust](https://img.shields.io/badge/rust-2024%20edition-orange.svg)](https://www.rust-lang.org/)
[![SQLite](https://img.shields.io/badge/sqlite-WAL%20%2B%20FTS5-blue.svg)](https://www.sqlite.org/)
[![sqlite-vec](https://img.shields.io/badge/sqlite--vec-v0.1.9-green.svg)](https://github.com/asg017/sqlite-vec)
[![MCP](https://img.shields.io/badge/protocol-MCP%20Stdio-purple.svg)](https://modelcontextprotocol.io/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)

memdag is a local, durable memory layer for AI coding agents. It stores memories as a directed graph, supports full-text retrieval, keeps decisions and context synchronized, and avoids the operational cost of background daemons and fragile flat-note systems. `task`/`blocker` memory kinds plus dependency edges and a computed [ready queue](#task-tracking) also make it a lightweight dependency-aware task tracker on the same schema — see [Task Tracking](#task-tracking).

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
- **Dependency-aware task tracking, for free**: `task`/`blocker` kinds plus `depends_on`/`blocks` edges and `resolve_memory` give you lightweight, DAG-aware task tracking on the same schema — no separate issue tracker for agent-scoped work. See [Task Tracking](#task-tracking) below, and [Scope & Limitations](#scope--limitations) for where this stops being enough.

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
| `list_ready` | `limit` | Lists claimable tasks/blockers: active, with no incoming active `blocks` edge. |
| `resolve_memory` | `id`, `note` | Marks tasks or blockers as resolved with a completion note. |
| `consolidate_session` | `session_id`, `learnings`, `purge_ephemeral` | Promotes session learnings and prunes ephemeral scratchpads. |

---

## Installation

### Download a prebuilt binary

Each tagged release (`vX.Y.Z`) publishes binaries for Linux, macOS (Intel + Apple Silicon),
and Windows via [GitHub Actions](.github/workflows/release.yml) — see the
[Releases](https://github.com/hiranp/memdag/releases) page.

Extract it somewhere stable and on `PATH`, not left in `~/Downloads` or a temp directory —
both the CLI and the MCP config `memdag mcp install` writes reference this exact file path:

```bash
# Linux / macOS
mkdir -p ~/.local/bin
mv memdag ~/.local/bin/
chmod +x ~/.local/bin/memdag
# ensure ~/.local/bin is on PATH (add to ~/.bashrc / ~/.zshrc if not):
export PATH="$HOME/.local/bin:$PATH"
```

`memdag mcp install` warns if it's run from a Downloads/tmp/build directory or a path that
isn't on `PATH`, since moving or deleting that file later breaks the registered MCP entry.

### Build from source

```bash
git clone https://github.com/hiranp/memdag.git
cd memdag
cargo build --release
```

The optimized binary is built at `target/release/memdag` — move it to `~/.local/bin` (see
above) before running `memdag mcp install`, rather than registering it straight out of
`target/release/`.

### Install MCP Client Configuration

`memdag mcp install` automatically registers the server in your client configuration. Only
Claude Code reads `.mcp.json` / `~/.claude.json` directly, so that's the zero-flag default;
every other client keeps its own file, location, and (for VS Code) top-level JSON key —
`install`/`uninstall` only ever touch one key in whatever file you point `--path` at via
`--key`, so it's safe on a config shared with other servers.

| Client | Command | Notes |
|---|---|---|
| **Claude Code** (project) | `memdag mcp install` | writes `./.mcp.json`, key `mcpServers` |
| **Claude Code** (user) / **Claude Desktop** | `memdag mcp install --global` | macOS/Linux: `~/.claude.json`; Windows: `%APPDATA%\Claude\claude_desktop_config.json` for Desktop |
| **Codex CLI** | manual — see below | `~/.codex/config.toml`, TOML table `[mcp_servers.memdag]`, not JSON |
| **Cursor** (project) | `memdag mcp install --path .cursor/mcp.json` | key `mcpServers` |
| **Cursor** (user) | `memdag mcp install --path ~/.cursor/mcp.json` | key `mcpServers` |
| **VS Code** (workspace) | `memdag mcp install --path .vscode/mcp.json --key servers` | VS Code uses `servers`, not `mcpServers` — `--key` is required here |
| **VS Code** (user profile) | `memdag mcp install --path <user-mcp.json> --key servers` | Linux: `~/.config/Code/User/mcp.json`; macOS: `~/Library/Application Support/Code/User/mcp.json`; Windows: `%APPDATA%\Code\User\mcp.json`. Easiest: run **MCP: Open User Configuration** in VS Code once (creates the file), note the path it opens, then run the command above |
| **Windsurf** | `memdag mcp install --path ~/.codeium/windsurf/mcp_config.json` | key `mcpServers` |
| **Antigravity** (global, works today) | `memdag mcp install --path ~/.gemini/config/mcp_config.json` | key `mcpServers`; the project-local `.antigravitycli/mcp_config.json` file uses the same shape but [isn't actually loaded yet](https://github.com/google-antigravity/antigravity-cli/issues/60) as of Antigravity CLI v1.0.0 |

pi has no MCP client at all, by design (see [pi's README](https://github.com/earendil-works/pi-coding-agent) for the
rationale) — use the Skill below instead.

Codex stores config as TOML, not JSON, so `mcp install`'s JSON merge doesn't apply. Either
run `codex mcp add memdag -- /path/to/memdag serve`, or add by hand:

```toml
# ~/.codex/config.toml
[mcp_servers.memdag]
command = "/path/to/memdag"
args = ["serve"]
```

To uninstall (same `--path`/`--key` flags as install):

```bash
memdag mcp uninstall --global
```

### Will agents actually call these tools?

The `initialize` response includes an `instructions` field telling the agent to call
`search_memory` before starting a task, `record_memory`/`link_entities` at decision points,
`list_ready` before claiming task/blocker work, and `consolidate_session` before ending a
session. Clients that surface MCP server instructions to the model (Claude Code, Claude
Desktop) pick this up automatically — no project hooks or extra setup required.

**Most other MCP clients don't render the `instructions` field at all**, so the agent only
sees each tool's one-line `description`. Those are self-explanatory enough for basic use, but
for reliable task-tracking behavior (using `kind='task'`/`kind='blocker'`, calling `list_ready`
before claiming work) on a client that ignores `instructions`, paste this into your project's
`AGENTS.md`/`CLAUDE.md`:

```markdown
This project uses memdag (MCP) for durable memory and task tracking.

- Call `search_memory` before starting a task to recall past decisions/invariants/blockers.
- Call `record_memory` for decisions/invariants worth remembering, and for tasks
  (kind='task') and things blocking them (kind='blocker').
- Call `link_entities(blocker_id, task_id, 'blocks')` to connect a blocker to a task.
- Call `list_ready` to find claimable, unblocked tasks/blockers — don't just scan
  `list_memories` and eyeball it, especially if other agents may be working on this project.
- Call `resolve_memory` when a task or blocker is done.
- Call `consolidate_session` before ending a session to promote durable learnings and
  purge scratch/ephemeral notes.
```

**Or install it as a Skill.** [`.agents/skills/memdag/`](.agents/skills/memdag/SKILL.md) is the
canonical location for the shared [Agent Skills standard](https://agentskills.io/specification):
**pi**, **Codex CLI**, and **VS Code Copilot** all discover `.agents/skills/` in your repo
automatically — no setup needed, nothing to symlink. **Claude Code** looks in `.claude/skills/`
instead, so that's a symlink (already committed in this repo, `.claude/skills/memdag ->
../../.agents/skills/memdag`; do the same in your own project):

```bash
mkdir -p .claude/skills
ln -s "$(pwd)/.agents/skills/memdag" .claude/skills/memdag
```

Antigravity supports `SKILL.md` too, but its exact discovery directory wasn't confirmed at
the time of writing — check its `/skills` panel for the workspace/global paths it's actually
scanning and symlink `.agents/skills/memdag` there if needed.

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

## Task Tracking

`task` and `blocker` are memory kinds like any other, so `depends_on`/`blocks` edges plus
`resolve_memory` give you a small dependency-aware task tracker on the same schema — no
separate issue tracker for agent-scoped work:

```bash
# Create a task and a blocker, and link them
memdag record --kind task --title "Ship v0.2 release automation" --id "TSK-100"
memdag record --kind blocker --title "CI lacks Windows runner" --id "BLK-100"
memdag link "BLK-100" "TSK-100" "blocks"

# See what's still open
memdag list --kind task --status active

# Resolve the blocker, then the task
memdag resolve "BLK-100" --note "Added windows-latest to the release matrix."
memdag resolve "TSK-100" --note "Shipped in v0.2.0."

# Find claimable work: active tasks/blockers with no incoming active `blocks` edge
memdag ready
```

`get`/`get_memory` on `TSK-100` shows the blocker as an incoming `blocked_by` relation until it's
resolved. `memdag ready` (MCP: `list_ready`) computes the unblocked subset directly in SQL, so
multiple agents/subagents working the same project can each call it instead of listing every
task and checking `blocked_by` one by one — note it's not an atomic claim, so two agents can
still both pick the same ready item; resolve/coordinate out of band if that matters for your
workflow.

### Scope & Limitations

memdag is a **single-machine, single-writer** SQLite store. It is not a replacement for a
distributed, multi-agent task tracker like [beads](https://github.com/steveyegge/beads):

| | memdag | beads |
| --- | --- | --- |
| Storage | Local SQLite file | Dolt (versioned SQL, git-synced) |
| Cross-machine sync | None (copy/sync the file yourself) | `bd dolt push`/`pull` across git remotes |
| Concurrent agents | Safe reads/writes on one machine (WAL) | Atomic `--claim` (assignee + in_progress) prevents two agents grabbing the same task |
| Ready queue | `memdag ready` / `list_ready` (not atomic — no claim) | `bd ready` + atomic `--claim` |
| Priority / epics / sub-tasks | Not modeled (encode in `tags` if needed) | First-class fields and hierarchical IDs |

If you need agents on different machines or git branches claiming and syncing work without
colliding, use beads. If you want one lightweight, zero-daemon store for a single agent's
decisions, invariants, and tasks on one machine, memdag covers it.

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
