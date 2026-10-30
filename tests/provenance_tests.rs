//! Provenance/staleness, token-budgeted output, and hybrid (BM25 + vector) search.

use memdag::db::{open_connection, open_in_memory};
use memdag::import::{GhCli, ImportOptions, import_repo};
use memdag::models::{MemoryKind, SessionLearning};
use memdag::store::{
    MemoryStore, RecordOptions, SearchOptions, format_search_results_for_llm,
    format_search_results_with_budget, rrf_scores,
};
use std::path::Path;
use std::process::Command;

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git on PATH");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
}

fn commit_all(repo: &Path, msg: &str) -> String {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "--allow-empty", "-m", msg]);
    git(repo, &["rev-parse", "HEAD"])
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

fn search(store: &MemoryStore, q: &str) -> Vec<memdag::MemorySearchResult> {
    store
        .search_memory(SearchOptions {
            query: q.into(),
            ..Default::default()
        })
        .unwrap()
}

/// A repo with src/a.rs and src/b.rs committed, plus a store pointed at it.
fn repo_store(tmp: &tempfile::TempDir) -> MemoryStore {
    init_repo(tmp.path());
    write(tmp.path(), "src/a.rs", "fn a() {}\n");
    write(tmp.path(), "src/b.rs", "fn b() {}\n");
    commit_all(tmp.path(), "init");
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    store.set_repo_root(Some(tmp.path().to_path_buf()));
    store
}

// ---------------------------------------------------------------- provenance

#[test]
fn memory_with_paths_is_stamped_with_head_and_goes_stale_when_a_path_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = repo_store(&tmp);
    let head = git(tmp.path(), &["rev-parse", "HEAD"]);

    let mut o = opts(MemoryKind::Decision, "Parser uses a pratt loop", "see a.rs");
    o.paths = vec!["src/a.rs".into()];
    let mem = store.record_memory(o).unwrap();
    assert_eq!(mem.commit.as_deref(), Some(head.as_str()));
    assert_eq!(mem.paths, vec!["src/a.rs"]);

    // Fresh right after recording.
    assert!(store.stale_paths(&mem).is_empty());
    assert!(store.list_stale().unwrap().is_empty());

    // Unrelated change: still fresh.
    write(tmp.path(), "src/b.rs", "fn b() { /* changed */ }\n");
    commit_all(tmp.path(), "touch b");
    assert!(store.stale_paths(&mem).is_empty());

    // Cited file changes (committed): stale, and search says so.
    write(tmp.path(), "src/a.rs", "fn a() { /* rewritten */ }\n");
    commit_all(tmp.path(), "rewrite a");
    assert_eq!(store.stale_paths(&mem), vec!["src/a.rs"]);

    let hits = search(&store, "pratt");
    assert_eq!(hits[0].stale_paths, vec!["src/a.rs"]);
    let text = format_search_results_for_llm(&hits);
    assert!(text.contains("**STALE**"), "{text}");
    assert!(text.contains("**Cites**: src/a.rs"), "{text}");

    let listed = store.list_stale().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0.id, mem.id);
}

#[test]
fn uncommitted_edits_and_deletions_count_as_stale() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = repo_store(&tmp);
    let mut o = opts(MemoryKind::Invariant, "a is pure", "no io");
    o.paths = vec!["src".into()]; // a directory cites everything under it
    let mem = store.record_memory(o).unwrap();

    // The cited path is the directory, so a deletion inside it reports the directory.
    std::fs::remove_file(tmp.path().join("src/b.rs")).unwrap();
    assert_eq!(store.stale_paths(&mem), vec!["src"]);
}

#[test]
fn memories_without_paths_or_with_unknown_commit_are_never_stale() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = repo_store(&tmp);
    let plain = store
        .record_memory(opts(MemoryKind::Decision, "no paths", "x"))
        .unwrap();
    assert!(plain.commit.is_none(), "no git call without paths");

    let mut o = opts(MemoryKind::Decision, "bogus commit", "x");
    o.paths = vec!["src/a.rs".into()];
    o.commit = Some("deadbeefdeadbeef".into()); // valid hex, not in the repo
    let mem = store.record_memory(o).unwrap();
    write(tmp.path(), "src/a.rs", "changed\n");
    assert!(
        store.stale_paths(&mem).is_empty(),
        "unknown commit = no signal"
    );
    assert!(store.stale_paths(&plain).is_empty());
}

