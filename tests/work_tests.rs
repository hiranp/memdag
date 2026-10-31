//! The LLM-worker work queue: keyword expansion and STALE verification, via the library,
//! the MCP tools and the MCP prompts.

use memdag::McpServer;
use memdag::db::open_in_memory;
use memdag::import::{CommentSource, GhComment, ImportOptions, import_repo};
use memdag::models::{MemoryKind, MemoryStatus};
use memdag::store::{MemoryStore, RecordOptions, SearchOptions};
use memdag::work::{self, WorkTask};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

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
fn init_repo(dir: &Path) {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
}
fn commit_all(repo: &Path, msg: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "--allow-empty", "-m", msg]);
}
fn write(repo: &Path, rel: &str, text: &str) {
    let p = repo.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}
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
fn hits(store: &MemoryStore, q: &str) -> Vec<String> {
    store
        .search_memory(SearchOptions {
            query: q.into(),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .map(|r| r.memory.id)
        .collect()
}
struct NoGh;
impl CommentSource for NoGh {
    fn fetch_comments(&self, _: &Path) -> anyhow::Result<Vec<GhComment>> {
        Ok(Vec::new())
    }
}

// ------------------------------------------------------------------- expand

#[test]
fn keywords_make_a_memory_findable_by_words_it_does_not_contain() {
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    let m = store
        .record_memory(opts(
            MemoryKind::Decision,
            "Use WAL journal mode",
            "Readers never block the writer.",
        ))
        .unwrap();
    store
        .record_memory(opts(MemoryKind::Task, "Unrelated", "nothing here"))
        .unwrap();
    assert!(hits(&store, "concurrency").is_empty());

    let batch = work::next_batch(&store, WorkTask::Expand, 1).unwrap();
    assert_eq!(batch["items"].as_array().unwrap().len(), 1);
    assert_eq!(batch["remaining"], 1);
    assert!(
        batch["instructions"]
            .as_str()
            .unwrap()
            .contains("ONLY this JSON")
    );

    let reply = work::parse_reply(&format!(
        "Here you go:\n```json\n{{\"task\":\"expand\",\"results\":[{{\"id\":\"{}\",\"keywords\":[\"concurrency\",\"write-ahead log\",\"SQLITE_BUSY\",\"<b>bad</b>\",\"concurrency\"]}}]}}\n```",
        m.id
    ))
    .unwrap();
    let task = work::reply_task(&reply, None).unwrap();
    let report = work::submit(&mut store, task, &reply).unwrap();
    assert_eq!(report.applied, 1, "{report}");
    assert_eq!(
        store.get_keywords(&m.id).unwrap().as_deref(),
        Some("concurrency, write-ahead log, SQLITE_BUSY")
    );

    assert_eq!(hits(&store, "concurrency"), vec![m.id.clone()]);
    // One query can match the body and the keywords together.
    assert_eq!(hits(&store, "readers concurrency"), vec![m.id.clone()]);
    // Expanded memories leave the queue.
    let ids: Vec<String> = work::next_batch(&store, WorkTask::Expand, 10).unwrap()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_string())
        .collect();
    assert!(!ids.contains(&m.id));
}

#[test]
fn own_text_outranks_a_keyword_match() {
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    let direct = store
        .record_memory(opts(MemoryKind::Decision, "Retry on lock contention", "x"))
        .unwrap();
    let expanded = store
        .record_memory(opts(MemoryKind::Decision, "Busy timeout", "y"))
        .unwrap();
    store
        .set_keywords(&expanded.id, &["lock contention".into()])
        .unwrap();
    assert_eq!(hits(&store, "contention"), vec![direct.id, expanded.id]);
}

#[test]
fn worker_secrets_are_refused_and_bad_items_do_not_sink_the_batch() {
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    let a = store
        .record_memory(opts(MemoryKind::Decision, "a", "a"))
        .unwrap();
    let reply = json!({"results": [
        {"id": a.id, "keywords": ["ghp_abcdefghijklmnopqrstuvwxyz0123456789"]},
        {"id": "DEC-missing", "keywords": ["x"]},
        {"keywords": ["no id"]},
    ]});
    let report = work::submit(&mut store, WorkTask::Expand, &reply).unwrap();
    assert_eq!((report.applied, report.skipped), (0, 3), "{report}");
    assert!(store.get_keywords(&a.id).unwrap().is_none());
    assert!(
        !report.to_string().contains("ghp_"),
        "never echo the secret"
    );
}

#[test]
fn keywords_survive_reimport_and_resolution_but_not_a_rewrite() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    let note = |body: &str| {
        format!(
            "---\nid: BLK-x\nkind: blocker\ntags: []\nsupersedes:\nrefs: []\n---\n# Flaky CI\n\n{body}\n"
        )
    };
    write(repo, "docs/memory/BLK-x.md", &note("keychain prompt"));
    commit_all(repo, "c");
    let files = ImportOptions {
        git: false,
        gh: false,
        ..ImportOptions::all()
    };
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    import_repo(&mut store, repo, &files, &NoGh).unwrap();
    store.set_keywords("BLK-x", &["macos".into()]).unwrap();

    import_repo(&mut store, repo, &files, &NoGh).unwrap();
    assert!(
        store.get_keywords("BLK-x").unwrap().is_some(),
        "unchanged re-import"
    );
    store.resolve_memory("BLK-x", Some("pinned image")).unwrap();
    assert!(
        store.get_keywords("BLK-x").unwrap().is_some(),
        "appending keeps them"
    );

    write(repo, "docs/memory/BLK-x.md", &note("totally different now"));
    import_repo(&mut store, repo, &files, &NoGh).unwrap();
    assert!(
        store.get_keywords("BLK-x").unwrap().is_none(),
        "rewrite re-queues"
    );
}

