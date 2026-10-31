//! Agents in different worktrees of one repository: a shared database, per-worktree
//! ownership of imported rows, and the merge of old per-worktree databases.

use memdag::db::{legacy_worktree_db, merge_legacy_db, open_connection, project_db_path};
use memdag::import::{CommentSource, GhComment, ImportOptions, import_repo};
use memdag::models::{MemoryKind, MemoryStatus, RelationType};
use memdag::store::{MemoryStore, RecordOptions, SearchOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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
fn note(id: &str, title: &str, body: &str) -> String {
    format!(
        "---\nid: {id}\nkind: decision\ntags: []\nsupersedes:\nrefs: []\n---\n# {title}\n\n{body}\n"
    )
}
struct NoGh;
impl CommentSource for NoGh {
    fn fetch_comments(&self, _: &Path) -> anyhow::Result<Vec<GhComment>> {
        Ok(Vec::new())
    }
}
fn files() -> ImportOptions {
    ImportOptions {
        git: false,
        gh: false,
        ..ImportOptions::all()
    }
}
fn opts(kind: MemoryKind, title: &str) -> RecordOptions {
    RecordOptions {
        id: None,
        kind,
        title: title.into(),
        body: "b".into(),
        tags: None,
        supersedes_id: None,
        session_id: None,
        embedding: None,
        paths: Vec::new(),
        commit: None,
    }
}

/// A repository `main` with one linked worktree `wt` on branch `b`. Returns (tmp, main, wt).
fn repo_with_worktree() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "Test"]);
    git(&main, &["config", "commit.gpgsign", "false"]);
    write(&main, ".gitignore", ".memdag/\n");
    write(
        &main,
        "docs/memory/DEC-shared.md",
        &note("DEC-shared", "Retry limit is 3", "three"),
    );
    commit_all(&main, "init");
    git(&main, &["worktree", "add", "-q", "../wt", "-b", "b"]);
    let wt = tmp.path().join("wt");
    (tmp, main, wt)
}

/// A store as a process started in `checkout` would open it: the repository's shared DB.
fn store_in(checkout: &Path) -> MemoryStore {
    let path = project_db_path(checkout).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut store = MemoryStore::new(open_connection(&path).unwrap());
    store.set_repo_root(Some(checkout.to_path_buf()));
    store
}