#[test]
fn hostile_paths_and_commits_are_rejected() {
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    for bad in ["../secret", "/etc/passwd", "--output=/tmp/x", "a/../../b"] {
        let mut o = opts(MemoryKind::Decision, "t", "b");
        o.paths = vec![bad.into()];
        assert!(store.record_memory(o).is_err(), "{bad} should be rejected");
    }
    let mut o = opts(MemoryKind::Decision, "t", "b");
    o.paths = vec!["src/a.rs".into()];
    o.commit = Some("--all".into());
    assert!(store.record_memory(o).is_err());
}

#[test]
fn pathspec_magic_in_a_note_is_treated_literally() {
    // A cited ":(exclude)src/a.rs" must not behave as pathspec magic when diffing.
    let tmp = tempfile::tempdir().unwrap();
    let mut store = repo_store(&tmp);
    let mut o = opts(MemoryKind::Decision, "magic", "x");
    o.paths = vec![":(exclude)src/a.rs".into()];
    let mem = store.record_memory(o).unwrap();
    write(tmp.path(), "src/a.rs", "changed\n");
    assert!(store.stale_paths(&mem).is_empty());
}

#[test]
fn consolidated_learning_carries_provenance() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = repo_store(&tmp);
    let res = store
        .consolidate_session(
            "s1",
            vec![SessionLearning {
                title: "b is hot".into(),
                body: "keep it allocation free".into(),
                tags: None,
                kind: MemoryKind::Learning,
                paths: vec!["src/b.rs".into()],
                commit: None,
            }],
            true,
        )
        .unwrap();
    let (mem, _) = store.get_memory(&res.recorded_ids[0]).unwrap().unwrap();
    assert_eq!(mem.paths, vec!["src/b.rs"]);
    assert!(mem.commit.is_some());
}

#[test]
fn write_through_note_round_trips_provenance_through_the_importer() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = repo_store(&tmp);
    store.set_write_through(Some(tmp.path().join("docs/memory")));

    let mut o = opts(MemoryKind::Decision, "Use arena for ast", "faster");
    o.paths = vec!["src/a.rs".into()];
    let mem = store.record_memory(o).unwrap();

    let note =
        std::fs::read_to_string(tmp.path().join(format!("docs/memory/{}.md", mem.id))).unwrap();
    assert!(note.contains("paths: [\"src/a.rs\"]"), "{note}");
    assert!(note.contains(&format!("commit: {}", mem.commit.as_deref().unwrap())));

    // Rebuild a brand-new index purely from git-tracked text.
    commit_all(tmp.path(), "add note");
    let mut fresh = MemoryStore::new(open_in_memory().unwrap());
    fresh.set_repo_root(Some(tmp.path().to_path_buf()));
    import_repo(
        &mut fresh,
        tmp.path(),
        &ImportOptions {
            git: false,
            adr: false,
            notes: true,
            gh: false,
            ..ImportOptions::all()
        },
        &GhCli,
    )
    .unwrap();
    let (rebuilt, _) = fresh.get_memory(&mem.id).unwrap().unwrap();
    assert_eq!(rebuilt.paths, mem.paths);
    assert_eq!(rebuilt.commit, mem.commit);
}

#[test]
fn imported_note_with_hostile_provenance_is_sanitised() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo(tmp.path());
    write(
        tmp.path(),
        "docs/memory/DEC-evil.md",
        "---\nid: DEC-evil\nkind: decision\ntags: []\nsupersedes:\nrefs: []\npaths: [\"../../etc/passwd\", \"src/ok.rs\"]\ncommit: --upload-pack=x\n---\n# Evil\n\nbody\n",
    );
    commit_all(tmp.path(), "evil note");
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    import_repo(
        &mut store,
        tmp.path(),
        &ImportOptions {
            git: false,
            adr: false,
            notes: true,
            gh: false,
            ..ImportOptions::all()
        },
        &GhCli,
    )
    .unwrap();
    let (mem, _) = store.get_memory("DEC-evil").unwrap().unwrap();
    assert_eq!(mem.paths, vec!["src/ok.rs"]);
    assert_eq!(mem.commit, None);
}

