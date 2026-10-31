# 0005. All worktrees of a repository share one database

- Status: accepted
- Date: 2026-10-31
- Refs: ADR-0003

## Context

Agents increasingly work in parallel, each in its own `git worktree`. memdag found its
database by walking up to the nearest `.git`, and a linked worktree has its own `.git` file,
so each worktree got a separate database. A test with a main checkout and one worktree
showed the effects:

- A task recorded in one worktree was invisible from the other, so `list_ready` could not stop
  two agents from taking the same work. Coordinating agents is what tasks and blockers are
  for.
- With one shared `MEMDAG_DB`, ADR 0003's ownership counted each worktree path as a different
  repository. A branch's edit of a note was refused ("id already used by a note memory from
  another repository") and its deletion of a note was ignored.
- Treating the worktrees as one repository would have been worse. Each worktree's import
  would prune the notes that exist only on the other's branch, including just-written
  uncommitted ones, so memories would appear and disappear depending on which worktree
  refreshed last.

The conflict: recorded memories (tasks, blockers, links) should be shared, but the index of
git-tracked files differs per branch, and a memory id names one row.

We first proposed a second, per-worktree database for the file index, read alongside the
shared one with SQLite `ATTACH`. Writing it out showed the cost. Relations routinely connect
the two kinds of row (a local task `depends_on` a note-backed decision), and SQLite cannot
enforce foreign keys across databases. Every query (search, vector search, get, list, ready,
stale, relations) would have needed a second copy for the attached database.

## Decision

One database per repository, and per-worktree ownership of imported rows.

- **Location.** In a git repository the default database is `<main checkout>/.memdag/memdag.db`
  for every worktree. That is where memdag always kept the main checkout's database, so that
  one stays in place. A bare repository uses `<common dir>/memdag/memdag.db`. The repository
  is identified by its git common dir, read from git's own files (`.git` → `gitdir` →
  `commondir`) without starting `git`. A directory with its own `.memdag/` that isn't a
  checkout root (a nested crate) still keeps its own database. `MEMDAG_DB` still overrides
  everything.
- **Ownership (schema v5).** `memory_owners(id, worktree)` lists the checkouts whose files
  contain an imported row's source. `memories.origin_repo` becomes the repository key. An
  import:
  - claims every row it applies;
  - after a complete scan of a source, drops only its own claim on rows that source no longer
    produces;
  - drops the claims of worktrees missing from `<common dir>/worktrees/`;
  - then deletes the repository's rows that no checkout claims.

  Rows imported before v5 recorded the importing checkout in `origin_repo`, which becomes
  their first owner; such paths are also accepted as "same repository".
- **Visibility.** Rows only another worktree claims are still found, and labelled `only in
  another worktree`, because their branch may never be merged. Their cited paths describe
  that branch, so they are never STALE-checked against this checkout and never offered to
  `verify` workers.
- **Upgrade.** The first time a linked worktree opens the shared database, memdag merges that
  worktree's old database into it: the rows recorded directly (`source IS NULL`) and the
  relations whose two ends exist afterwards. Imported rows are rebuilt from git rather than
  copied. The old file is claimed by renaming it before the merge, so two starting processes
  can't both merge it, and it is kept as `memdag.db.merged`. If the merge fails, the rename is
  undone so the next start retries.

## Consequences

- Tasks, blockers, links and resolutions are visible to every agent in every worktree, and
  concurrent access is the same multi-process SQLite case as before (WAL, `busy_timeout`).
- The index is now the union of the repository's checkouts, not just the current one (this
  replaces ADR 0003's "the index reflects the current checkout").
- A note edited differently on two branches is still one row, and the version from the most
  recent import is shown. Notes are append-only by design (supersession writes a new note), so
  this needs a hand edit, and it is reported nowhere; accepted.
- Embedding vectors and worker keywords are not carried over by the legacy merge; `memdag
  embed` and `memdag work next expand` recreate them.
- Claims of a worktree on a disk that isn't mounted look like a removed worktree and are
  dropped; the next import from that worktree restores them.
