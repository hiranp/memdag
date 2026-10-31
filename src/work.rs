//! Work batches for a low-cost LLM worker.
//!
//! memdag never calls a model. Instead it hands out batches of bulk text work together with
//! the instructions for doing it; the calling agent runs them on a cheap model (a Claude
//! Haiku or Gemini Flash subagent, or a headless CLI) and submits the JSON answers back.
//!
//! Tasks:
//! - `expand`: search keywords for memories (synonyms, related terms) so keyword search finds
//!   them with words they don't contain. The no-embedder alternative to vector search.
//! - `verify`: check STALE memories against the code they cite; confirm, correct or skip.
//! - `consolidate`: judge pairs of similar durable memories: duplicate (resolve one),
//!   conflict (becomes a task for a person), related (link them) or distinct.

use crate::models::{Memory, MemoryKind, MemoryStatus, RelationType};
use crate::store::{MemoryStore, RecordOptions};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::fmt;
use std::str::FromStr;

/// Default and maximum items per batch: small enough for a cheap model's context and a quick
/// answer, large enough to amortise the call.
pub const DEFAULT_BATCH: usize = 20;
pub const MAX_BATCH: usize = 50;
/// Body characters sent per item.
const BODY_CHARS: usize = 1500;
const MAX_KEYWORDS: usize = 25;
const MAX_KEYWORD_CHARS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkTask {
    Expand,
    Verify,
    Consolidate,
}

impl WorkTask {
    pub const ALL: [WorkTask; 3] = [WorkTask::Expand, WorkTask::Verify, WorkTask::Consolidate];

    pub fn as_str(self) -> &'static str {
        match self {
            WorkTask::Expand => "expand",
            WorkTask::Verify => "verify",
            WorkTask::Consolidate => "consolidate",
        }
    }

    /// The prompt for the worker model: what to do with each item and the exact JSON to
    /// answer with.
    pub fn instructions(self) -> &'static str {
        match self {
            WorkTask::Expand => EXPAND_INSTRUCTIONS,
            WorkTask::Verify => VERIFY_INSTRUCTIONS,
            WorkTask::Consolidate => CONSOLIDATE_INSTRUCTIONS,
        }
    }
}

impl fmt::Display for WorkTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WorkTask {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "expand" | "keywords" | "enrich" => Ok(WorkTask::Expand),
            "verify" | "verify_stale" | "stale" => Ok(WorkTask::Verify),
            "consolidate" | "consolidate_memories" | "dedupe" => Ok(WorkTask::Consolidate),
            other => Err(format!(
                "unknown work task '{other}' (expected: expand, verify, consolidate)"
            )),
        }
    }
}

const EXPAND_INSTRUCTIONS: &str = r#"You write search keywords for entries in a software project's memory index. A developer or coding agent will later search this index with a few words; your keywords help an entry match searches that use different words than the entry itself.

For EACH item below, list 8 to 20 search terms that someone looking for this entry might type but that are NOT already in its title or body:
- synonyms and near-synonyms ("lock contention" for "busy timeout")
- the general concept and the specific technology ("concurrency", "sqlite")
- abbreviations and their expansions ("WAL" <-> "write-ahead log")
- error messages, error codes, symptoms it would explain
- names of components, commands, or files it clearly implies

Rules:
- Each term is 1 to 3 words, lowercase unless it is a proper name or code identifier.
- Terms only. No sentences, no new facts, no opinions, no secrets or credentials.
- If an item is too vague to expand, return an empty list for it.
- Answer with ONLY this JSON, no prose and no code fences:
{"task":"expand","results":[{"id":"<item id>","keywords":["term", "term"]}]}"#;

const VERIFY_INSTRUCTIONS: &str = r#"You check whether entries in a software project's memory index still match the code. Each item below records a decision or fact and cites files ("paths"); the files listed in "stale_paths" have changed since the entry was recorded.

For EACH item:
1. Read the files in "stale_paths" (only those, and only as much as you need).
2. Decide:
   - "valid": the entry's claim still holds for the current code.
   - "outdated": the code now contradicts the entry. Then give "correction": the entry rewritten so it is true for the current code. Keep what still holds, say what changed, same style and length. Optionally a new "title".
   - "unsure": you cannot tell from a quick read. Prefer this over guessing.
3. "reason": one sentence with the evidence (file and what you saw there).