#[test]
fn v1_database_migrates_to_v2_and_keeps_data() {
    use rusqlite::Connection;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v1.db");
    {
        // Create a current DB, then roll it back to v1's shape.
        let conn = open_connection(&path).unwrap();
        conn.execute(
            "INSERT INTO memories (id, kind, title, body) VALUES ('DEC-1','decision','t','b')",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "ALTER TABLE memories DROP COLUMN paths; ALTER TABLE memories DROP COLUMN commit_sha; PRAGMA user_version = 1;",
        )
        .unwrap();
    }
    let _ = Connection::open(&path).unwrap(); // sanity: file is a valid db
    let conn = open_connection(&path).unwrap();
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, memdag::db::SCHEMA_VERSION);
    conn.execute(
        "UPDATE memories SET paths = 'a', commit_sha = 'abc1234' WHERE id = 'DEC-1'",
        [],
    )
    .unwrap();
    let store = MemoryStore::new(conn);
    let (m, _) = store.get_memory("DEC-1").unwrap().unwrap();
    assert_eq!(m.paths, vec!["a"]);
}

// ------------------------------------------------------------- token budget

fn seeded_store(n: usize, body_chars: usize) -> MemoryStore {
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    for i in 0..n {
        let mut o = opts(
            MemoryKind::Decision,
            &format!("budget topic {i}"),
            &"word ".repeat(body_chars / 5),
        );
        o.tags = Some("budgetword".into());
        store.record_memory(o).unwrap();
    }
    store
}

#[test]
fn budget_packs_in_rank_order_and_reports_omissions() {
    let store = seeded_store(10, 2000); // ~500 tokens each
    let hits = store
        .search_memory(SearchOptions {
            query: "budgetword".into(),
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 10);

    let unlimited = format_search_results_with_budget(&hits, None);
    let small = format_search_results_with_budget(&hits, Some(1500));
    assert!(unlimited.matches("### ").count() == 10);
    let shown = small.matches("### ").count();
    assert!((1..10).contains(&shown), "shown={shown}\n{small}");
    assert!(small.contains("lower-ranked result(s) omitted"));
    assert!(small.contains(&format!("showing top {shown}")));
    assert!(
        memdag::store::estimate_tokens(&small) <= 1500,
        "output {} tokens",
        memdag::store::estimate_tokens(&small)
    );
}

#[test]
fn budget_never_returns_empty_and_truncates_an_oversized_top_hit() {
    let store = seeded_store(1, 40_000); // ~10k tokens
    let hits = search(&store, "budgetword");
    let out = format_search_results_with_budget(&hits, Some(300));
    assert_eq!(out.matches("### ").count(), 1);
    assert!(out.contains("truncated; call get_memory"), "{out}");
    assert!(out.len() < 3000, "len={}", out.len());
}

#[test]
fn zero_budget_means_unlimited_and_default_matches_unbounded_when_small() {
    let store = seeded_store(3, 100);
    let hits = search(&store, "budgetword");
    assert_eq!(
        format_search_results_with_budget(&hits, Some(2000)),
        format_search_results_for_llm(&hits)
    );
}

#[test]
fn relations_are_capped_per_result() {
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    let hub = store
        .record_memory(opts(MemoryKind::Decision, "hub node uniquehub", "x"))
        .unwrap();
    for i in 0..9 {
        let m = store
            .record_memory(opts(MemoryKind::Task, &format!("spoke {i}"), "x"))
            .unwrap();
        store
            .link_entities(&m.id, &hub.id, memdag::models::RelationType::DependsOn)
            .unwrap();
    }
    let out = format_search_results_for_llm(&search(&store, "uniquehub"));
    assert!(out.contains("(+3 more; call get_memory for all)"), "{out}");
}

// ------------------------------------------------------------- RRF + hybrid

#[test]
fn rrf_rewards_agreement_between_channels() {
    // "a" is 2nd in both channels; "x" and "y" are each 1st in only one.
    let fused = rrf_scores(&[
        vec!["x".into(), "a".into(), "b".into()],
        vec!["y".into(), "a".into(), "c".into()],
    ]);
    let ids: Vec<&str> = fused.iter().map(|(i, _)| i.as_str()).collect();
    assert_eq!(
        ids[0], "a",
        "agreement outranks a single first place: {ids:?}"
    );
    assert!((fused[0].1 - 2.0 / 62.0).abs() < 1e-12);
    // Ties keep first-appearance order (stable sort): x before y, b before c.
    assert_eq!(ids[1..], ["x", "y", "b", "c"]);
    // Scores are strictly rank-based: 1/(60+rank).
    assert!((fused[1].1 - 1.0 / 61.0).abs() < 1e-12);
}

/// Embedder for tests: a one-hot 384-vector whose hot index depends on keywords, so
/// "feline" and "cat" land together while "dog" lands elsewhere.
#[cfg(unix)]
const TEST_EMBEDDER: &str = r#"awk '{t = t " " $0} END {
  k = 2;
  if (t ~ /cat|feline|kitten/) k = 0;
  else if (t ~ /dog|canine|puppy/) k = 1;
  printf "[";
  for (i = 0; i < 384; i++) printf "%s%d", (i ? "," : ""), (i == k);
  printf "]"
}'"#;

