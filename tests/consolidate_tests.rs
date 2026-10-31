//! The `consolidate` work task: a cheap worker model judges pairs of similar durable memories
//! as duplicate, conflict, related or distinct.

use memdag::db::open_in_memory;
use memdag::import::{CommentSource, GhComment, ImportOptions, import_repo};
use memdag::models::{EdgeDirection, MemoryKind, MemoryStatus, RelationType};
use memdag::store::{MemoryStore, RecordOptions};
use memdag::work::{self, WorkTask};
use serde_json::json;
use std::path::Path;
use std::process::Command;

fn opts(kind: MemoryKind, title: &str, body: &str) -> RecordOptions {
    RecordOptions {
        id: None,
        kind,
        title: title.into(),
        body: body.into(),
        tags: None,
        supersedes_id: None,
        session_id: None,
        embedding: None,
        paths: Vec::new(),
        commit: None,
    }
}

fn record(store: &mut MemoryStore, kind: MemoryKind, title: &str, body: &str) -> String {
    store.record_memory(opts(kind, title, body)).unwrap().id
}

/// Two near-duplicate decisions plus an unrelated learning.
fn setup(store: &mut MemoryStore) -> (String, String, String) {
    let a = record(
        store,
        MemoryKind::Decision,
        "SQLite busy timeout is five seconds",
        "Connections wait 5 s on a locked database.",
    );
    let b = record(
        store,
        MemoryKind::Decision,
        "Busy timeout for SQLite connections",
        "We wait five seconds when the database is locked.",
    );
    let c = record(
        store,
        MemoryKind::Learning,
        "Gzip the export",
        "Smaller release artefacts.",
    );
    (a, b, c)
}