Rules: do not edit any files. Do not include secrets or credentials. Answer with ONLY this JSON, no prose and no code fences:
{"task":"verify","results":[{"id":"<item id>","verdict":"valid|outdated|unsure","reason":"...","correction":{"title":"optional","body":"required when outdated"}}]}"#;

const CONSOLIDATE_INSTRUCTIONS: &str = r#"You tidy a software project's memory index. Each item below is a pair of entries ("a" and "b": decisions, invariants or learnings) that use similar words. Decide what the pair is.

For EACH pair, give one "verdict":
- "duplicate": both say the same thing. Then give "keep": the id of the entry to keep (the clearer, more complete one). The other is closed as a duplicate.
- "conflict": they contradict each other and the text does not make clear which one is current. A person will reconcile them.
- "related": different claims about the same topic, worth linking.
- "distinct": unrelated, or only superficially similar.
And "reason": one sentence saying why.

Rules:
- When unsure, prefer "distinct" or "related". Only say "duplicate" when one entry adds nothing the other lacks, and "conflict" when both cannot be true at once.
- Judge only from the text given. Never invent facts. No secrets or credentials.
- Answer with ONLY this JSON, no prose and no code fences:
{"task":"consolidate","results":[{"pair":["<a id>","<b id>"],"verdict":"duplicate|conflict|related|distinct","keep":"<id, duplicates only>","reason":"..."}]}"#;

fn truncate(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push_str(" […]");
    }
    out
}

fn base_item(m: &Memory) -> serde_json::Map<String, Value> {
    let mut o = serde_json::Map::new();
    o.insert("id".into(), json!(m.id));
    o.insert("kind".into(), json!(m.kind.as_str()));
    o.insert("title".into(), json!(m.title));
    o.insert("body".into(), json!(truncate(&m.body, BODY_CHARS)));
    if let Some(t) = &m.tags {
        o.insert("tags".into(), json!(t));
    }
    o
}

fn pair_member(m: &Memory) -> Value {
    let mut o = base_item(m);
    o.insert("status".into(), json!(m.status.as_str()));
    Value::Object(o)
}

/// The next batch for `task`: `{task, instructions, items, remaining, submit}`. `remaining`
/// counts items still waiting after this batch.
pub fn next_batch(store: &MemoryStore, task: WorkTask, limit: usize) -> Result<Value> {
    let limit = limit.clamp(1, MAX_BATCH);
    let (items, total): (Vec<Value>, usize) = match task {
        WorkTask::Expand => {
            let (mems, total) = store.memories_needing_keywords(limit)?;
            (
                mems.iter().map(|m| Value::Object(base_item(m))).collect(),
                total,
            )
        }
        WorkTask::Verify => {
            let stale = store.list_stale()?;
            let total = stale.len();
            let items = stale
                .into_iter()
                .take(limit)
                .map(|(m, changed)| {
                    let mut o = base_item(&m);
                    o.insert("paths".into(), json!(m.paths));
                    o.insert("stale_paths".into(), json!(changed));
                    if let Some(c) = &m.commit {
                        o.insert("recorded_at_commit".into(), json!(c));
                    }
                    Value::Object(o)
                })
                .collect();
            (items, total)
        }
        WorkTask::Consolidate => {
            let (pairs, total) = store.consolidation_candidates(limit)?;
            let items = pairs
                .iter()
                .map(|(a, b)| {
                    json!({
                        "pair": [a.id, b.id],
                        "a": pair_member(a),
                        "b": pair_member(b),
                    })
                })
                .collect();
            (items, total)
        }
    };
    Ok(json!({
        "task": task.as_str(),
        "instructions": task.instructions(),
        "remaining": total.saturating_sub(items.len()),
        "items": items,
        "submit": "Pass the worker's JSON `results` to submit_work (CLI: `memdag work submit`).",
    }))
}

/// A batch rendered as one plain-text prompt, for piping into a headless model CLI.
pub fn batch_as_prompt(batch: &Value) -> String {
    let items = serde_json::to_string_pretty(&batch["items"]).unwrap_or_default();
    format!(
        "{}\n\nItems:\n{items}\n",
        batch["instructions"].as_str().unwrap_or_default()
    )
}

