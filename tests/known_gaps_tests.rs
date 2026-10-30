//! Regression tests for the gaps listed after the first review: pruning, resolutions that
//! survive a rebuild, refresh inside a running server, embedder identity, atomic note files.

use memdag::McpServer;
use memdag::db::{open_connection, open_in_memory};
use memdag::import::{CommentSource, GhComment, ImportOptions, import_repo, refresh_if_stale};
use memdag::models::{MemoryKind, MemoryStatus};
use memdag::notes::{is_valid_note_id, write_new_file_atomic};
use memdag::store::{MemoryStore, RecordOptions};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
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
fn commit_all(repo: &Path, msg: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "--allow-empty", "-m", msg]);
}
fn write(repo: &Path, rel: &str, text: &str) {
    let p = repo.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}
fn note(id: &str, kind: &str, title: &str, supersedes: &str) -> String {
    format!(
        "---\nid: {id}\nkind: {kind}\ntags: []\nsupersedes: {supersedes}\nrefs: []\n---\n# {title}\n\nbody of {title}\n"
    )
}
fn adr(num: &str, title: &str) -> String {
    format!("# {num}. {title}\n- Status: accepted\n- Date: 2026-10-01\n\nbody\n")
}
struct NoGh;
impl CommentSource for NoGh {
    fn fetch_comments(&self, _: &Path) -> anyhow::Result<Vec<GhComment>> {
        Ok(Vec::new())
    }
}
fn files_only() -> ImportOptions {
    ImportOptions {
        git: false,
        gh: false,
        ..ImportOptions::all()
    }
}
fn exists(store: &MemoryStore, id: &str) -> bool {
    store.get_memory(id).unwrap().is_some()
}
fn status(store: &MemoryStore, id: &str) -> MemoryStatus {
    store.get_memory(id).unwrap().unwrap().0.status
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

// ------------------------------------------------------------------ pruning

#[test]
fn deleted_notes_and_adrs_are_pruned() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    write(
        repo,
        "docs/memory/DEC-a.md",
        &note("DEC-a", "decision", "A", ""),
    );
    write(
        repo,
        "docs/memory/DEC-b.md",
        &note("DEC-b", "decision", "B", ""),
    );
    write(repo, "docs/adr/0001-one.md", &adr("0001", "One"));
    write(repo, "docs/adr/0002-two.md", &adr("0002", "Two"));
    commit_all(repo, "c");
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    import_repo(&mut store, repo, &files_only(), &NoGh).unwrap();
    assert!(exists(&store, "DEC-a") && exists(&store, "ADR-0002"));

    std::fs::remove_file(repo.join("docs/memory/DEC-a.md")).unwrap();
    std::fs::remove_file(repo.join("docs/adr/0002-two.md")).unwrap();
    let r = import_repo(&mut store, repo, &files_only(), &NoGh).unwrap();
    assert_eq!(r.pruned, 2, "{r:?}");
    assert!(!exists(&store, "DEC-a") && !exists(&store, "ADR-0002"));
    assert!(exists(&store, "DEC-b") && exists(&store, "ADR-0001"));

    // A second run is a no-op.
    let r = import_repo(&mut store, repo, &files_only(), &NoGh).unwrap();
    assert_eq!((r.pruned, r.changed), (0, 0));
}

#[test]
fn amended_commit_is_pruned_on_the_next_refresh() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_all(repo, "init");
    commit_all(repo, "Pick a queue\n\nDecision: use a task queue");
    let old = format!("GIT-{}", &git(repo, &["rev-parse", "HEAD"])[..7]);
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    refresh_if_stale(&mut store, repo, Path::new("docs/memory")).unwrap();
    assert!(exists(&store, &old));

    git(
        repo,
        &[
            "commit",
            "-q",
            "--amend",
            "--allow-empty",
            "-m",
            "Pick a queue\n\nDecision: use a durable task queue",
        ],
    );
    let new = format!("GIT-{}", &git(repo, &["rev-parse", "HEAD"])[..7]);
    // HEAD is no longer a descendant of the recorded one, so this is a full rescan.
    let r = refresh_if_stale(&mut store, repo, Path::new("docs/memory"))
        .unwrap()
        .unwrap();
    assert_eq!(r.pruned, 1, "{r:?}");
    assert!(!exists(&store, &old));
    assert!(exists(&store, &new));
}

#[test]
fn incremental_git_import_never_prunes() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_all(repo, "x\n\nDecision: first");
    let first = format!("GIT-{}", &git(repo, &["rev-parse", "HEAD"])[..7]);
    commit_all(repo, "y\n\nDecision: second");
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    let git_only = ImportOptions {
        adr: false,
        notes: false,
        gh: false,
        ..ImportOptions::all()
    };
    import_repo(&mut store, repo, &git_only, &NoGh).unwrap();
    let since = ImportOptions {
        since: Some("HEAD~1".into()),
        record_head: false,
        ..git_only
    };
    let r = import_repo(&mut store, repo, &since, &NoGh).unwrap();
    assert_eq!(r.pruned, 0);
    assert!(exists(&store, &first));
}