// ------------------------------------------------------------------- verify

fn stale_setup(tmp: &tempfile::TempDir, write_through: bool) -> (MemoryStore, String) {
    let repo = tmp.path();
    init_repo(repo);
    write(repo, "src/db.rs", "const TIMEOUT_MS: u32 = 5000;\n");
    commit_all(repo, "init");
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    store.set_repo_root(Some(repo.to_path_buf()));
    if write_through {
        store.set_write_through(Some(repo.join("docs/memory")));
    }
    let mut o = opts(
        MemoryKind::Decision,
        "Busy timeout is 5 s",
        "TIMEOUT_MS is 5000.",
    );
    o.paths = vec!["src/db.rs".into()];
    let m = store.record_memory(o).unwrap();
    write(
        repo,
        "src/db.rs",
        "// tidy\nconst TIMEOUT_MS: u32 = 5000;\n",
    );
    (store, m.id)
}

#[test]
fn verify_batch_lists_stale_memories_with_their_changed_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, id) = stale_setup(&tmp, false);
    let batch = work::next_batch(&store, WorkTask::Verify, 5).unwrap();
    let item = &batch["items"][0];
    assert_eq!(item["id"], id.as_str());
    assert_eq!(item["stale_paths"], json!(["src/db.rs"]));
}

#[test]
fn valid_verdict_rebaselines_and_unsure_leaves_it_stale() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut store, id) = stale_setup(&tmp, false);
    let unsure = json!({"results": [{"id": id, "verdict": "unsure", "reason": "can't tell"}]});
    let r = work::submit(&mut store, WorkTask::Verify, &unsure).unwrap();
    assert_eq!((r.applied, r.skipped), (0, 1));
    assert_eq!(store.list_stale().unwrap().len(), 1);

    let valid = json!({"results": [{"id": id, "verdict": "valid", "reason": "still 5000"}]});
    let r = work::submit(&mut store, WorkTask::Verify, &valid).unwrap();
    assert_eq!(r.applied, 1, "{r}");
    assert!(store.list_stale().unwrap().is_empty());
    let (m, _) = store.get_memory(&id).unwrap().unwrap();
    assert_eq!(m.status, MemoryStatus::Active);

    // Not stale any more, so a repeated answer is refused rather than applied twice.
    let r = work::submit(&mut store, WorkTask::Verify, &valid).unwrap();
    assert_eq!(r.skipped, 1);
}

#[test]
fn outdated_verdict_supersedes_with_the_correction() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut store, id) = stale_setup(&tmp, false);
    let reply = json!({"results": [{
        "id": id, "verdict": "outdated", "reason": "now 8000",
        "correction": {"title": "Busy timeout is 8 s", "body": "TIMEOUT_MS is 8000."}
    }]});
    let r = work::submit(&mut store, WorkTask::Verify, &reply).unwrap();
    assert_eq!(r.applied, 1, "{r}");
    let (old, rels) = store.get_memory(&id).unwrap().unwrap();
    assert_eq!(old.status, MemoryStatus::Superseded);
    let new_id = &rels[0].target_id;
    let (new, _) = store.get_memory(new_id).unwrap().unwrap();
    assert_eq!(new.title, "Busy timeout is 8 s");
    assert_eq!(new.paths, vec!["src/db.rs"]);
    assert!(store.list_stale().unwrap().is_empty());

    let no_body = json!({"results": [{"id": new_id, "verdict": "outdated"}]});
    assert_eq!(
        work::submit(&mut store, WorkTask::Verify, &no_body)
            .unwrap()
            .applied,
        0
    );
}