fn pairs(store: &MemoryStore) -> Vec<(String, String)> {
    work::next_batch(store, WorkTask::Consolidate, 20).unwrap()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["pair"][0].as_str().unwrap().to_string(),
                i["pair"][1].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn ordered(a: &str, b: &str) -> (String, String) {
    if a < b {
        (a.into(), b.into())
    } else {
        (b.into(), a.into())
    }
}

#[test]
fn near_duplicates_are_a_candidate_and_a_duplicate_verdict_resolves_one() {
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    let (a, b, c) = setup(&mut store);
    let (lo, hi) = ordered(&a, &b);

    let batch = work::next_batch(&store, WorkTask::Consolidate, 20).unwrap();
    assert!(
        batch["instructions"]
            .as_str()
            .unwrap()
            .contains("ONLY this JSON")
    );
    let item = &batch["items"][0];
    assert_eq!(item["pair"], json!([lo, hi]));
    assert_eq!(item["a"]["id"], lo.as_str());
    assert_eq!(item["b"]["status"], "active");
    assert!(item["a"]["body"].is_string() && item["b"]["title"].is_string());
    assert!(
        !pairs(&store).iter().any(|(x, y)| x == &c || y == &c),
        "the unrelated learning is not paired"
    );

    let reply = work::parse_reply(&format!(
        "```json\n{{\"task\":\"consolidate\",\"results\":[{{\"pair\":[\"{hi}\",\"{lo}\"],\"verdict\":\"duplicate\",\"keep\":\"{a}\",\"reason\":\"same timeout\"}}]}}\n```"
    ))
    .unwrap();
    let task = work::reply_task(&reply, None).unwrap();
    assert_eq!(task, WorkTask::Consolidate);
    let r = work::submit(&mut store, task, &reply).unwrap();
    assert_eq!((r.applied, r.skipped), (1, 0), "{r}");
    assert!(
        r.to_string().contains(&format!(
            "{lo} + {hi}: duplicate; resolved {b} as duplicate of {a}"
        )),
        "{r}"
    );

    let (dropped, rels) = store.get_memory(&b).unwrap().unwrap();
    assert_eq!(dropped.status, MemoryStatus::Resolved);
    assert!(
        dropped.body.contains(&format!(
            "Duplicate of {a} (found by a worker model: same timeout)"
        )),
        "{}",
        dropped.body
    );
    assert!(rels.iter().any(|r| r.target_id == a
        && r.relation_type == RelationType::References
        && r.direction == EdgeDirection::Outgoing));
    let (kept, _) = store.get_memory(&a).unwrap().unwrap();
    assert_eq!(kept.status, MemoryStatus::Active);

    assert!(pairs(&store).is_empty(), "never offered again");
    assert_eq!(
        store.pair_review(&a, &b).unwrap().as_deref(),
        Some("duplicate")
    );
    // A repeated answer is refused.
    let r = work::submit(&mut store, task, &reply).unwrap();
    assert_eq!((r.applied, r.skipped), (0, 1), "{r}");
}

#[test]
fn a_conflict_becomes_a_task_linked_to_both_and_changes_neither() {
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    let (a, b, _) = setup(&mut store);
    let reply =
        json!({"results": [{"pair": [a, b], "verdict": "conflict", "reason": "5 s vs 8 s"}]});
    let r = work::submit(&mut store, WorkTask::Consolidate, &reply).unwrap();
    assert_eq!(r.applied, 1, "{r}");

    let tasks = store.list_ready(10).unwrap();
    assert_eq!(tasks.len(), 1);
    let t = &tasks[0];
    assert_eq!(t.kind, MemoryKind::Task);
    let (lo, hi) = ordered(&a, &b);
    assert_eq!(
        t.title,
        format!("Reconcile conflicting memories {lo} and {hi}")
    );
    assert!(t.body.contains("5 s vs 8 s") && t.body.contains("Busy timeout for SQLite"));
    assert!(t.tags.as_deref().unwrap().contains("worker-conflict"));
    let (_, rels) = store.get_memory(&t.id).unwrap().unwrap();
    for id in [&a, &b] {
        assert!(
            rels.iter()
                .any(|r| &r.target_id == id && r.relation_type == RelationType::References)
        );
        let (m, _) = store.get_memory(id).unwrap().unwrap();
        assert_eq!(m.status, MemoryStatus::Active);
    }
    assert!(pairs(&store).is_empty());
}

#[test]
fn related_links_and_distinct_only_marks_reviewed() {
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    let (a, b, _) = setup(&mut store);
    let r = work::submit(
        &mut store,
        WorkTask::Consolidate,
        &json!([{"pair": [a, b], "verdict": "related", "reason": "same topic"}]),
    )
    .unwrap();
    assert_eq!(r.applied, 1, "{r}");
    let (_, rels) = store.get_memory(&a).unwrap().unwrap();
    assert!(rels.iter().any(|r| r.target_id == b));
    assert!(pairs(&store).is_empty());

    let mut store = MemoryStore::new(open_in_memory().unwrap());
    let (a, b, _) = setup(&mut store);
    let r = work::submit(
        &mut store,
        WorkTask::Consolidate,
        &json!([{"pair": [a, b], "verdict": "distinct"}]),
    )
    .unwrap();
    assert_eq!(r.applied, 1, "{r}");
    let (_, rels) = store.get_memory(&a).unwrap().unwrap();
    assert!(rels.is_empty());
    assert!(pairs(&store).is_empty());
}

#[test]
fn non_candidates_and_bad_answers_are_skipped() {
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    let (a, b, c) = setup(&mut store);
    let task = record(&mut store, MemoryKind::Task, "SQLite busy timeout", "t");
    let reply = json!({"results": [
        {"pair": [a, c], "verdict": "duplicate", "keep": a, "reason": "x"},
        {"pair": [a, task], "verdict": "related"},
        {"pair": [a, b], "verdict": "duplicate", "keep": c},
        {"pair": [a, b], "verdict": "duplicate"},
        {"pair": [a, b], "verdict": "merge"},
        {"pair": [a, "DEC-missing"], "verdict": "related"},
        {"pair": [a, a], "verdict": "related"},
        {"pair": [a], "verdict": "related"},
        {"id": a, "verdict": "related"},
    ]});
    let r = work::submit(&mut store, WorkTask::Consolidate, &reply).unwrap();
    assert_eq!((r.applied, r.skipped), (0, 9), "{r}");
    assert!(
        r.to_string().contains("not a consolidation candidate"),
        "{r}"
    );
    assert!(
        r.to_string().contains("'keep' must be one of the pair"),
        "{r}"
    );
    for id in [&a, &b, &c] {
        let (m, rels) = store.get_memory(id).unwrap().unwrap();
        assert_eq!(m.status, MemoryStatus::Active);
        assert!(rels.is_empty(), "{id}: {rels:?}");
    }
    assert_eq!(pairs(&store).len(), 1, "still waiting");
}

#[test]
fn a_pair_repeated_in_one_reply_is_applied_once() {
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    let (a, b, _) = setup(&mut store);
    let reply = json!({"results": [
        {"pair": [a, b], "verdict": "conflict", "reason": "r"},
        {"pair": [b, a], "verdict": "conflict", "reason": "r"},
    ]});
    let r = work::submit(&mut store, WorkTask::Consolidate, &reply).unwrap();
    assert_eq!((r.applied, r.skipped), (1, 1), "{r}");
    assert!(r.to_string().contains("duplicate answer ignored"));
    assert_eq!(store.list_ready(10).unwrap().len(), 1);
}

// ------------------------------------------------------------ write-through

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct NoGh;
impl CommentSource for NoGh {
    fn fetch_comments(&self, _: &Path) -> anyhow::Result<Vec<GhComment>> {
        Ok(Vec::new())
    }
}

#[test]
fn a_resolved_duplicate_note_survives_a_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@example.com"]);
    git(repo, &["config", "user.name", "Test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
    let notes = repo.join("docs/memory");
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    store.set_repo_root(Some(repo.to_path_buf()));
    store.set_write_through(Some(notes.clone()));
    let (a, b, _) = setup(&mut store);
    assert!(notes.join(format!("{b}.md")).is_file());

    let reply = json!([{"pair": [a, b], "verdict": "duplicate", "keep": a, "reason": "same"}]);
    let r = work::submit(&mut store, WorkTask::Consolidate, &reply).unwrap();
    assert_eq!(r.applied, 1, "{r}");
    assert!(notes.join(format!("{b}.resolved.md")).is_file());
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "notes"]);

    let mut fresh = MemoryStore::new(open_in_memory().unwrap());
    fresh.set_repo_root(Some(repo.to_path_buf()));
    let files = ImportOptions {
        git: false,
        gh: false,
        ..ImportOptions::all()
    };
    import_repo(&mut fresh, repo, &files, &NoGh).unwrap();
    let (m, _) = fresh.get_memory(&b).unwrap().unwrap();
    assert_eq!(m.status, MemoryStatus::Resolved);
    assert!(m.body.contains(&format!("Duplicate of {a}")), "{}", m.body);
    let (m, _) = fresh.get_memory(&a).unwrap().unwrap();
    assert_eq!(m.status, MemoryStatus::Active);
}

