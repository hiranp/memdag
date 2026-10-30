# 0003. Imported rows follow their sources: pruning, resolution sidecars, embedder identity

- Status: accepted
- Date: 2026-10-30
- Refs: ADR-0001

## Context

ADR 0001 made the SQLite index a cache that `memdag import` rebuilds from git-tracked text.
Its review left five gaps, each a way the cache could drift from its sources:

1. The importer only upserted. A deleted note or ADR, an amended or rebased commit (which
   changes the `GIT-<sha7>` id) or a deleted handoff comment left its row behind for good.
2. `resolve_memory` changed only the database. Notes are append-only, so the file still
   described an open blocker, and a rebuild from git made it `active` again.
3. `serve` imported once at startup. A long-lived MCP session never saw later commits.
4. The embedder's identity was not recorded. Switching `MEMDAG_EMBED_CMD` mixed two vector
   spaces in one KNN index without any warning.
5. Note files were written in place with no fsync, so a crash could leave a truncated note
   that the next import would read as truth.

## Decision

**Ownership and pruning.** Schema v3 adds `memories.origin_repo`: the canonical path of the
repository an imported or note-backed row came from. A row is owned by its `(source,
origin_repo)` pair. After a source has been scanned *completely*, the importer deletes the
rows that source owns in this repo but did not produce this time. Relations cascade, and
triggers remove the FTS and vector rows. "Completely" means:

- `git`: only a full history scan (no `--since`) that found a `HEAD`. Auto-refresh does a
  full scan when the recorded `HEAD` is no longer an ancestor, which is exactly the
  amend/rebase/branch-switch case.
- `adr`, `note`: the directory was read without error. A missing directory counts as empty.
  A file that exists but cannot be read or parsed keeps its row and causes a warning, so a
  half-edited note does not vanish from the index.
- `gh`: the fetch succeeded.

Rows from another repo sharing the same `MEMDAG_DB` are never pruned. An import also refuses
to overwrite a same-source row that belongs to another repo. Rows imported before v3 have no
origin; the next import of their repo claims them, and an orphan from before the upgrade may
remain until it is deleted by hand.

**The index reflects the current checkout.** Because notes and commits on other branches are
not in the scan, switching branches prunes their rows, and switching back restores them.
This is the intended reading of "the database is a cache of what is tracked here".

**Supersession is re-derived.** For each imported row, the outgoing `supersedes` edges are
set to exactly what its source says now. A row that loses its last incoming `supersedes`
edge, either because the line was removed or because the superseding note was deleted,
becomes `active` again. So does an ADR whose Status line no longer says superseded. Other
relation types are left alone, because agents add `depends_on`/`blocks`/`references` edges
with `link_entities` and nothing in git records them.

**Resolutions are sidecar files.** Under write-through, `resolve_memory` on a note-backed
memory first writes `<notes_dir>/<id>.resolved.md`:

```text
---
resolves: <id>
---
<resolution note, optional>
```

The note itself is still never rewritten. The importer merges the sidecar into its note: the
body gets the same `\n\n[Resolved]: <note>` suffix `resolve_memory` appends, and the status
becomes `resolved`. A rebuild therefore reproduces the row exactly, and re-importing changes
nothing. The `.resolved` suffix is reserved in note ids. A second resolve of the same
memory is an error, because resolutions are append-only. Tasks and other memories that are
not note-backed stay database-only, as before.

**Refresh while serving.** The MCP server runs the same refresh the CLI uses
(`import::auto_refresh`) before the read tools: `search_memory`, `search_vector`,
`get_memory`, `list_memories` and `list_ready`. It does this at most once every 2 s. On a
fresh index this costs one `git rev-parse`.

**Embedder identity.** The command string of `MEMDAG_EMBED_CMD` is stored under the
`memdag_meta` key `embedder`. The first use of an embedder in a process compares it with
that key. On a mismatch every stored vector is deleted, a warning tells the user to run
`memdag embed`, and the new command is recorded. Vectors stored before identities were
tracked are adopted. The command string is the identity, so changing it in any way (a new
path, a new flag) also resets the vectors. Changing the model without changing the command
is not detected.

**Atomic note files.** Notes and sidecars are written to a hidden temp file in the same
directory (`.<name>.<pid>.<uuid>.tmp`, ignored by the importer), fsynced, then hard-linked
into place. A hard link never replaces an existing file. If the filesystem has no hard
links, an existence check followed by `rename` is used. The temp file is then removed and,
on unix, the directory is fsynced.

## Consequences

- `ImportReport` gains `pruned`. `memdag import` prints "N removed".
- Deleting a note file is now the way to forget a durable memory, and it propagates to every
  index on the next import or refresh.
- Hand-written `link_entities` references from imported rows survive. Stale `refs:`
  references are removed only when the row itself is pruned.
- Schema v3 migration: `ALTER TABLE ADD COLUMN origin_repo`, an index on
  `(source, origin_repo)`, and removal of vectors with no memory.
