# 0001. Evidence-informed memory direction: keep memdag, add provenance, budgeted hybrid recall

- Status: accepted
- Date: 2026-10-30

## Context

We re-examined memdag (and its sibling `hief`) against recent practice and research to
decide whether to keep the design, adopt a different memory system (Hindsight, Vectorize),
or change course. Sources and how much of each was actually read:

| Source | Read | Used for |
| --- | --- | --- |
| Anthropic, *Effective context engineering for AI agents* (2025-09-29) | full (via web archive; site blocked by corporate proxy) | context is a finite budget; JIT retrieval; compaction, note-taking, sub-agents |
| Anthropic, *Effective harnesses for long-running agents* (2025-11-26) | full (web archive) | progress file + git log + JSON feature list; one feature per session; clean state |
| Anthropic, *How we built our multi-agent research system* | skimmed | artifacts on disk over relayed summaries; end-state evaluation |
| Nikolov et al., *How is Google using AI for internal code migrations?* (arXiv 2501.06972) | full | LLM only generates edits; location finding and validation are deterministic; review is the bottleneck |
| Wadhwa et al., *CORE* (FSE'24, arXiv 2309.12938) | abstract + Springer summary (PDF URL returned 502) | proposer + ranker; static gate + rubric ranker |
| Cihan et al., *Automated Code Review in Practice* (arXiv 2412.18531) | abstract | 73.8% of comments resolved, but PR closure time rose 5h52m to 8h20m |
| Patcas & Motogna, Empirical Software Engineering 31:118 (2026) | full | static pass is not semantic correctness; LLM-judge vs tests agree 71% (kappa 0.158); function-scoped fixes break callers; flaky baselines |
| Latimer et al., *Hindsight is 20/20* (arXiv 2512.12818) and Hindsight docs | paper + coding-agent and observation docs | separate evidence from belief; 4-way retrieval + RRF + rerank + token budget; consolidation with evidence |
| martinuke0 context-engineering tutorial | full | generic; nothing beyond Anthropic's post |

Findings that bear on memdag:

1. **Context is a budget.** Both Anthropic and Hindsight cap recall by tokens, not item
   count. memdag capped by count (default 100 FTS rows, 1-hop relations unbounded).
2. **Cross-session state works best as plain, versioned artifacts** (progress file, git
   log, JSON feature list). memdag already moved to this: git-tracked text is the source of
   truth and SQLite is a disposable index (`import.rs`, write-through notes).
3. **Deterministic gates beat LLM judgement.** The strongest results (Google, CORE) put
   validation in code. The Springer study shows LLM-as-judge is a weak oracle.
4. **Memory goes stale silently.** Hindsight tracks observation freshness with an LLM.
   memdag's explicit `supersedes` edges are more deterministic, but a memory about
   `src/x.rs` has no way to say "true as of commit C" and so cannot be flagged when `x.rs`
   later changes.
5. **Hindsight's evidence is not about coding.** Its headline numbers (LongMemEval 91.4%,
   LoCoMo 89.6%) are conversational and vendor-run; the paper has no component ablation.
   The one coding result (AMB sdebench, 61 tasks) shows the solve rate unchanged (60-61 of
   61) while corrections per task drop (Claude Code 0.85 to 0.36) and cost per task falls
   ($0.445 to $0.338). The benefit is fewer corrections and lower cost, not capability.
6. **memdag defect found during review:** the `search_vector` MCP tool required the calling
   model to supply a 384-float embedding, which a language model cannot produce. The
   vector half of "hybrid retrieval" was unreachable through MCP.

## Decision

Keep memdag's architecture (zero daemon, SQLite as a derived index over git-tracked text,
explicit DAG supersession). Do **not** adopt Hindsight. Close three gaps:

1. **Vector search works without a bundled model.** An optional external command,
   `MEMDAG_EMBED_CMD`, turns text into a 384-float JSON vector (stdin to stdout). When set,
   `search_memory` fuses BM25 and vector hits with Reciprocal Rank Fusion (k=60), new
   memories are embedded on write, and `memdag embed` backfills the rest. When unset,
   behaviour is unchanged (BM25 only) and `search_vector` is not advertised over MCP.
   The command is read **only from the environment**, never from `memdag.toml`: a cloned
   repository must not be able to make memdag execute a command.
2. **Recall output is token-budgeted.** `search_memory` takes `max_tokens` (default 2000,
   `0` = unlimited), packs results in rank order, always returns at least one (truncated
   to fit), caps relations per result, and says how many results were omitted.