#[test]
fn task_names_and_mcp_prompt() {
    assert_eq!("dedupe".parse::<WorkTask>().unwrap(), WorkTask::Consolidate);
    assert_eq!(
        "consolidate".parse::<WorkTask>().unwrap(),
        WorkTask::Consolidate
    );
    let text = work::host_prompt_text(WorkTask::Consolidate, 2);
    assert!(text.contains("find duplicate or conflicting memories"));
    assert!(text.contains("task=\"consolidate\"") && text.contains("Conflicts"));
    let p = work::host_prompt("consolidate_memories").unwrap();
    assert_eq!(p.task, WorkTask::Consolidate);
    assert!(p.description.contains("duplicate"));
}

#[test]
fn mcp_get_and_submit_consolidate_work() {
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    let (a, b, _) = setup(&mut store);
    let mcp = memdag::McpServer::new(store);
    let call = |method: &str, params: serde_json::Value| {
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let resp = mcp
            .handle_request(serde_json::from_value(req).unwrap())
            .unwrap();
        assert!(resp.error.is_none(), "{:?}", resp.error);
        resp.result.unwrap()
    };
    let tools = call("tools/list", json!({}));
    let get_work = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "get_work")
        .unwrap();
    assert!(
        get_work["inputSchema"]["properties"]["task"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("consolidate"))
    );
    let out = call(
        "tools/call",
        json!({"name": "get_work", "arguments": {"task": "consolidate"}}),
    );
    let batch: serde_json::Value =
        serde_json::from_str(out["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(batch["items"].as_array().unwrap().len(), 1);
    let out = call(
        "tools/call",
        json!({"name": "submit_work", "arguments": {"task": "consolidate",
            "results": [{"pair": [a, b], "verdict": "related", "reason": "same topic"}]}}),
    );
    assert!(
        out["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Applied 1"),
        "{out}"
    );
}

/// Two ADRs that say the same thing, plus a local decision that repeats them.
fn adr_setup() -> (tempfile::TempDir, MemoryStore, String) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    git(repo, &["init", "-q"]);
    for (n, title) in [
        ("0001", "SQLite busy timeout is five seconds"),
        ("0002", "Busy timeout for SQLite connections"),
    ] {
        let path = repo.join(format!("docs/adr/{n}-busy.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("# {n}. {title}\n- Status: accepted\n\nWe wait five seconds when SQLite is locked.\n")).unwrap();
    }
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    let files = ImportOptions {
        git: false,
        gh: false,
        ..ImportOptions::all()
    };
    import_repo(&mut store, repo, &files, &NoGh).unwrap();
    let local = record(
        &mut store,
        MemoryKind::Decision,
        "SQLite busy timeout five seconds",
        "Connections wait five seconds on a locked SQLite database.",
    );
    (tmp, store, local)
}

#[test]
fn two_repository_backed_duplicates_become_a_task_not_a_hidden_adr() {
    let (_tmp, mut store, _local) = adr_setup();
    let (a, b) = ordered("ADR-0001", "ADR-0002");
    let reply = json!({"results": [{"pair": [a, b], "verdict": "duplicate", "keep": "ADR-0001", "reason": "same"}]});
    let r = work::submit(&mut store, WorkTask::Consolidate, &reply).unwrap();
    assert_eq!(r.applied, 1, "{r}");
    assert!(r.to_string().contains("recorded task"), "{r}");
    for id in ["ADR-0001", "ADR-0002"] {
        assert_eq!(
            store.get_memory(id).unwrap().unwrap().0.status,
            MemoryStatus::Active,
            "{id}"
        );
    }
}

#[test]
fn a_duplicate_of_an_adr_resolves_the_local_copy_whatever_keep_says() {
    let (_tmp, mut store, local) = adr_setup();
    let (a, b) = ordered("ADR-0001", &local);
    let reply = json!({"results": [{"pair": [a, b], "verdict": "duplicate", "keep": local, "reason": "same"}]});
    let r = work::submit(&mut store, WorkTask::Consolidate, &reply).unwrap();
    assert_eq!(r.applied, 1, "{r}");
    assert_eq!(
        store.get_memory("ADR-0001").unwrap().unwrap().0.status,
        MemoryStatus::Active
    );
    assert_eq!(
        store.get_memory(&local).unwrap().unwrap().0.status,
        MemoryStatus::Resolved
    );
}
