---
name: memdag
description: Use memdag's tools for durable project memory and dependency-aware task tracking whenever the project has a memdag MCP server or CLI available. Trigger this at the start of any task (to recall past decisions/invariants/blockers), whenever an architectural decision or durable learning is made, whenever a task or blocker is created/resolved/linked, and at the end of a session.
---

# memdag

memdag is a local, zero-daemon, relational-DAG memory engine and lightweight task tracker.
Use it proactively — don't wait to be asked.

## When to call it

- **Starting a task**: call `search_memory` with the task's key terms before writing any code,
  to recall relevant past decisions, invariants, and open blockers.
- **Making a decision**: call `record_memory` with `kind="decision"`. If it replaces an earlier
  decision, pass `supersedes_id` so the old one is atomically marked superseded instead of
  silently coexisting with the new one.
- **Discovering an invariant** (a rule that should hold going forward): `record_memory` with
  `kind="invariant"`.
- **Tracking work**: use `kind="task"` for work items and `kind="blocker"` for anything
  blocking one. Connect them with `link_entities(blocker_id, task_id, "blocks")`.
- **Finding claimable work**: call `list_ready` instead of `list_memories` + inspecting each
  item's relations by hand — it returns active tasks/blockers with no open blocker already.
  This matters most when multiple agents/subagents may be working the same project; `list_ready`
  is not an atomic claim, so still coordinate if two agents might grab the same item.
- **Finishing a task or blocker**: call `resolve_memory` with a short note on how it was
  resolved.
- **Ending a session**: call `consolidate_session` to promote durable learnings into permanent
  memories and purge (or archive) scratch/ephemeral notes so they don't pollute future search.

## Tool reference

| Tool | Purpose |
| --- | --- |
| `record_memory` | Insert a decision/task/invariant/blocker/ephemeral node; atomic supersession via `supersedes_id`. |
| `link_entities` | Directed DAG edge: `depends_on`, `blocks`, `references`, `supersedes`. |
| `search_memory` | BM25 keyword search with 1-hop DAG expansion. |
| `search_vector` | KNN embedding search with 1-hop DAG expansion. |
| `get_memory` | Fetch one memory with all incoming/outgoing relations. |
| `list_memories` | Filtered list by kind/status. |
| `list_ready` | Claimable tasks/blockers: active, no incoming active `blocks` edge. |
| `resolve_memory` | Mark a task/blocker resolved, with a note. |
| `consolidate_session` | Promote session learnings, purge/archive ephemeral notes. |

If memdag is exposed as an MCP server, call these tools directly. If only the CLI is
available, the same operations exist as `memdag record|link|search|get|list|ready|resolve|consolidate`
(see the project's `README.md` for exact flags).

## What NOT to do

- Don't record every trivial completed task as a permanent memory — most task/blocker rows are
  fine left as `resolved` (retained, but excluded from default search). Only decisions,
  invariants, and durable learnings from `consolidate_session` should accumulate as
  long-term, actively-searched context.
- Don't treat memdag as a distributed multi-machine task tracker — it's a single-file SQLite
  store. There's no atomic claim/assignee field, so don't assume `list_ready` prevents two
  agents from picking the same item.