#[cfg(unix)]
#[test]
fn hybrid_search_finds_a_memory_with_no_shared_keywords() {
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    store.set_embed_command(Some(TEST_EMBEDDER.into()));
    assert!(store.has_embedder());

    // No word overlaps the query "cat" except through the embedder.
    store
        .record_memory(opts(
            MemoryKind::Decision,
            "Feline care",
            "kitten vaccination schedule",
        ))
        .unwrap();
    store
        .record_memory(opts(
            MemoryKind::Decision,
            "Canine care",
            "puppy training schedule",
        ))
        .unwrap();

    // BM25 alone cannot see it...
    let mut keyword_only = MemoryStore::new(open_in_memory().unwrap());
    keyword_only
        .record_memory(opts(
            MemoryKind::Decision,
            "Feline care",
            "kitten vaccination schedule",
        ))
        .unwrap();
    assert!(search(&keyword_only, "cat").is_empty());

    // ...but the hybrid pipeline does, and ranks it first.
    let hits = search(&store, "cat");
    assert!(!hits.is_empty());
    assert_eq!(hits[0].memory.title, "Feline care");
    assert!(hits[0].rank > 0.0, "RRF score expected");
}

#[cfg(unix)]
#[test]
fn embed_missing_backfills_and_a_dead_embedder_degrades_to_keywords() {
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    store
        .record_memory(opts(MemoryKind::Decision, "Feline care", "kitten shots"))
        .unwrap();
    assert!(store.embed_missing().is_err(), "needs an embedder");

    store.set_embed_command(Some(TEST_EMBEDDER.into()));
    assert_eq!(store.embed_missing().unwrap(), 1);
    assert_eq!(store.embed_missing().unwrap(), 0, "idempotent");

    // A failing embedder must not break recording or searching.
    store.set_embed_command(Some("exit 1".into()));
    store
        .record_memory(opts(
            MemoryKind::Decision,
            "Still works",
            "keyword findable zebra",
        ))
        .unwrap();
    let hits = search(&store, "zebra");
    assert_eq!(hits.len(), 1);
}

#[test]
fn vector_tool_is_advertised_only_with_an_embedder() {
    use memdag::mcp::JsonRpcRequest;
    let list = |store: MemoryStore| -> Vec<String> {
        let server = memdag::McpServer::new(store);
        let req: JsonRpcRequest = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list"
        }))
        .unwrap();
        let resp = server.handle_request(req).unwrap();
        resp.result.unwrap()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    };
    let without = list(MemoryStore::new(open_in_memory().unwrap()));
    assert!(!without.contains(&"search_vector".to_string()));
    assert!(without.contains(&"search_memory".to_string()));

    let mut s = MemoryStore::new(open_in_memory().unwrap());
    s.set_embed_command(Some("true".into()));
    assert!(list(s).contains(&"search_vector".to_string()));
}
