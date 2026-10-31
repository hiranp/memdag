# Benchmark results: does memdag cut corrections?

ADR 0001 set the bar for memdag: memory should cut **corrections and cost**, not just raise
a solve rate. This benchmark tests that on a controlled project. It is the only evidence so
far that memdag's features help, so its limits (end of this page) matter as much as its
numbers.

Run on 2026-10-31 with memdag 0.4.1-dev, Claude Code 2.1.293 (`claude -p --bare`), through
an API gateway. Costs are as Claude Code reports them for that gateway.

## Setup

- **Project:** `fixture/ledger`, a small Python payments service. Its eight house rules are
  not written down in the code. For example, money is integer cents, timestamps come from
  `clock.now_utc()`, outbound HTTP goes through `request_with_retry`, and
  `legacy_tax.compute` is deprecated. The helper a rule calls for exists, but so does the
  tempting alternative.
- **Memories:** `memories.py` holds the eight rules as a past session would have recorded
  them, plus 12 unrelated memories. The wording differs from the task wording on purpose.
- **Tasks:** `tasks.py` has eight ordinary requests ("add a discount function", "make the
  limit configurable", ...). Each quietly depends on one rule. A deterministic grader checks
  the files and behaviour. Each grader fails on the untouched project and passes on a
  reference solution, both checked before the run. A failed grade is one correction a
  reviewer would have to ask for.
- **Conditions:**

  | Condition | What the agent has |
  |---|---|
  | `none` | The code only. |
  | `mcp` | The memdag MCP server with the memories; only the server's own instructions and tool descriptions. |
  | `installed` | The same, plus the CLAUDE.md block `memdag install` writes (taken from the binary). |
  | `installed+keywords` | The same, after a cheap model expanded the memories' search keywords (`memdag work next expand`). |

- **Isolation:** `--bare` (no CLAUDE.md discovery, no auto-memory, no hooks or plugins),
  `--strict-mcp-config`, a fresh copy of the project per run, and file tools only (no shell).
- **Model:** the agent is Claude Haiku, 5 runs per task and condition (160 runs). A check
  with Sonnet used 3 runs each (96 runs).

## Results

### Haiku, after the search fix (memdag `e9c3311`)

| Condition | Pass rate | Corrections / task | Cost / task | Tokens / task | Turns | memdag calls | Rule reached the agent |
|---|---|---|---|---|---|---|---|
| none | 57% | 0.42 | $0.0016 | 22,882 | 9.5 | 0.0 | 0% |
| mcp | 52% | 0.47 | $0.0018 | 45,822 | 9.9 | 0.3 | 15% |
| **installed** | **88%** | **0.12** | $0.0022 | 54,280 | 10.8 | 1.2 | 55% |
| installed+keywords | 78% | 0.23 | $0.0021 | 50,354 | 10.4 | 1.2 | 52% |

| Task (rule) | none | mcp | installed | installed+keywords |
|---|---|---|---|---|
| discount (money is cents) | 5/5 | 4/5 | 5/5 | 5/5 |
| created_at (UTC clock) | 0/5 | 3/5 | 4/5 | 2/5 |
| rates (HTTP with retry) | 0/5 | 0/5 | 4/5 | 3/5 |
| max_order (settings via config) | 3/5 | 1/5 | 2/5 | 2/5 |
| currency (PaymentError) | 5/5 | 3/5 | 5/5 | 4/5 |
| vat (deprecated tax function) | 5/5 | 5/5 | 5/5 | 5/5 |
| debug (no print) | 5/5 | 5/5 | 5/5 | 5/5 |
| why_90 (rationale) | 0/5 | 0/5 | 5/5 | 5/5 |

"Rule reached the agent" means the task's rule appeared in a memdag tool result the agent
received.

### Haiku, before the search fix (memdag `b72b1ac`)

| Condition | Pass rate | Corrections / task | Cost / task | memdag calls | Rule reached the agent |
|---|---|---|---|---|---|
| none | 52% | 0.47 | $0.0015 | 0.0 | 0% |
| mcp | 52% | 0.47 | $0.0018 | 0.5 | 8% |
| installed | 65% | 0.35 | $0.0021 | 1.1 | 15% |
| installed+keywords | 60% | 0.40 | $0.0022 | 1.2 | 12% |

### Sonnet, after the fix

| Condition | Pass rate | Corrections / task | Cost / task | memdag calls | Rule reached the agent |
|---|---|---|---|---|---|
| none | 71% | 0.29 | $0.033 | 0.0 | 0% |
| mcp | 67% | 0.33 | $0.044 | 0.2 | 21% |
| installed | 83% | 0.17 | $0.050 | 0.4 | 29% |
| installed+keywords | 75% | 0.25 | $0.053 | 0.6 | 42% |

### Significance

Two-sided Fisher exact test on pass counts:

| Comparison | Passes | p |
|---|---|---|
| Haiku: installed vs none | 35/40 vs 23/40 | 0.005 |
| Haiku: installed vs mcp | 35/40 vs 21/40 | 0.001 |
| Haiku: installed, after vs before the search fix | 35/40 vs 26/40 | 0.034 |
| Haiku: installed vs installed+keywords | 35/40 vs 31/40 | 0.38 (not significant) |
| Sonnet: installed vs none | 20/24 vs 17/24 | 0.49 (not significant) |

## What this shows

1. **Installed memdag cut Haiku's corrections by 71%.** Corrections fell from 0.42 to 0.12 per
   task, and the pass rate rose from 57% to 88% (p = 0.005). The cost per task rose by about
   38% ($0.0016 → $0.0022), mostly from reading search results (2.4× the tokens). For this
   model, memory raised quality and cost more.
2. **Connecting the server is not enough.** With only the MCP server, Haiku searched in about
   one task in three and did no better than no memory (52%). The instructions block that
   `memdag install` writes is what makes agents search before working, so install it.
3. **The benchmark found and fixed a real retrieval bug.** FTS5 requires every query word to
   match, so natural queries like "VAT invoice" or "order created_at timestamp" returned
   nothing whenever one word wasn't in the memory. Even agents that searched got the rule in
   only 15% of runs. Search now falls back to any-word matches ranked by BM25. That lifted
   the rule-reached rate to 55% and the installed pass rate from 65% to 88% (p = 0.034).
4. **Worker keyword expansion did not help here** (78% vs 88%, p = 0.38). After the
   any-word fallback, ordinary words already bridged most of the vocabulary gap. The extra
   keywords sometimes ranked a distractor higher. The feature stays, but there is no
   evidence for it yet. Its claim was search without an embedding model, and this
   benchmark's queries share at least one word with the memory far more often than real
   ones might.
5. **A stronger model gains less and searches less.** Sonnet started higher without memory
   (71%) and reached 83% with it. That difference isn't significant at 24 runs. Even with
   the instructions it called memdag in only 0.4 of tasks, so on most tasks it never saw the
   rule.

## Limits

- One synthetic project, eight tasks, written by the same author as memdag. The rules are
  deliberately invisible in the code. In a real repository more of them would be visible,
  so the gain over no memory would likely be smaller.
- Small samples: 40 runs per condition with Haiku, 24 with Sonnet. Differences under about
  20 points are within noise.
- The graders check one rule each, mostly by inspecting the source (e.g. "uses
  `request_with_retry`, not `urllib`"). A solution that obeys the rule in an unexpected way
  would fail.
- `why_90` asks for a rationale. Sonnet guessed "chargebacks" from general knowledge in 3/3
  runs without memory, so that task only discriminates for weaker models.
- Costs come from Claude Code's own accounting through a gateway, and agents had no shell,
  so the turn and cost figures are lower than in real sessions.
- Not measured yet: STALE flag accuracy on a real repository, worktrees, and the
  `verify` and `consolidate` workers. ADR 0001 lists them.

## Reproduce

```bash
cargo build --release
bench/run.py --model haiku --repeats 5 --jobs 6        # ~160 runs, about 10 minutes
bench/run.py --model sonnet --repeats 3 --tasks rates,created_at
```

Raw per-run results go to `bench/results/` (ignored by git); the tables above come from the
summaries it writes.