#[test]
fn confirming_a_note_backed_memory_writes_a_new_note_that_survives_a_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut store, id) = stale_setup(&tmp, true);
    let reply = json!({"results": [{"id": id, "verdict": "valid", "reason": "ok"}]});
    let r = work::submit(&mut store, WorkTask::Verify, &reply).unwrap();
    assert!(r.to_string().contains("re-recorded"), "{r}");
    commit_all(tmp.path(), "confirm");

    let mut fresh = MemoryStore::new(open_in_memory().unwrap());
    fresh.set_repo_root(Some(tmp.path().to_path_buf()));
    let files = ImportOptions {
        git: false,
        gh: false,
        ..ImportOptions::all()
    };
    import_repo(&mut fresh, tmp.path(), &files, &NoGh).unwrap();
    assert!(
        fresh.list_stale().unwrap().is_empty(),
        "rebuilt index is fresh"
    );
    let (old, _) = fresh.get_memory(&id).unwrap().unwrap();
    assert_eq!(old.status, MemoryStatus::Superseded);
}

// --------------------------------------------------------------------- MCP

fn call(mcp: &McpServer, method: &str, params: Value) -> Value {
    let req = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let resp = mcp
        .handle_request(serde_json::from_value(req).unwrap())
        .unwrap();
    assert!(resp.error.is_none(), "{:?}", resp.error);
    resp.result.unwrap()
}

#[test]
fn mcp_prompts_and_work_tools_round_trip() {
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    let m = store
        .record_memory(opts(
            MemoryKind::Learning,
            "Gzip the export",
            "smaller files",
        ))
        .unwrap();
    let mcp = McpServer::new(store);

    let init = call(&mcp, "initialize", json!({}));
    assert!(init["capabilities"]["prompts"].is_object());
    let names: Vec<String> = call(&mcp, "prompts/list", json!({}))["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["expand_keywords", "verify_stale"]);
    let prompt = call(
        &mcp,
        "prompts/get",
        json!({"name": "verify_stale", "arguments": {"rounds": "2"}}),
    );
    let text = prompt["messages"][0]["content"]["text"].as_str().unwrap();
    assert!(
        text.contains("get_work") && text.contains("haiku") && text.contains("at most 2 batches")
    );

    let tools = call(&mcp, "tools/list", json!({}));
    let tool_names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(tool_names.contains(&"get_work") && tool_names.contains(&"submit_work"));

    let out = call(
        &mcp,
        "tools/call",
        json!({"name": "get_work", "arguments": {"task": "expand"}}),
    );
    let batch: Value = serde_json::from_str(out["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(batch["items"][0]["id"], m.id.as_str());

    let reply = format!(
        "```json\n{{\"results\":[{{\"id\":\"{}\",\"keywords\":[\"compression\"]}}]}}\n```",
        m.id
    );
    let out = call(
        &mcp,
        "tools/call",
        json!({"name": "submit_work", "arguments": {"task": "expand", "reply": reply}}),
    );
    assert_eq!(out["isError"], false, "{out}");
    assert!(
        out["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Applied 1")
    );

    let found = call(
        &mcp,
        "tools/call",
        json!({"name": "search_memory", "arguments": {"query": "compression"}}),
    );
    assert!(
        found["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(&m.id)
    );
}

#[test]
fn unknown_prompt_is_a_json_rpc_error() {
    let mcp = McpServer::new(MemoryStore::new(open_in_memory().unwrap()));
    let req =
        json!({"jsonrpc": "2.0", "id": 1, "method": "prompts/get", "params": {"name": "nope"}});
    let resp = mcp
        .handle_request(serde_json::from_value(req).unwrap())
        .unwrap();
    assert_eq!(resp.error.unwrap().code, -32602);
}

// ------------------------------------------------- review regressions

#[test]
fn a_repeated_or_late_outdated_answer_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut store, id) = stale_setup(&tmp, false);
    let outdated = json!({"id": id, "verdict": "outdated", "reason": "r",
                          "correction": {"body": "TIMEOUT_MS is 8000."}});
    let r = work::submit(
        &mut store,
        WorkTask::Verify,
        &json!({"results": [outdated, outdated]}),
    )
    .unwrap();
    assert_eq!((r.applied, r.skipped), (1, 1), "{r}");
    // Superseded now, though its files still differ: a late answer must not fork the chain.
    let r = work::submit(
        &mut store,
        WorkTask::Verify,
        &json!({"results": [outdated]}),
    )
    .unwrap();
    assert_eq!(r.applied, 0, "{r}");
    let (_, rels) = store.get_memory(&id).unwrap().unwrap();
    assert_eq!(rels.len(), 1, "exactly one successor: {rels:?}");
}

#[test]
fn a_correction_is_labelled_as_worker_output() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut store, id) = stale_setup(&tmp, false);
    let reply = json!({"results": [{"id": id, "verdict": "outdated", "reason": "src/db.rs says 8000",
                                    "correction": {"body": "TIMEOUT_MS is 8000."}}]});
    work::submit(&mut store, WorkTask::Verify, &reply).unwrap();
    let (_, rels) = store.get_memory(&id).unwrap().unwrap();
    let (new, _) = store.get_memory(&rels[0].target_id).unwrap().unwrap();
    assert!(
        new.body
            .ends_with("Corrected by a worker model: src/db.rs says 8000")
    );
    assert!(new.tags.unwrap().contains("worker-corrected"));
}