#[test]
fn a_note_that_fails_to_parse_keeps_its_row() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    write(
        repo,
        "docs/memory/DEC-a.md",
        &note("DEC-a", "decision", "A", ""),
    );
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    import_repo(&mut store, repo, &files_only(), &NoGh).unwrap();

    // A half-edited file (no `kind:`) is a warning, not a deletion.
    write(repo, "docs/memory/DEC-a.md", "---\nid: DEC-a\n---\n# A\n");
    let r = import_repo(&mut store, repo, &files_only(), &NoGh).unwrap();
    assert_eq!(r.pruned, 0);
    assert!(exists(&store, "DEC-a"));
    assert!(
        r.warnings.iter().any(|w| w.contains("DEC-a")),
        "{:?}",
        r.warnings
    );
}

#[test]
fn repos_sharing_a_database_do_not_prune_or_overwrite_each_other() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    for (dir, own) in [(a.path(), "DEC-from-a"), (b.path(), "DEC-from-b")] {
        init_repo(dir);
        write(
            dir,
            &format!("docs/memory/{own}.md"),
            &note(own, "decision", own, ""),
        );
        write(
            dir,
            "docs/memory/DEC-same.md",
            &note("DEC-same", "decision", own, ""),
        );
    }
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    import_repo(&mut store, a.path(), &files_only(), &NoGh).unwrap();
    let r = import_repo(&mut store, b.path(), &files_only(), &NoGh).unwrap();

    assert_eq!(r.pruned, 0);
    assert!(exists(&store, "DEC-from-a") && exists(&store, "DEC-from-b"));
    let (same, _) = store.get_memory("DEC-same").unwrap().unwrap();
    assert_eq!(same.title, "DEC-from-a", "the first repo keeps its row");
    assert!(
        r.warnings.iter().any(|w| w.contains("another repository")),
        "{:?}",
        r.warnings
    );
}

#[test]
fn removing_a_supersedes_line_or_the_superseding_note_restores_the_target() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    write(
        repo,
        "docs/memory/DEC-old.md",
        &note("DEC-old", "decision", "Old", ""),
    );
    write(
        repo,
        "docs/memory/DEC-new.md",
        &note("DEC-new", "decision", "New", "DEC-old"),
    );
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    import_repo(&mut store, repo, &files_only(), &NoGh).unwrap();
    assert_eq!(status(&store, "DEC-old"), MemoryStatus::Superseded);

    write(
        repo,
        "docs/memory/DEC-new.md",
        &note("DEC-new", "decision", "New", ""),
    );
    import_repo(&mut store, repo, &files_only(), &NoGh).unwrap();
    assert_eq!(status(&store, "DEC-old"), MemoryStatus::Active);
    let (_, rels) = store.get_memory("DEC-old").unwrap().unwrap();
    assert!(rels.is_empty(), "{rels:?}");

    write(
        repo,
        "docs/memory/DEC-new.md",
        &note("DEC-new", "decision", "New", "DEC-old"),
    );
    import_repo(&mut store, repo, &files_only(), &NoGh).unwrap();
    assert_eq!(status(&store, "DEC-old"), MemoryStatus::Superseded);
    std::fs::remove_file(repo.join("docs/memory/DEC-new.md")).unwrap();
    import_repo(&mut store, repo, &files_only(), &NoGh).unwrap();
    assert_eq!(status(&store, "DEC-old"), MemoryStatus::Active);
}

// --------------------------------------------------------------- resolutions

#[test]
fn a_resolution_survives_a_rebuild_from_git_without_churn() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_all(repo, "init");
    let dir = repo.join("docs/memory");
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    store.set_repo_root(Some(repo.to_path_buf()));
    store.set_write_through(Some(dir.clone()));

    let b = store
        .record_memory(opts(
            MemoryKind::Blocker,
            "CI flakes on macOS",
            "keychain prompt",
        ))
        .unwrap();
    let resolved = store
        .resolve_memory(&b.id, Some("  pinned the runner image  "))
        .unwrap();
    assert_eq!(resolved.status, MemoryStatus::Resolved);
    assert!(
        resolved
            .body
            .ends_with("\n\n[Resolved]: pinned the runner image")
    );
    let sidecar = dir.join(format!("{}.resolved.md", b.id));
    assert!(sidecar.is_file());
    assert!(
        store.resolve_memory(&b.id, Some("again")).is_err(),
        "append-only"
    );

    // The note itself is untouched; re-importing into the same index changes nothing.
    let r = import_repo(&mut store, repo, &files_only(), &NoGh).unwrap();
    assert_eq!((r.changed, r.pruned), (0, 0), "{r:?}");

    // A fresh database rebuilt from the files reproduces the resolved row exactly.
    commit_all(repo, "resolve");
    let mut fresh = MemoryStore::new(open_in_memory().unwrap());
    import_repo(&mut fresh, repo, &files_only(), &NoGh).unwrap();
    let (rebuilt, _) = fresh.get_memory(&b.id).unwrap().unwrap();
    assert_eq!(rebuilt.status, MemoryStatus::Resolved);
    assert_eq!(rebuilt.body, resolved.body);
    let r = import_repo(&mut fresh, repo, &files_only(), &NoGh).unwrap();
    assert_eq!(r.changed, 0, "{r:?}");
}