/// Pull the JSON answer out of a model's reply: tolerates code fences and stray prose, and
/// accepts either `{"results": [...]}` or a bare array.
pub fn parse_reply(text: &str) -> Result<Value> {
    let t = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Ok(v);
    }
    let candidates = [t.find('{').zip(t.rfind('}')), t.find('[').zip(t.rfind(']'))];
    for (start, end) in candidates.into_iter().flatten() {
        if start < end
            && let Ok(v) = serde_json::from_str::<Value>(&t[start..=end])
        {
            return Ok(v);
        }
    }
    bail!("worker reply contains no JSON object or array")
}

/// What a submission did, item by item.
#[derive(Debug, Default)]
pub struct SubmitReport {
    pub applied: usize,
    pub skipped: usize,
    pub lines: Vec<String>,
}

impl fmt::Display for SubmitReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Applied {}, skipped {}.", self.applied, self.skipped)?;
        for l in &self.lines {
            writeln!(f, "- {l}")?;
        }
        Ok(())
    }
}

/// The task named in a reply, falling back to `default`.
/// The task to apply a reply as. The caller's explicit task wins: the reply is worker output,
/// so it may only confirm that task, never switch to another one.
pub fn reply_task(reply: &Value, explicit: Option<WorkTask>) -> Result<WorkTask> {
    let named = match reply.get("task").and_then(Value::as_str) {
        Some(t) => Some(t.parse::<WorkTask>().map_err(|e| anyhow::anyhow!(e))?),
        None => None,
    };
    match (explicit, named) {
        (Some(e), Some(n)) if e != n => {
            bail!("the reply is for task '{n}' but '{e}' was requested")
        }
        (Some(t), _) | (None, Some(t)) => Ok(t),
        (None, None) => bail!("the reply names no task; pass one (expand, verify or consolidate)"),
    }
}

/// Apply a worker's answers. Every item is validated on its own, so one bad answer never
/// sinks the batch. Worker output is untrusted text: it goes through the same secret check
/// as any write, and keyword lists are length-capped and stripped to plain terms.
pub fn submit(store: &mut MemoryStore, task: WorkTask, reply: &Value) -> Result<SubmitReport> {
    let results = match reply {
        Value::Array(a) => a,
        v => v
            .get("results")
            .and_then(Value::as_array)
            .context("expected {\"results\": [...]} or an array of results")?,
    };
    let mut report = SubmitReport::default();
    let mut seen = std::collections::HashSet::new();
    for r in results {
        // Expand and verify results are keyed by "id"; consolidate results by "pair", shown
        // as "a + b" with the smaller id first.
        let key = match task {
            WorkTask::Consolidate => result_pair(r).map(|(a, b)| format!("{a} + {b}")),
            _ => r.get("id").and_then(Value::as_str).map(str::to_string),
        };
        let Some(id) = key else {
            report.skipped += 1;
            report.lines.push(match task {
                WorkTask::Consolidate => "result without a pair of two different ids".into(),
                _ => "result without an id".into(),
            });
            continue;
        };
        if !seen.insert(id.clone()) {
            report.skipped += 1;
            report.lines.push(format!("{id}: duplicate answer ignored"));
            continue;
        }
        let outcome = match task {
            WorkTask::Expand => apply_keywords(store, &id, r),
            WorkTask::Verify => apply_verdict(store, &id, r),
            WorkTask::Consolidate => apply_pair(store, r),
        };
        match outcome {
            Ok(Outcome::Applied(line)) => {
                report.applied += 1;
                report.lines.push(format!("{id}: {line}"));
            }
            Ok(Outcome::Skipped(line)) => {
                report.skipped += 1;
                report.lines.push(format!("{id}: {line}"));
            }
            Err(e) => {
                report.skipped += 1;
                report.lines.push(format!("{id}: skipped ({e:#})"));
            }
        }
    }
    Ok(report)
}

enum Outcome {
    Applied(String),
    Skipped(String),
}

/// Normalise one keyword: collapse whitespace, keep word characters and a few joiners, cap
/// the length. Returns None for anything that isn't a plain search term.
fn clean_keyword(raw: &str) -> Option<String> {
    let words: Vec<&str> = raw.split_whitespace().collect();
    if words.is_empty() || words.len() > 4 {
        return None;
    }
    let term = words.join(" ");
    let ok = term
        .chars()
        .all(|c| c.is_alphanumeric() || " -_./#+:".contains(c));
    (ok && term.chars().count() <= MAX_KEYWORD_CHARS).then_some(term)
}