#[test]
fn the_requested_task_wins_over_the_reply() {
    let reply = json!({"task": "verify", "results": []});
    assert!(work::reply_task(&reply, Some(WorkTask::Expand)).is_err());
    assert_eq!(
        work::reply_task(&reply, Some(WorkTask::Verify)).unwrap(),
        WorkTask::Verify
    );
    assert_eq!(work::reply_task(&reply, None).unwrap(), WorkTask::Verify);
    assert!(work::reply_task(&json!({"results": []}), None).is_err());
}

#[test]
fn workers_can_only_expand_memories_waiting_in_the_queue() {
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    let done = store
        .record_memory(opts(MemoryKind::Decision, "d", "b"))
        .unwrap();
    store.set_keywords(&done.id, &["original".into()]).unwrap();
    let scratch = store
        .record_memory(opts(MemoryKind::Ephemeral, "e", "scratch"))
        .unwrap();
    let reply = json!({"results": [
        {"id": done.id, "keywords": ["hijack"]},
        {"id": scratch.id, "keywords": ["x"]},
    ]});
    let r = work::submit(&mut store, WorkTask::Expand, &reply).unwrap();
    assert_eq!(r.applied, 0, "{r}");
    assert_eq!(
        store.get_keywords(&done.id).unwrap().as_deref(),
        Some("original")
    );
}

#[test]
fn imported_rows_without_write_through_are_not_rebaselined_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    write(repo, "src/a.rs", "fn a() {}\n");
    write(
        repo,
        "docs/memory/DEC-a.md",
        "---\nid: DEC-a\nkind: decision\ntags: []\nsupersedes:\nrefs: []\npaths: [\"src/a.rs\"]\ncommit: 0000000\n---\n# A\n\nabout a\n",
    );
    commit_all(repo, "c");
    write(repo, "src/a.rs", "fn a() { changed() }\n");
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    store.set_repo_root(Some(repo.to_path_buf()));
    let files = ImportOptions {
        git: false,
        gh: false,
        ..ImportOptions::all()
    };
    import_repo(&mut store, repo, &files, &NoGh).unwrap();
    commit_all(repo, "change a");
    let err = store.confirm_memory("DEC-a").unwrap_err().to_string();
    assert!(err.contains("imported from note"), "{err}");
}

#[test]
fn a_local_rebaseline_survives_the_next_import() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut store, id) = stale_setup(&tmp, false);
    store.confirm_memory(&id).unwrap();
    commit_all(tmp.path(), "tidy");
    let files = ImportOptions {
        gh: false,
        ..ImportOptions::all()
    };
    import_repo(&mut store, tmp.path(), &files, &NoGh).unwrap();
    assert!(store.list_stale().unwrap().is_empty());
}

#[test]
fn mcp_submit_accepts_results_sent_as_json_text() {
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    let m = store
        .record_memory(opts(MemoryKind::Learning, "l", "b"))
        .unwrap();
    let mcp = McpServer::new(store);
    let results = format!("[{{\"id\":\"{}\",\"keywords\":[\"alpha\"]}}]", m.id);
    let out = call(
        &mcp,
        "tools/call",
        json!({"name": "submit_work", "arguments": {"task": "expand", "results": results}}),
    );
    assert!(
        out["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Applied 1"),
        "{out}"
    );
}
