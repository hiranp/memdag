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
- **Worktree-aware**: Agents in different git worktrees of a repository share one database, so they see the same tasks and blockers. Agents on other machines share only what is committed to git (see [Scope & Limitations](#scope--limitations)).
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
| `record_memory` | `kind`, `title`, `body`, `tags`, `supersedes_id`, `session_id`, `id`, `paths`, `commit` | Inserts a new memory node and atomically supersedes outdated decisions. `paths` ties it to files so it can be flagged STALE later. |
| `link_entities` | `source_id`, `target_id`, `relation_type` | Inserts a directed edge (`depends_on`, `blocks`, `references`, `supersedes`). |
| `search_memory` | `query`, `kind`, `include_resolved`, `limit`, `max_tokens` | BM25 full-text search (fused with vector search when an embedder is set) with 1-hop bidirectional DAG expansion, packed into a token budget. |
| `search_vector` | `query` or `embedding`, `limit` | Dense vector KNN similarity search with 1-hop DAG expansion. Only listed when an embedder is configured. |
| `get_memory` | `id` | Fetches a specific memory node with all incoming and outgoing relations. |
| `list_memories` | `kind`, `status`, `limit` | Lists active, superseded, or resolved memories. |
| `list_ready` | `limit` | Lists claimable tasks/blockers: active, with no incoming active `blocks` edge. |
| `resolve_memory` | `id`, `note` | Marks tasks or blockers as resolved with a completion note. |
| `consolidate_session` | `session_id`, `learnings`, `purge_ephemeral` | Promotes session learnings and prunes ephemeral scratchpads. |
| `get_work` | `task` (`expand`, `verify` or `consolidate`), `limit` | Hands out a batch of bulk upkeep, with the instructions for it, to run on a cheap model. See [Cheap-model workers](#cheap-model-workers). |
| `submit_work` | `task`, `results` or `reply` | Applies the worker's answers: search keywords, or confirm/correct STALE memories. |

MCP prompts (slash commands in clients that support them, e.g. `/mcp__memdag__verify_stale` in
Claude Code, `/memdag:verify_stale` in OpenCode): `expand_keywords`, `verify_stale` and
`consolidate_memories` run those tasks end to end on a cheap subagent.

---

## Installation

### Download a prebuilt binary

Each tagged release (`vX.Y.Z`) publishes binaries for Linux, macOS (Intel + Apple Silicon),
and Windows via [GitHub Actions](.github/workflows/release.yml) — see the
[Releases](https://github.com/hiranp/memdag/releases) page.

Extract it somewhere stable and on `PATH`, not left in `~/Downloads` or a temp directory —
both the CLI and the configs `memdag install` writes reference this exact file path:

```bash
# Linux / macOS
mkdir -p ~/.local/bin
mv memdag ~/.local/bin/
chmod +x ~/.local/bin/memdag
# ensure ~/.local/bin is on PATH (add to ~/.bashrc / ~/.zshrc if not):
export PATH="$HOME/.local/bin:$PATH"
```

`memdag install` warns if it's run from a Downloads/tmp/build directory or a path that
isn't on `PATH`, since moving or deleting that file later breaks the registered MCP entry.

### Build from source

```bash
git clone https://github.com/hiranp/memdag.git
cd memdag
cargo build --release
```

The optimized binary is built at `target/release/memdag` — move it to `~/.local/bin` (see
above) before running `memdag install`, rather than registering it straight out of
`target/release/`.

### Connect your agents: `memdag install`

```bash
memdag install                 # harnesses this project already uses (.claude/, opencode.json, .cursor/, ...)
memdag install --global        # harnesses installed for your user, in their user-level configs
memdag install --target claude-code,opencode --model <cheap model>
memdag install --print-config  # show what would be written; change nothing
memdag install --refresh       # after upgrading: update only harnesses already set up
memdag uninstall               # remove all of it (only what memdag wrote)
```

For each harness it writes:

- **the MCP server entry**, edited in place. Comments, formatting and other servers in the file
  are kept, and a file memdag can't parse is refused, not rewritten.
- **the `memdag-worker` subagent** on a cheap model, for [cheap-model workers](#cheap-model-workers).
- **instructions**: a marked `<!-- MEMDAG_START -->` block in the harness's instructions file,
  or a rule/steering file of memdag's own.
- **the memdag skill** (`SKILL.md`), in a skills folder the harness reads.
- **approvals** for the worker's two tools, where a file can grant them.

Each part can be skipped: `--no-mcp`, `--no-worker`, `--no-instructions`, `--no-skill`,
`--no-permissions`. `--hooks` also adds a Claude Code SessionStart hook
(`memdag hook session-start`), which mentions memories waiting for upkeep.

| `--target` | MCP config (project / `--global`) | Worker | Instructions | Skill |
|---|---|---|---|---|
| `claude-code` | `.mcp.json` / `~/.claude.json` | `.claude/agents/` (model: haiku) | `CLAUDE.md` | `.claude/skills/` |
| `opencode` | `opencode.json[c]` / `~/.config/opencode/` (V2 `mcp.servers`, `codemode: false`) | `.opencode/agents/` | `AGENTS.md` | `.agents/skills/` |
| `codex` | `.codex/config.toml` / `~/.codex/config.toml` | `.codex/agents/*.toml` | `AGENTS.md` | `.agents/skills/` |
| `cursor` | `.cursor/mcp.json` / `~/.cursor/mcp.json` | `.cursor/agents/` | `.cursor/rules/memdag.mdc` (project) | `.agents/skills/` |
| `gemini` | `.gemini/settings.json` / `~/.gemini/settings.json` | `.gemini/agents/` | `GEMINI.md` | `.agents/skills/` |
| `vscode` (Copilot) | `.vscode/mcp.json` / VS Code user `mcp.json` (`servers`) | `.github/agents/` | `.github/copilot-instructions.md` | `.agents/skills/` |
| `kiro` | `.kiro/settings/mcp.json` / `~/.kiro/settings/` (`autoApprove`) | — | `.kiro/steering/memdag.md` | `.kiro/skills/` |
| `antigravity` | `.agents/mcp_config.json` / `~/.gemini/config/mcp_config.json` | — | `.agents/rules/memdag.md` | `.agents/skills/` |

Notes:

- **Command path.** Project configs say plain `memdag` when that resolves to the same binary
  on `PATH`, so a committed config works for every collaborator. Global configs, and
  Antigravity (whose macOS app doesn't see the shell's `PATH`), use the absolute path.
- **Shared files.** Files several harnesses use (one `AGENTS.md`, one `.agents/skills/memdag`)
  are kept on uninstall while another harness still has memdag configured.
- **Detection.** For a project, `--target auto` goes only by markers in the project (a
  `.claude/` or `CLAUDE.md`, `opencode.json`, `.cursor/`, `.vscode/mcp.json`, ...), never by
  what's installed on your machine, so it doesn't add files for harnesses your teammates
  don't use. Name harnesses with `--target` to set up new ones.
- **Re-running is safe.** memdag owns only `type`, `command`, `args` and its own environment
  variable in a server entry; other variables (a pinned `MEMDAG_DB`), timeouts, flags you
  turned off, and tool lists stay. A worker keeps its pinned model unless `--model` is given.
  `--refresh` rewrites only the parts a harness already has.
- **OpenCode.** OpenCode V2 reads the V1 shape too, but V1 can't read V2's. By default an
  existing entry keeps its shape and a new one is V2; `--opencode-format v2` migrates a V1
  entry, `--opencode-format v1` writes V1.
- **Codex.** Codex reads a project's `.codex/config.toml` only once the project is trusted.
- **Worker model.** Only Claude Code has a model alias to default to (`haiku`). Elsewhere,
  pass `--model` with a Haiku- or Flash-class model id, or the worker uses the harness's
  default model.

`memdag mcp install --client <target>` writes only the MCP entry. With `--path`/`--key` it
writes it into any file, which covers clients without a target, e.g. Windsurf:
`memdag mcp install --path ~/.codeium/windsurf/mcp_config.json`. pi has no MCP client by design;
use the skill below instead.

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

**The skill.** `memdag install` puts the memdag skill
([`.agents/skills/memdag/SKILL.md`](.agents/skills/memdag/SKILL.md), shipped inside the binary)
where each harness looks for skills: `.agents/skills/` for the
[Agent Skills standard](https://agentskills.io/specification) (Codex, Cursor, Gemini CLI,
Copilot, OpenCode, Antigravity), `.claude/skills/` for Claude Code and `.kiro/skills/` for
Kiro. pi reads `.agents/skills/` too. A skill folder memdag didn't write (like this repo's own
`.claude/skills/memdag` symlink) is left alone.

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

### 6. Export to JSON
```bash
memdag export                       # pretty JSON to stdout
memdag export ./memdag-export.json  # write to a file
```
Dumps every memory and relation as plain JSON (`{ "memories": [...], "relations": [...] }`).
The SQLite file at `default_db_path()` (see [Where memdag stores its database](#where-memdag-stores-its-database)) remains the source of truth; this is a point-in-time sidecar for
committing a snapshot to git, diffing across sessions, or migrating data to another machine —
there's no importer, so treat it as read-only output.

---

## Where memdag stores its database

`default_db_path()` resolves, in order:

1. `$MEMDAG_DB`, if set.
2. The project's database, found by walking up from the current directory:
   - In a git repository, one database for the whole repository:
     `<main checkout>/.memdag/memdag.db`, or `<repo>.git/memdag/memdag.db` for a bare
     repository. Every worktree uses the same one (see below).
   - A directory with its own `.memdag/` (for example a nested crate) uses
     `<dir>/.memdag/memdag.db`.

   Projects stay isolated from each other: each has its own memories, tasks and `list_ready`
   queue.
3. The OS-standard application data directory (e.g. `~/.local/share/memdag/memdag.db` on Linux),
   if no project root is found.

`.memdag/` is gitignored by default — the database itself isn't meant to be committed. If you
want a project's memory to travel with the repo (code review, cross-machine sync), commit the
output of `memdag export` instead of the SQLite file.

### Several agents, several worktrees

All worktrees of a repository share one database, so agents working in different worktrees
(`git worktree add`) see the same tasks, blockers and links, and `list_ready` covers all of
them.

What comes from git-tracked files (notes, ADRs, `Decision:` commits) differs per branch.
memdag records which worktrees' checkouts contain each imported memory:

- An import only drops its own worktree's claim on a memory. The memory goes once no worktree
  has its source any more, so a branch that deletes a note doesn't remove it for the others.
- A memory whose note exists only on another worktree's branch is still found, but labelled
  `only in another worktree` (its branch may never be merged). It isn't checked for staleness
  against this checkout's files, and `verify` workers don't get it.
- When a worktree is removed (`git worktree remove`), its claims go with it on the next
  import.
- A note edited differently on two branches is stored once, so the version from the most
  recent import is the one shown. Notes are append-only, so this only happens with hand
  edits.

This is for worktrees on one machine. Agents on other machines, including cloud agents
working on their own clone, don't share the database; see
[Scope & Limitations](#scope--limitations).

Before 0.4, memdag kept a separate database in each linked worktree. The first time a worktree
opens the shared database, memdag merges the memories recorded there (tasks, blockers, links)
into it and renames the old file to `memdag.db.merged`. Imported rows are rebuilt from git. Set
`MEMDAG_DB` to opt out of all of this and use one explicit file.

Existing databases from before this project-scoping change (anywhere under the old global data
directory) are **not** auto-migrated — copy the file into `<repo>/.memdag/memdag.db` manually if
you want to keep that history.

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

### Provenance and stale memories

A decision about `src/store.rs` stops being trustworthy when `src/store.rs` changes. Record the
files a memory is about and memdag does the bookkeeping:

```bash
memdag record -k decision -t "Search packs results by token budget" -b "..." --paths src/store.rs
```

The memory is stamped with the current git `HEAD` and a content fingerprint (git blob hash) of
each cited path; with write-through, the note file records them as `paths:`, `commit:` and
`fingerprints:`. On every search and `get`, memdag recomputes the fingerprints; if a cited file's
content differs from what it was when the memory was recorded, the result carries a `STALE`
warning. `memdag stale` lists every active memory in that state. Because the baseline is
content, not the commit, recording a decision about a file you just edited is not stale, and
rebases, amends and shallow clones don't cause false alarms. Memories without `paths` cost
nothing.

### Token-budgeted recall

`search_memory` / `memdag search` pack results in rank order into an estimated token budget
(default 2000, about 4 characters per token; `max_tokens: 0` / `--max-tokens 0` disables it). The
top result is always returned (its body shortened if it alone is too big), at most six relations
are shown per result, and the reply says how many lower-ranked results were left out.

### Hybrid search (optional embedder)

memdag ships no embedding model. Point `MEMDAG_EMBED_CMD` at any command that reads text on
stdin and prints a 384-float JSON array (or `{"embedding": [...]}`) and search becomes
BM25 + vector, fused with Reciprocal Rank Fusion:

```bash
export MEMDAG_EMBED_CMD='my-embedder --dim 384'   # environment only, never memdag.toml
memdag embed                                      # backfill vectors for existing memories
```

New memories are embedded on write (best effort: an embedder outage never blocks recording).
If the command is missing or failing, search falls back to BM25. The command is read from the
environment only, so a cloned repository can't make memdag run anything. memdag records which
command produced the stored vectors; if you switch to a different command, the old vectors are
dropped (vectors from two models can't be compared) and `memdag embed` recomputes them.

### Cheap-model workers

Some upkeep is bulk text work that a small, cheap model does well. memdag never calls a model
itself and needs no API key. It hands out batches of work, with the instructions for them,
and your agent runs them on a Claude Haiku, Gemini Flash or similar subagent. Three tasks exist:

- **`expand`: search keywords.** For each memory, the worker writes the synonyms, related
  terms, abbreviations and error codes someone might search for. They go into the full-text
  index at half the weight of the memory's own text. So `lock contention` finds "Use WAL
  journal mode" with no embedding model. This is the lightweight alternative to
  `MEMDAG_EMBED_CMD`; you can use both.
- **`verify`: check STALE memories.** The worker reads the cited files that changed and answers
  for each memory:
  - `valid`: the files become the new baseline, so the memory is no longer stale. A
    write-through note gets a new note with the same content that supersedes it. A memory
    imported from a file without write-through is refused, because the next import would undo
    the change.
  - `outdated`, with a correction: the correction is recorded as a new memory that supersedes
    the old one, tagged `worker-corrected` with the worker's reason appended. The old text stays
    in history.
  - `unsure`: the memory stays flagged.

  Only active memories that are stale at submit time can change, and each id counts once per
  reply, so a repeated, late or invented answer does nothing. Likewise, keywords can only be
  written for memories still waiting for them.

- **`consolidate`: duplicates and conflicts.** memdag pairs active decisions, invariants and
  learnings whose text overlaps, and the worker judges each pair:
  - `duplicate`, naming the one to keep: the other is resolved as a duplicate (for a note, via
    its `.resolved.md` file, so this survives a rebuild) and linked to the kept one.
  - `conflict`: a task "Reconcile conflicting memories A and B" is recorded for a person or
    agent to settle; neither memory changes.
  - `related`: the two are linked.
  - `distinct`: nothing changes.

  Each pair is judged once. Only pairs memdag itself would offer are accepted.

**With an MCP client**, install the worker subagent once, then run the prompt:

```bash
memdag install --target claude-code                   # .claude/agents/memdag-worker.md (model: haiku) + CLAUDE.md block
memdag install --target opencode --model <provider>/<haiku-or-flash-model>   # .opencode/agents/ + AGENTS.md block
# Cursor, Gemini CLI, Codex and Copilot get a worker too (pass --model); see the install table
# add --global for user-level configs; `memdag uninstall` removes only what memdag wrote
```

The install also auto-approves the worker's two tools, `get_work` and `submit_work`, so a
background worker isn't stopped for approval on every call. In Claude Code that means
`mcp__memdag__get_work` and `mcp__memdag__submit_work` in `permissions.allow` of
`.claude/settings.json`. In OpenCode it is two `allow` rules in the worker file itself; agent
rules apply last, so they override an `ask` in your config for just those tools. Nothing else
is approved: reads, edits and shell keep your own rules. Use `--no-permissions` to skip this.

Then run `/mcp__memdag__expand_keywords` (Claude Code) or `/memdag:expand_keywords` (OpenCode),
or just ask for memdag upkeep. The agent hands the task to `memdag-worker`, which calls
`get_work` and `submit_work` itself, so your main model never reads the batch. The install also
adds a short block between `<!-- MEMDAG_START -->` and `<!-- MEMDAG_END -->` to `CLAUDE.md` or
`AGENTS.md`. Subagents read those files but not the instructions an MCP server sends when it
connects, so the block is how they learn to delegate.

**Headless, from the shell** (any model CLI that takes a prompt and prints the reply):

```bash
memdag work next expand | claude -p --model haiku | memdag work submit
memdag work next verify > batch.txt && claude -p "$(cat batch.txt)" --model haiku --allowedTools=Read | memdag work submit
```

`memdag work next` prints a ready-to-send prompt (`--json` for the raw batch). `memdag work
submit` accepts the reply as it comes back: code fences and stray prose around the JSON are
fine. Each item is validated on its own, so one bad answer is skipped without losing the rest.
Worker output gets the same secret check as any write, and keywords are cut down to short plain
terms. `memdag work prompt verify_stale` prints the full delegation prompt for clients without
MCP prompt support.

Keywords live only in the index. They are dropped when a memory's text is rewritten (which
puts it back in the queue) and are not part of a rebuild from git. Rerun `expand` after
rebuilding. See [ADR 0004](docs/adr/0004-cheap-model-workers.md).

### The index follows git

`memdag import` (and the automatic refresh on `search`, and before read tools in a running
`serve`) keeps the index in step with the files and commits it was built from:

- Delete a note or ADR, or amend/rebase away a `Decision:` commit, and its memory is removed
  on the next import. A file that exists but fails to parse keeps its memory, with a warning.
- Remove a note's `supersedes:` line, or the superseding note itself, and the older memory
  becomes active again.
- With write-through, resolving a note-backed blocker writes `docs/memory/<id>.resolved.md`
  next to the note, so the resolution survives a rebuild from git. Commit it with the note.
- Several repositories can share one database (`MEMDAG_DB`); each import only touches rows
  from its own repository.

The index reflects the current checkout: notes and commits on other branches are not in it
until you switch back. See [ADR 0003](docs/adr/0003-imported-rows-follow-their-sources.md).

### Scope & Limitations

memdag is a **local** SQLite store: one database per repository, on one machine. Several
agents can use it at once, including agents in different
[worktrees](#several-agents-several-worktrees) of the same repository: they share one
database, so tasks, blockers and `list_ready` cover all of them.

It does **not** coordinate agents on other machines. That includes cloud agents that run in
their own VM or container (hosted coding agents working on a fresh clone). They never see
your database. What they can share is what travels through git:

- **From you to them:** committed write-through notes, ADRs and `Decision:` commit trailers.
  `memdag import` rebuilds those in any clone.
- **From them to you:** a note they commit and push reaches you on your next pull and import.
  Tasks, blockers, links and resolutions of tasks stay in the database they were recorded in.

It is not a replacement for a distributed, multi-agent task tracker like
[beads](https://github.com/steveyegge/beads):

| | memdag | beads |
| --- | --- | --- |
| Storage | Local SQLite file | Dolt (versioned SQL, git-synced) |
| Worktrees on one machine | One shared database per repository; branch-only notes labelled | — |
| Agents on other machines / cloud agents | Only git-tracked notes, ADRs and commits, via `memdag import`; no shared tasks | `bd dolt push`/`pull` across git remotes |
| Concurrent agents | Safe concurrent reads/writes on one machine (WAL) | Atomic `--claim` (assignee + in_progress) prevents two agents grabbing the same task |
| Ready queue | `memdag ready` / `list_ready` (not atomic — no claim) | `bd ready` + atomic `--claim` |
| Priority / epics / sub-tasks | Not modeled (encode in `tags` if needed) | First-class fields and hierarchical IDs |

If agents on different machines need to claim and sync work without colliding, use beads.
If your agents run on one machine, in one checkout or several worktrees, and you want one
lightweight, zero-daemon store for their decisions, invariants and tasks, memdag covers it.

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