3. **Memories carry provenance and can be flagged stale.** New optional `paths`,
   `commit`, `fingerprints` and `source` fields (schema v2, note front matter, importer,
   MCP/CLI). A memory recorded with `paths` is stamped with the current git HEAD and a
   **content fingerprint** of each cited path (git blob hash; for a directory, a hash over
   its tracked files). At read time memdag recomputes the fingerprints and marks the memory
   STALE when a cited path's content differs. `memdag stale` lists them. Memories that only
   have an explicit `commit` fall back to `git diff <commit> -- <paths>`. A baseline that
   cannot be compared yields no signal rather than a false alarm.

   The first implementation diffed against the recorded commit. Review showed that is wrong
   for the most common workflow: edit `x.rs`, then record a decision about it. The working
   tree already differs from HEAD, so the memory is stale from birth and stays stale after
   the edit is committed. Content fingerprints fix that and also survive rebases, amends,
   branch switches and shallow clones.

## Alternatives considered

- **Adopt Hindsight.** Needs an LLM call on every retain and on every session write-back,
  Postgres/pgvector, and a Python daemon or its cloud. That contradicts the zero-daemon
  principle and sends repository content to an LLM provider. Revisit only if the
  validation below shows a large gap.
- **Bundle an ONNX embedding model (fastembed/ort).** Large compile cost, large binary,
  a model download at runtime (blocked on managed networks), and a heavier release matrix
  (Linux, Windows, macOS x2). The external command costs zero dependencies and lets users
  pick any local embedder (Ollama, a script, `hief`'s embedder).
- **LLM-based consolidation inside memdag.** Violates the sidecar rule that the host
  agent supplies reasoning. The host already consolidates via `consolidate_session`.
- **Staleness via LLM freshness checks (Hindsight style).** Non-deterministic and costs
  tokens; a path diff against a recorded commit is exact and free.

## Consequences

- Schema bumps to v2 (four nullable columns: `paths`, `commit_sha`, `fingerprints`, `source`;
  `ALTER TABLE ADD COLUMN`, no table rebuild). Existing databases migrate on open; existing
  note files stay valid.
- `source` records which importer owns a row. An import may refresh only rows it owns (or
  adopt a source-less durable row that a note file describes), so a note, ADR or comment can
  no longer overwrite an unrelated local memory by claiming its id.
- Notes gain optional `paths:` and `commit:` front-matter keys, emitted only when set, so
  existing note output is byte-identical.
- Provenance is opt-in. Memories without `paths` behave exactly as before and cost no git
  calls.
- Imported ADR, git-trailer and GitHub-handoff memories carry no paths yet. Extracting a
  `Paths:` trailer from `Decision:` commits is a natural follow-up.
- **Imported content is untrusted.** Anyone who can open a PR can add a note, and anyone can
  comment on a public issue. Imports now get the same secret check as direct writes plus a
  size cap; note ids must equal the file name; `gh` is opt-in and only `OWNER`/`MEMBER`/
  `COLLABORATOR` comments that *start* with the handoff marker are accepted; search output
  labels imported rows with their source and escapes heading lines so memory text cannot
  forge result blocks.
- Observed-versus-inferred markers and confidence (Hindsight's evidence/belief split) are
  deliberately deferred until provenance proves useful.

## Known limitations (deferred after review)

Found by an independent review of the branch. Pruning, resolutions in git, refresh inside
`serve`, embedder identity and atomic note files were fixed afterwards; see ADR 0003. Still
open:

- Fixed in 0.4.1: a rebuild now restores the creation time and session id of write-through
  notes (`recorded:`, `session:`); `MEMDAG_EMBED_MODEL` names the model behind an unchanged
  command; commit messages containing `0x1f` parse correctly; staleness takes a constant
  number of git processes per search; `gh` import is incremental; `search_vector` applies
  status and kind filters.
- Still open: a `0x1f` inside a `Decision:`/`Refs:` trailer value; notes written before 0.4.1
  have no `recorded:`, so their rebuild time is the import time.

## Validation

The claim to test is Hindsight's: memory should cut corrections and cost, not raise solve
rate. Replay a fixed set of recent tasks from `email-agent` with (a) no memory, (b) memdag,
(c) Hindsight daemon mode, and compare corrections per task, tokens, and wall time. Check
whether AMB sdebench can be run locally before building a bespoke harness. Also track the
rate at which STALE flags are correct, using `memdag stale` output reviewed against the
actual diffs.