fn apply_keywords(store: &mut MemoryStore, id: &str, r: &Value) -> Result<Outcome> {
    // Only memories that are waiting in the queue: a worker can't rewrite the keywords of
    // ones it was never given.
    if !store.needs_keywords(id)? {
        bail!("not waiting for keywords");
    }
    let raw = r
        .get("keywords")
        .and_then(Value::as_array)
        .context("missing 'keywords' list")?;
    let mut keywords: Vec<String> = Vec::new();
    for k in raw
        .iter()
        .filter_map(Value::as_str)
        .filter_map(clean_keyword)
    {
        if !keywords.iter().any(|e| e.eq_ignore_ascii_case(&k)) {
            keywords.push(k);
        }
        if keywords.len() == MAX_KEYWORDS {
            break;
        }
    }
    store.set_keywords(id, &keywords)?;
    Ok(Outcome::Applied(format!("{} keywords", keywords.len())))
}

fn apply_verdict(store: &mut MemoryStore, id: &str, r: &Value) -> Result<Outcome> {
    let verdict = r.get("verdict").and_then(Value::as_str).unwrap_or("unsure");
    let reason = r.get("reason").and_then(Value::as_str).unwrap_or("").trim();
    // Only what a verify batch could contain may change: an active, non-scratch memory that
    // is stale right now. A repeated, late or invented answer therefore does nothing.
    let (mem, _) = store
        .get_memory(id)?
        .with_context(|| format!("memory '{id}' not found"))?;
    if mem.status != MemoryStatus::Active || mem.kind == MemoryKind::Ephemeral {
        bail!("not an active memory ({}, {})", mem.kind, mem.status);
    }
    if store.stale_paths(&mem).is_empty() {
        bail!("not stale (already handled?)");
    }
    match verdict {
        "valid" => {
            let m = store.confirm_memory(id)?;
            Ok(Outcome::Applied(if m.id == id {
                "confirmed; cited files re-baselined".into()
            } else {
                format!("confirmed; re-recorded as {}", m.id)
            }))
        }
        "outdated" => {
            let correction = r.get("correction");
            let body = correction
                .and_then(|c| c.get("body"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .context("verdict 'outdated' needs correction.body")?;
            let title = correction
                .and_then(|c| c.get("title"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .unwrap_or(&mem.title);
            // Say where the text came from, so a later reader can tell a worker's correction
            // from a decision someone made.
            let mut body = body.to_string();
            if !reason.is_empty() {
                body.push_str(&format!("\n\nCorrected by a worker model: {reason}"));
            }
            let tags = match mem.tags.as_deref() {
                Some(t) if !t.trim().is_empty() => format!("{t} worker-corrected"),
                _ => "worker-corrected".to_string(),
            };
            let new = store.record_memory(RecordOptions {
                id: None,
                kind: mem.kind,
                title: title.to_string(),
                body,
                tags: Some(tags),
                supersedes_id: Some(id.to_string()),
                session_id: None,
                embedding: None,
                paths: mem.paths.clone(),
                commit: None,
            })?;
            Ok(Outcome::Applied(format!(
                "outdated; superseded by {}",
                new.id
            )))
        }
        _ => Ok(Outcome::Skipped(if reason.is_empty() {
            "unsure; left as stale".into()
        } else {
            format!("unsure; left as stale ({reason})")
        })),
    }
}

/// The two ids of a consolidate result, smaller first, or None unless "pair" holds two
/// different ids.
fn result_pair(r: &Value) -> Option<(String, String)> {
    let pair = r.get("pair")?.as_array()?;
    let [a, b] = pair.as_slice() else {
        return None;
    };
    let (a, b) = (a.as_str()?.trim(), b.as_str()?.trim());
    if a.is_empty() || b.is_empty() || a == b {
        return None;
    }
    Some(if a < b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    })
}

fn apply_pair(store: &mut MemoryStore, r: &Value) -> Result<Outcome> {
    let (a_id, b_id) = result_pair(r).context("missing 'pair'")?;
    let verdict = r
        .get("verdict")
        .and_then(Value::as_str)
        .map(str::trim)
        .context("missing 'verdict'")?;
    if !["duplicate", "conflict", "related", "distinct"].contains(&verdict) {
        bail!("unknown verdict '{}'", truncate(verdict, 30));
    }
    let reason = r.get("reason").and_then(Value::as_str).unwrap_or("").trim();
    // Only a pair a consolidate batch could contain right now may change anything: an
    // invented, already judged or late answer (one of them resolved since) does nothing.
    if let Some(v) = store.pair_review(&a_id, &b_id)? {
        bail!("already reviewed ({v})");
    }
    let fetch = |id: &str| -> Result<Memory> {
        Ok(store
            .get_memory(id)?
            .with_context(|| format!("memory '{id}' not found"))?
            .0)
    };
    let (a, b) = (fetch(&a_id)?, fetch(&b_id)?);
    if !store.is_consolidation_candidate(&a, &b)? {
        bail!(
            "not a consolidation candidate (both must be active decisions, invariants or learnings with similar titles)"
        );
    }
    let line = match verdict {
        "duplicate" => {
            let keep = r
                .get("keep")
                .and_then(Value::as_str)
                .map(str::trim)
                .context("verdict 'duplicate' needs 'keep'")?;
            let (keep_mem, drop_mem) = if keep == a.id {
                (&a, &b)
            } else if keep == b.id {
                (&b, &a)
            } else {
                bail!("'keep' must be one of the pair");
            };
            // Resolve only where it sticks. Hiding an ADR or commit the repository still says
            // is current (or one that comes back on the next import) would be wrong, so prefer
            // resolving the other one, or leave the call to a person.
            let (keep_mem, drop_mem) = if store.resolution_persists(drop_mem) {
                (keep_mem, drop_mem)
            } else if store.resolution_persists(keep_mem) {
                (drop_mem, keep_mem)
            } else {
                let task = store.record_memory(RecordOptions {
                    id: None,
                    kind: MemoryKind::Task,
                    title: format!("Possible duplicate memories {} and {}", a.id, b.id),
                    body: format!(
                        "A worker model found that these say the same thing:\n\n- {}: {}\n- {}: {}\n\n\
                         Both come from repository files, so memdag didn't hide either. Merge them \
                         at the source (e.g. supersede one ADR with the other).{}",
                        a.id,
                        a.title,
                        b.id,
                        b.title,
                        if reason.is_empty() { String::new() } else { format!("\n\nWorker's reason: {reason}") }
                    ),
                    tags: Some("worker-duplicate".into()),
                    supersedes_id: None,
                    session_id: None,
                    embedding: None,
                    paths: Vec::new(),
                    commit: None,
                })?;
                for m in [&a.id, &b.id] {
                    store.link_entities(&task.id, m, RelationType::References)?;
                }
                store.mark_pair_reviewed(&a.id, &b.id, "duplicate")?;
                return Ok(Outcome::Applied(format!(
                    "duplicate of two repository-backed memories; recorded task {}",
                    task.id
                )));
            };
            let (keep, drop) = (keep_mem.id.as_str(), drop_mem.id.as_str());
            let note = if reason.is_empty() {
                format!("Duplicate of {keep} (found by a worker model)")
            } else {
                format!("Duplicate of {keep} (found by a worker model: {reason})")
            };
            store.resolve_memory(drop, Some(&note))?;
            store.link_entities(drop, keep, RelationType::References)?;
            format!("duplicate; resolved {drop} as duplicate of {keep}")
        }
        "conflict" => {
            let mut body = format!(
                "A worker model found that these memories contradict each other:\n\n\
                 - {}: {}\n- {}: {}\n",
                a.id, a.title, b.id, b.title
            );
            if !reason.is_empty() {
                body.push_str(&format!("\nWorker's reason: {reason}\n"));
            }
            body.push_str("\nDecide which one is current, then supersede or resolve the other.");
            let task = store.record_memory(RecordOptions {
                id: None,
                kind: MemoryKind::Task,
                title: format!("Reconcile conflicting memories {} and {}", a.id, b.id),
                body,
                tags: Some("worker-conflict".into()),
                supersedes_id: None,
                session_id: None,
                embedding: None,
                paths: Vec::new(),
                commit: None,
            })?;
            store.link_entities(&task.id, &a.id, RelationType::References)?;
            store.link_entities(&task.id, &b.id, RelationType::References)?;
            format!("conflict; recorded task {}", task.id)
        }
        "related" => {
            store.link_entities(&a.id, &b.id, RelationType::References)?;
            format!("related; linked {} to {}", a.id, b.id)
        }
        _ => "distinct; nothing to do".to_string(),
    };
    store.mark_pair_reviewed(&a.id, &b.id, verdict)?;
    Ok(Outcome::Applied(line))
}

/// An MCP prompt / slash command that drives one task end to end from the host agent.
pub struct HostPrompt {
    pub name: &'static str,
    pub task: WorkTask,
    pub description: &'static str,
}

pub const HOST_PROMPTS: &[HostPrompt] = &[
    HostPrompt {
        name: "expand_keywords",
        task: WorkTask::Expand,
        description: "Have a cheap model (Haiku/Flash) add search keywords to memories, so keyword search finds them without an embedding model.",
    },
    HostPrompt {
        name: "verify_stale",
        task: WorkTask::Verify,
        description: "Have a cheap model (Haiku/Flash) check STALE memories against the code they cite, then confirm or correct them.",
    },
    HostPrompt {
        name: "consolidate_memories",
        task: WorkTask::Consolidate,
        description: "Have a cheap model (Haiku/Flash) find duplicate or conflicting memories: duplicates are resolved, conflicts become tasks, related ones are linked.",
    },
];

pub fn host_prompt(name: &str) -> Option<&'static HostPrompt> {
    HOST_PROMPTS
        .iter()
        .find(|p| p.name == name || p.task.as_str() == name)
}

/// Default number of batches one prompt run works through.
pub const DEFAULT_ROUNDS: usize = 3;

/// The text of a host prompt: how the calling agent runs `task` on a cheap worker model.
pub fn host_prompt_text(task: WorkTask, rounds: usize) -> String {
    let (goal, review, worker_review) = match task {
        WorkTask::Expand => ("add search keywords to memories that have none yet", "", ""),
        WorkTask::Verify => (
            "check memories marked STALE against the files they cite",
            "\n   Before submitting, review every \"outdated\" verdict. If its correction changes what a decision means, open the cited file and confirm it, or change the verdict to \"unsure\". The worker needs read access to the repository.",
            " Corrections it submits become new memories that supersede the old ones (the old text stays in history); list any it reports so the user can check them.",
        ),
        WorkTask::Consolidate => (
            "find duplicate or conflicting memories",
            "\n   Before submitting, review every \"duplicate\" verdict: if the two entries differ in substance, change it to \"related\".",
            " Conflicts it reports become tasks (\"Reconcile conflicting memories ...\"); mention each one so the user can decide which memory is current.",
        ),
    };
    format!(
        r#"Maintain the memdag project memory: {goal}. This is bulk work for a fast, low-cost model; do not do it with your own (larger) model unless no cheaper one is available.

If a `memdag-worker` subagent is available (installed by `memdag work install`), start it with: "task: {task}, rounds: {rounds}". It fetches, processes and submits the batches itself. Relay its summary in one or two lines and stop.{worker_review}

Otherwise:
1. Call the memdag tool `get_work` with task="{task}" and limit={DEFAULT_BATCH}.
2. If "items" is empty, stop and say nothing is waiting.
3. Delegate the batch to a subagent on a cheap model (Claude Code: model "haiku"; OpenCode: an agent configured with a Haiku- or Flash-class model; elsewhere: the cheapest model available, and only if there is none, yourself, briefly). Give it the batch's "instructions" and "items" exactly as returned and ask for the JSON answer only.{review}
4. Call `submit_work` with task="{task}" and results set to the worker's JSON "results".
5. Repeat while "remaining" is above 0, at most {rounds} batches in total. Finish with one or two lines: how many items were applied and skipped."#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_and_chatty_replies() {
        let v = parse_reply("Sure!\n```json\n{\"results\": []}\n```\nDone.").unwrap();
        assert!(v["results"].is_array());
        assert!(parse_reply("[{\"id\":\"x\"}]").unwrap().is_array());
        assert!(parse_reply("no json here").is_err());
    }

    #[test]
    fn keywords_are_plain_terms() {
        assert_eq!(
            clean_keyword("  write-ahead   log "),
            Some("write-ahead log".into())
        );
        assert_eq!(clean_keyword("SQLITE_BUSY"), Some("SQLITE_BUSY".into()));
        assert_eq!(clean_keyword("this is far too long a phrase"), None);
        assert_eq!(clean_keyword("<script>"), None);
        assert_eq!(clean_keyword(""), None);
    }

    #[test]
    fn task_names() {
        assert_eq!(
            "verify_stale".parse::<WorkTask>().unwrap(),
            WorkTask::Verify
        );
        assert_eq!("Expand".parse::<WorkTask>().unwrap(), WorkTask::Expand);
        assert!("nope".parse::<WorkTask>().is_err());
    }
}