fn ids(store: &MemoryStore, q: &str) -> Vec<(String, bool)> {
    store
        .search_memory(SearchOptions {
            query: q.into(),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .map(|r| (r.memory.id, r.other_checkout))
        .collect()
}

#[test]
fn all_worktrees_of_a_repository_share_one_database() {
    let (_tmp, main, wt) = repo_with_worktree();
    assert_eq!(project_db_path(&main), project_db_path(&wt));
    assert_eq!(
        project_db_path(&wt).unwrap(),
        main.canonicalize().unwrap().join(".memdag/memdag.db")
    );

    let mut a = store_in(&main);
    let task = a
        .record_memory(opts(MemoryKind::Task, "Port the exporter"))
        .unwrap();
    let b = store_in(&wt);
    let ready = b.list_ready(10).unwrap();
    assert!(
        ready.iter().any(|m| m.id == task.id),
        "B sees A's task: {ready:?}"
    );
}

#[test]
fn a_branch_may_edit_a_shared_note_without_being_refused() {
    let (_tmp, main, wt) = repo_with_worktree();
    let mut a = store_in(&main);
    import_repo(&mut a, &main, &files(), &NoGh).unwrap();

    write(
        &wt,
        "docs/memory/DEC-shared.md",
        &note("DEC-shared", "Retry limit is 5", "five on b"),
    );
    commit_all(&wt, "b edits");
    let mut b = store_in(&wt);
    let r = import_repo(&mut b, &wt, &files(), &NoGh).unwrap();
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    let (m, _) = b.get_memory("DEC-shared").unwrap().unwrap();
    assert_eq!(m.title, "Retry limit is 5");
}

#[test]
fn a_worktree_never_prunes_what_another_worktree_still_has() {
    let (_tmp, main, wt) = repo_with_worktree();
    // A note that exists only on branch b.
    write(
        &wt,
        "docs/memory/DEC-b-only.md",
        &note("DEC-b-only", "Branch b idea", "zebra"),
    );
    commit_all(&wt, "b note");
    let mut b = store_in(&wt);
    import_repo(&mut b, &wt, &files(), &NoGh).unwrap();

    // The main checkout's import keeps it, and labels it as another worktree's.
    let mut a = store_in(&main);
    let r = import_repo(&mut a, &main, &files(), &NoGh).unwrap();
    assert_eq!(r.pruned, 0, "{r:?}");
    assert_eq!(ids(&a, "zebra"), vec![("DEC-b-only".to_string(), true)]);
    assert_eq!(ids(&b, "zebra"), vec![("DEC-b-only".to_string(), false)]);

    // Branch b deletes the shared note: the main checkout still has it, so it stays.
    std::fs::remove_file(wt.join("docs/memory/DEC-shared.md")).unwrap();
    commit_all(&wt, "b drops shared");
    import_repo(&mut b, &wt, &files(), &NoGh).unwrap();
    assert!(a.get_memory("DEC-shared").unwrap().is_some());
    let (shared, _) = b.get_memory("DEC-shared").unwrap().unwrap();
    assert!(b.only_in_other_checkout(&shared));

    // Once no checkout has it, it goes.
    std::fs::remove_file(main.join("docs/memory/DEC-shared.md")).unwrap();
    commit_all(&main, "main drops shared");
    let r = import_repo(&mut a, &main, &files(), &NoGh).unwrap();
    assert_eq!(r.pruned, 1, "{r:?}");
    assert!(a.get_memory("DEC-shared").unwrap().is_none());
}

#[test]
fn another_worktrees_note_is_not_stale_checked_against_this_checkout() {
    let (_tmp, main, wt) = repo_with_worktree();
    write(&wt, "src/new.rs", "fn only_on_b() {}\n");
    commit_all(&wt, "b file");
    let mut b = store_in(&wt);
    b.set_write_through(Some(wt.join("docs/memory")));
    let mut o = opts(MemoryKind::Decision, "new.rs exists on b");
    o.paths = vec!["src/new.rs".into()];
    let m = b.record_memory(o).unwrap();
    assert!(b.stale_paths(&m).is_empty());

    // In the main checkout src/new.rs doesn't exist; that's not a reason to flag it.
    let a = store_in(&main);
    assert!(a.only_in_other_checkout(&m));
    assert!(a.stale_paths(&m).is_empty());
    assert!(a.list_stale().unwrap().is_empty());
}

#[test]
fn removing_a_worktree_drops_its_claims() {
    let (_tmp, main, wt) = repo_with_worktree();
    write(
        &wt,
        "docs/memory/DEC-b-only.md",
        &note("DEC-b-only", "Branch b idea", "zebra"),
    );
    commit_all(&wt, "b note");
    let mut b = store_in(&wt);
    import_repo(&mut b, &wt, &files(), &NoGh).unwrap();
    drop(b);

    git(&main, &["worktree", "remove", "--force", "../wt"]);
    let mut a = store_in(&main);
    let r = import_repo(&mut a, &main, &files(), &NoGh).unwrap();
    assert_eq!(r.pruned, 1, "{r:?}");
    assert!(a.get_memory("DEC-b-only").unwrap().is_none());
    assert!(a.get_memory("DEC-shared").unwrap().is_some());
}

#[test]
fn an_old_per_worktree_database_is_merged_once() {
    let (_tmp, main, wt) = repo_with_worktree();
    // What memdag <= 0.3 did: a database inside the worktree, with a task that depends on
    // an imported decision.
    let old_path = wt.join(".memdag/memdag.db");
    std::fs::create_dir_all(old_path.parent().unwrap()).unwrap();
    let task_id = {
        let mut old = MemoryStore::new(open_connection(&old_path).unwrap());
        import_repo(&mut old, &wt, &files(), &NoGh).unwrap();
        let t = old
            .record_memory(opts(MemoryKind::Task, "Ship from b"))
            .unwrap();
        old.link_entities(&t.id, "DEC-shared", RelationType::DependsOn)
            .unwrap();
        t.id
    };

    let mut shared = store_in(&main);
    import_repo(&mut shared, &main, &files(), &NoGh).unwrap();
    let shared_path = project_db_path(&wt).unwrap();
    let legacy = legacy_worktree_db(&wt, &shared_path).expect("old database detected");
    assert_eq!(merge_legacy_db(shared.connection(), &legacy).unwrap(), 1);

    let (task, rels) = shared.get_memory(&task_id).unwrap().unwrap();
    assert_eq!(task.status, MemoryStatus::Active);
    assert_eq!(rels.len(), 1, "the edge to the imported decision survives");
    assert!(!old_path.exists());
    assert!(wt.join(".memdag/memdag.db.merged").exists());
    assert!(
        legacy_worktree_db(&wt, &shared_path).is_none(),
        "merged once"
    );
}
