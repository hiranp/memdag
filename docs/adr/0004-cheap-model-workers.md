# 0004. Cheap-model workers: memdag hands out work, the calling agent runs it

- Status: accepted
- Date: 2026-10-30
- Refs: ADR-0001, ADR-0003

## Context

Two jobs in memdag need judgement over text, not arithmetic:

- **Vocabulary mismatch.** Keyword search only finds a memory through words it contains, so
  "lock contention" misses "Use WAL journal mode". The existing fix is an embedding model
  behind `MEMDAG_EMBED_CMD`. Many users won't set one up, and memdag bundles none (ADR 0001).
- **STALE memories.** memdag can tell that a cited file changed, but not whether the memory
  is still true. Today a human or the main agent has to check each one.

Both are bulk, low-stakes, parallel work: the kind a small model (Claude Haiku, Gemini Flash)
does well, at a small fraction of the cost of the main agent's model. ADR 0001 rules out
memdag calling a model itself: no API keys, no network, no model choice baked into the
binary.

Options considered:

1. **MCP sampling** (`sampling/createMessage` with `modelPreferences`): the server asks the
   client's model for a completion. This fits in principle, but the Claude Code docs don't
   mention it, so it can't be relied on.
2. **memdag calls a model API.** Rejected by ADR 0001.
3. **memdag hands out work; the host runs it.** memdag returns batches with instructions, the
   host agent runs them on a cheap subagent, and the answers come back through a tool. Both
   target harnesses support this today: Claude Code and OpenCode both turn MCP prompts into
   slash commands, and both have subagents with a per-agent `model:`.

We researched how three other tools integrate with harnesses: `hief`, Graphify and codegraph.
None of them sends work to a cheaper model. Two lessons were worth copying:

- codegraph found that the instructions an MCP server sends when it connects reach only the
  main agent; subagents read `CLAUDE.md`/`AGENTS.md`. It writes a block between markers there.
- Graphify matches section markers only when the whole line is the marker. A looser match had
  deleted user content.

## Decision

Option 3.

- **Work queue** (`src/work.rs`). `get_work {task, limit}` returns
  `{instructions, items, remaining}`. `submit_work {task, results | reply}` applies the
  answers. The CLI equivalents are `memdag work next | submit`. The instructions *are* the
  worker prompt and spell out the exact JSON to answer with. `submit` takes the reply as it
  comes: code fences and stray prose around the JSON are fine. Each item is validated on its
  own.
- **`expand`.** The worker writes 8 to 20 search terms per memory. They are stored in
  `memories.keywords`, the fifth column of the FTS index, weighted 0.5. They must share the
  FTS row with the memory's text because the query is an implicit AND: a separate table
  could never match a query that uses one word from the body and one from the keywords.
  Keywords are cut down to plain 1-to-4-word terms and checked for secrets. They live only in
  the index: a rewrite clears them and puts the memory back in the queue, and a rebuild from
  git starts without them.
- **`verify`.** The worker reads the changed files and answers for each memory:
  - `valid` re-baselines the memory. For a write-through note this records a new note with
    the same content that supersedes the old one, because notes are append-only and the
    confirmation must survive a rebuild. Other imported rows are refused: their source would
    overwrite an in-place re-baseline on the next import.
  - `outdated` records the correction as a new memory that supersedes the old one. The new
    memory is tagged `worker-corrected` and carries the worker's reason, so later readers can
    tell it from a human decision.
  - `unsure` leaves the memory flagged.

- **Worker output is untrusted.**
  - The task the caller asked for wins. A reply naming a different task is rejected.
  - Each id counts once per reply.
  - A verdict applies only to an active, non-ephemeral memory that is stale right now.
  - Keywords can only be written for memories still waiting in the queue.

  So a repeated, late or invented answer does nothing.
- **Delegation.** The MCP prompts `expand_keywords` and `verify_stale` tell the host agent to
  start the `memdag-worker` subagent if it exists, or else to run the batch on its cheapest
  model itself. `memdag work install --client claude-code|opencode [--global] [--model]`
  writes:
  - the subagent. On Claude Code it defaults to `model: haiku`, with tools limited to Read,
    Grep, Glob, `mcp__memdag__get_work` and `mcp__memdag__submit_work`. Those names assume
    the server is registered as `memdag`, as `memdag mcp install` does. On OpenCode it is
    `mode: subagent` with edit and shell denied, using the V2 `permissions:` list format from
    the OpenCode V2 agent docs. There is no default model there, because OpenCode needs a
    `provider/model` id.
  - a marked `MEMDAG_START`/`MEMDAG_END` block in `CLAUDE.md` or `AGENTS.md`.

  The worker runs the get/submit loop itself, so the main model never reads a batch. Uninstall
  deletes only files that carry memdag's marker line, and only text between exact marker
  lines. Files keep their line endings, permissions and symlinks (`CLAUDE.md -> AGENTS.md`).
  A file left empty is deleted.

## Consequences

- An index can be made searchable by meaning without an embedding model, at the cost of one
  cheap-model call per batch of 20. The two approaches combine.
- STALE memories can be cleared in bulk. Wrong corrections are recoverable because the old
  memory stays in history as superseded. Still, the prompts ask the host to point out
  corrections to the user.
- Measured on this machine with Claude Haiku via `claude -p`: an `expand` batch produced 18
  terms per memory, and searches such as "lock contention" and "zlib backup" then found the
  intended memories. A `verify` batch correctly marked a 5 s → 8 s timeout change `outdated`
  with an accurate correction, and answered `unsure` where the code didn't settle the claim.
- Not done yet: MCP sampling as a third path; worker files for Codex, Cursor and Gemini;
  Claude Code `permissions.allow` for the worker's tools, without which each call may ask for
  approval; consolidation of duplicate memories as a third task.