#[test]
fn resolving_without_a_note_and_tasks_stay_database_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("docs/memory");
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    store.set_write_through(Some(dir.clone()));
    let b = store
        .record_memory(opts(MemoryKind::Blocker, "b", "x"))
        .unwrap();
    let r = store.resolve_memory(&b.id, Some("   ")).unwrap();
    assert_eq!(r.body, "x", "a blank note adds nothing");
    let text = std::fs::read_to_string(dir.join(format!("{}.resolved.md", b.id))).unwrap();
    assert_eq!(text, format!("---\nresolves: {}\n---\n", b.id));

    // Tasks aren't note-backed, so nothing is written for them.
    let t = store
        .record_memory(opts(MemoryKind::Task, "t", "y"))
        .unwrap();
    store.resolve_memory(&t.id, Some("done")).unwrap();
    assert!(!dir.join(format!("{}.resolved.md", t.id)).exists());
}

#[test]
fn resolved_suffix_is_reserved() {
    assert!(is_valid_note_id("DEC-x"));
    assert!(!is_valid_note_id("DEC-x.resolved"));
    assert!(!is_valid_note_id("DEC-x.RESOLVED"));
}

// ----------------------------------------------------- refresh while serving

#[test]
fn running_server_sees_a_note_committed_after_it_started() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    init_repo(repo);
    commit_all(repo, "init");
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    store.set_repo_root(Some(repo.to_path_buf()));
    let mcp = McpServer::new(store).with_auto_refresh(
        repo.to_path_buf(),
        "docs/memory".into(),
        Duration::ZERO,
    );
    let search = |id: i64| {
        let req = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "search_memory", "arguments": {"query": "zebra"}}
        });
        let resp = mcp
            .handle_request(serde_json::from_value(req).unwrap())
            .unwrap();
        resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(!search(1).contains("DEC-zebra"));

    write(
        repo,
        "docs/memory/DEC-zebra.md",
        &note("DEC-zebra", "decision", "Zebra stripes", ""),
    );
    commit_all(repo, "add zebra note");
    assert!(search(2).contains("DEC-zebra"));

    std::fs::remove_file(repo.join("docs/memory/DEC-zebra.md")).unwrap();
    commit_all(repo, "drop zebra note");
    assert!(!search(3).contains("DEC-zebra"));
}

// --------------------------------------------------------- embedder identity

#[cfg(unix)]
fn one_hot(i: usize) -> String {
    format!(
        r#"awk 'BEGIN{{printf "["; for(i=0;i<384;i++) printf "%s%d",(i?",":""),(i=={i}); printf "]"}}'"#
    )
}

#[cfg(unix)]
#[test]
fn switching_embedders_drops_vectors_from_the_old_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.db");
    let vectors = |s: &MemoryStore| -> i64 {
        s.connection()
            .query_row("SELECT count(*) FROM memories_vec", [], |r| r.get(0))
            .unwrap()
    };

    let mut first = MemoryStore::new(open_connection(&path).unwrap());
    first.set_embed_command(Some(one_hot(0)));
    first
        .record_memory(opts(MemoryKind::Decision, "d", "b"))
        .unwrap();
    assert_eq!(vectors(&first), 1);
    drop(first);

    // Same embedder: nothing to redo.
    let mut same = MemoryStore::new(open_connection(&path).unwrap());
    same.set_embed_command(Some(one_hot(0)));
    assert_eq!(same.embed_missing().unwrap(), 0);
    drop(same);

    // Different embedder: the old space is discarded and recomputed.
    let mut other = MemoryStore::new(open_connection(&path).unwrap());
    other.set_embed_command(Some(one_hot(1)));
    assert_eq!(other.embed_missing().unwrap(), 1);
    assert_eq!(vectors(&other), 1);
}

// -------------------------------------------------------- atomic note files

#[test]
fn note_files_are_created_atomically_and_never_clobbered() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("docs/memory");
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    store.set_write_through(Some(dir.clone()));
    let m = store
        .record_memory(opts(MemoryKind::Decision, "Atomic", "body"))
        .unwrap();
    store.resolve_memory(&m.id, Some("ok")).unwrap();

    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "no temp files left behind: {names:?}");
    assert!(
        names
            .iter()
            .all(|n| n.ends_with(".md") && !n.starts_with('.'))
    );

    let path = dir.join(format!("{}.md", m.id));
    let before = std::fs::read_to_string(&path).unwrap();
    assert!(write_new_file_atomic(&path, b"overwrite").is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
}
