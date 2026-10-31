use memdag::db::open_connection;
use memdag::import::{
    CommentSource, GhComment, ImportOptions, import_repo, last_imported_head, refresh_if_stale,
};
use memdag::models::{MemoryKind, MemoryStatus};
use memdag::store::{MemoryStore, RecordOptions};
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

fn commit(repo: &Path, msg: &str) -> String {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "--allow-empty", "-m", msg]);
    git(repo, &["rev-parse", "HEAD"])
}

fn write(repo: &Path, rel: &str, text: &str) {
    let p = repo.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

struct FakeGh(Vec<GhComment>);
impl CommentSource for FakeGh {
    fn fetch_comments(&self, _: &Path) -> anyhow::Result<Vec<GhComment>> {
        Ok(self.0.clone())
    }
}

struct BrokenGh;
impl CommentSource for BrokenGh {
    fn fetch_comments(&self, _: &Path) -> anyhow::Result<Vec<GhComment>> {
        anyhow::bail!("gh not authenticated")
    }
}

fn fake_gh() -> FakeGh {
    FakeGh(vec![
        GhComment {
            id: 9001,
            issue_number: Some(752),
            body: "<!-- handoff -->\n**Handoff** — opencode, 2026-10-30, branch `feat/x`\n- Tried: WAL\n- Next: ship".into(),
            created_at: Some("2026-10-30T10:00:00Z".into()),
            author: Some("hiranp".into()),
            association: Some("OWNER".into()),
            url: Some("https://github.com/o/r/issues/752#issuecomment-9001".into()),
        },
        GhComment {
            id: 9002,
            issue_number: Some(752),
            body: "just a normal comment".into(),
            ..Default::default()
        },
    ])
}

/// A repo exercising every local source.
fn seed_repo(repo: &Path) -> String {
    init_repo(repo);
    write(
        repo,
        "docs/adr/0001-use-sqlite.md",
        "# 0001. Use SQLite\n- Status: superseded by 0002\n- Date: 2026-01-01\n- Refs: #10\n## Context\nneed storage\n## Decision\nsqlite\n## Consequences\nnone\n",
    );
    write(
        repo,
        "docs/adr/0002-use-sqlite-wal.md",
        "# 0002. Use SQLite in WAL mode\n- Status: accepted\n- Date: 2026-02-01\n- Refs: #11, ADR-0001\n## Context\nconcurrency\n## Decision\nWAL\n## Consequences\nwal files\n",
    );
    write(repo, "docs/adr/README.md", "index\n");
    write(
        repo,
        "docs/memory/INV-2026-10-30-no-global-db.md",
        "---\nid: INV-2026-10-30-no-global-db\nkind: invariant\ntags: [db, cache]\nsupersedes:\nrefs: [\"#752\", \"ADR-0002\"]\n---\n# DB is a cache\n\nRebuild from git.\n",
    );
    write(
        repo,
        "docs/memory/LRN-2026-10-30-old.md",
        "---\nid: LRN-2026-10-30-old\nkind: learning\ntags: []\nsupersedes:\nrefs: []\n---\n# Old learning\n",
    );
    write(
        repo,
        "docs/memory/LRN-2026-10-30-new.md",
        "---\nid: LRN-2026-10-30-new\nkind: learning\ntags: []\nsupersedes: LRN-2026-10-30-old\nrefs: []\n---\n# New learning\n\nbetter\n",
    );
    write(repo, "docs/memory/README.md", "# Memory notes\n");
    commit(
        repo,
        "feat(db): adopt wal\n\nReaders must not block writers.\n\nRefs: #719, ADR-0002\nDecision: SQLite runs in WAL mode\nAssisted-by: opencode",
    );
    commit(repo, "chore: no decision here\n\nRefs: #1");
    git(repo, &["rev-parse", "HEAD"])
}

fn store_at(path: &Path) -> MemoryStore {
    MemoryStore::new(open_connection(path).unwrap())
}

/// Content-only snapshot (timestamps excluded) for rebuild comparisons.
fn snapshot(store: &MemoryStore) -> (Vec<String>, Vec<String>) {
    let data = store.export_all().unwrap();
    let mut mems: Vec<String> = data
        .memories
        .iter()
        .map(|m| {
            format!(
                "{}|{}|{}|{}|{:?}|{}",
                m.id, m.kind, m.title, m.body, m.tags, m.status
            )
        })
        .collect();
    mems.sort();
    let mut rels: Vec<String> = data
        .relations
        .iter()
        .map(|r| format!("{}-{}->{}", r.source_id, r.relation_type, r.target_id))
        .collect();
    rels.sort();
    (mems, rels)
}

#[test]
fn import_all_sources_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let head = seed_repo(&repo);
    let mut store = store_at(&tmp.path().join("m.db"));

    let r1 = import_repo(&mut store, &repo, &ImportOptions::all(), &fake_gh()).unwrap();
    assert_eq!(
        (r1.git, r1.adr, r1.notes, r1.gh),
        (1, 2, 3, 1),
        "{:?}",
        r1.warnings
    );
    assert!(r1.warnings.is_empty(), "{:?}", r1.warnings);
    assert_eq!(r1.changed, 7);
    assert_eq!(r1.pending, 0);
    assert_eq!(r1.head.as_deref(), Some(head.as_str()));
    let first = snapshot(&store);

    let r2 = import_repo(&mut store, &repo, &ImportOptions::all(), &fake_gh()).unwrap();
    assert_eq!(r2.changed, 0, "re-import must not rewrite rows");
    assert_eq!(r2.edges, 0);
    assert_eq!(snapshot(&store), first);
    assert_eq!(store.stats().unwrap().total_memories, 7);

    // Git decision commit.
    let id = format!(
        "GIT-{}",
        &git(&repo, &["log", "--format=%H", "--grep=Decision:"])[..7]
    );
    let (m, rels) = store.get_memory(&id).unwrap().unwrap();
    assert_eq!(m.kind, MemoryKind::Decision);
    assert_eq!(m.title, "SQLite runs in WAL mode");
    assert_eq!(
        m.body,
        "feat(db): adopt wal\n\nReaders must not block writers."
    );
    assert_eq!(m.tags.as_deref(), Some("#719 ADR-0002 git"));
    assert!(rels.iter().any(|r| r.target_id == "ADR-0002"));

    // Handoff comment.
    let (h, _) = store.get_memory("GH-9001").unwrap().unwrap();
    assert_eq!(h.kind, MemoryKind::Learning);
    assert!(h.title.starts_with("#752 Handoff — opencode"));
    assert!(h.tags.unwrap().contains("#752"));
    assert!(store.get_memory("GH-9002").unwrap().is_none());

    // Note supersession.
    let (old, _) = store.get_memory("LRN-2026-10-30-old").unwrap().unwrap();
    assert_eq!(old.status, MemoryStatus::Superseded);
}

#[test]
fn adr_supersession_edges_and_status() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    seed_repo(&repo);
    let mut store = store_at(&tmp.path().join("m.db"));
    let opts = ImportOptions {
        git: false,
        notes: false,
        gh: false,
        ..ImportOptions::all()
    };
    import_repo(&mut store, &repo, &opts, &BrokenGh).unwrap();

    let (old, old_rels) = store.get_memory("ADR-0001").unwrap().unwrap();
    assert_eq!(old.status, MemoryStatus::Superseded);
    assert_eq!(old.title, "Use SQLite");
    let (new, _) = store.get_memory("ADR-0002").unwrap().unwrap();
    assert_eq!(new.status, MemoryStatus::Active);
    assert!(old_rels.iter().any(|r| r.target_id == "ADR-0002"
        && r.relation_type == memdag::models::RelationType::Supersedes));
    assert!(store.get_memory("ADR-README").unwrap().is_none());
}

#[test]
fn unit_separator_in_a_commit_message_does_not_shift_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let msg = tmp.path().join("msg.txt");
    std::fs::write(
        &msg,
        "feat: odd\x1fsubject\nwrapped line\n\nbody has \x1f a separator\n\nRefs: ADR-0009\nDecision: Keep the unit separator",
    )
    .unwrap();
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-F", msg.to_str().unwrap()],
    );
    let sha = git(&repo, &["rev-parse", "HEAD"]);
    let mut store = store_at(&tmp.path().join("m.db"));
    let r = import_repo(&mut store, &repo, &ImportOptions::local(), &BrokenGh).unwrap();
    assert_eq!(r.git, 1, "{:?}", r.warnings);

    let (m, _) = store
        .get_memory(&format!("GIT-{}", &sha[..7]))
        .unwrap()
        .unwrap();
    assert_eq!(m.title, "Keep the unit separator");
    assert_eq!(
        m.body,
        "feat: odd\x1fsubject wrapped line\n\nbody has \x1f a separator"
    );
    assert_eq!(m.tags.as_deref(), Some("ADR-0009 git"));
    assert!(m.created_at.starts_with("20"), "{}", m.created_at);
}

#[test]
fn rebuild_from_scratch_reproduces_db_including_write_through() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    seed_repo(&repo);
    let db = tmp.path().join("m.db");

    {
        let mut store = store_at(&db);
        import_repo(&mut store, &repo, &ImportOptions::all(), &fake_gh()).unwrap();
        store.set_write_through(Some(repo.join("docs/memory")));
        let rec = |kind, title: &str, sup: Option<&str>| RecordOptions {
            id: None,
            kind,
            title: title.into(),
            body: format!("body of {title}"),
            tags: Some("a, b".into()),
            supersedes_id: sup.map(str::to_string),
            session_id: Some("s".into()),
            embedding: None,
            paths: Vec::new(),
            commit: None,
        };
        let d = store
            .record_memory(rec(MemoryKind::Decision, "Written through", None))
            .unwrap();
        store
            .record_memory(rec(MemoryKind::Decision, "Replaces it", Some(&d.id)))
            .unwrap();
        store
            .record_memory(rec(MemoryKind::Blocker, "Blocked on CI", Some("ADR-0002")))
            .unwrap();
        commit(&repo, "docs(memory): record decisions");
    }
    let before = snapshot(&store_at(&db));

    for ext in ["", "-wal", "-shm"] {
        std::fs::remove_file(format!("{}{ext}", db.display())).ok();
    }
    let mut fresh = store_at(&db);
    assert_eq!(fresh.stats().unwrap().total_memories, 0);
    import_repo(&mut fresh, &repo, &ImportOptions::all(), &fake_gh()).unwrap();
    assert_eq!(snapshot(&fresh), before);
}

#[test]
fn missing_gh_is_a_warning_not_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    seed_repo(&repo);
    let mut store = store_at(&tmp.path().join("m.db"));
    let r = import_repo(&mut store, &repo, &ImportOptions::all(), &BrokenGh).unwrap();
    assert_eq!(r.gh, 0);
    assert!(r.warnings.iter().any(|w| w.starts_with("gh: skipped")));
    assert_eq!(r.git, 1);
}

#[test]
fn non_git_dir_imports_files_and_warns() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "docs/adr/0003-x.md",
        "# 0003. X\n- Status: accepted\n",
    );
    let mut store = store_at(&tmp.path().join("m.db"));
    let r = import_repo(&mut store, tmp.path(), &ImportOptions::local(), &BrokenGh).unwrap();
    assert_eq!(r.adr, 1);
    assert!(r.head.is_none());
    assert!(r.warnings.iter().any(|w| w.starts_with("git:")));
}

#[test]
fn refresh_runs_only_when_head_moves_and_is_incremental() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let head = seed_repo(&repo);
    let mut store = store_at(&tmp.path().join("m.db"));
    let notes = Path::new("docs/memory");

    let r = refresh_if_stale(&mut store, &repo, notes).unwrap().unwrap();
    assert_eq!(r.git, 1);
    assert_eq!(
        last_imported_head(&store, &repo).unwrap().as_deref(),
        Some(head.as_str())
    );
    assert!(
        refresh_if_stale(&mut store, &repo, notes)
            .unwrap()
            .is_none()
    );

    // A new decision commit that references a note added in the same commit.
    write(
        &repo,
        "docs/memory/DEC-2026-10-31-later.md",
        "---\nid: DEC-2026-10-31-later\nkind: decision\ntags: []\nsupersedes:\nrefs: []\n---\n# Later\n",
    );
    commit(
        &repo,
        "feat: later\n\nRefs: DEC-2026-10-31-later\nDecision: Do it later",
    );
    let r = refresh_if_stale(&mut store, &repo, notes).unwrap().unwrap();
    assert_eq!(r.git, 1, "only the new commit is scanned");
    assert_eq!(r.edges, 1);
    assert_eq!(r.changed, 2);
    assert!(
        refresh_if_stale(&mut store, &repo, notes)
            .unwrap()
            .is_none()
    );
}

#[test]
fn import_cli_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    seed_repo(&repo);
    let db = tmp.path().join("cli.db");
    let bin = env!("CARGO_BIN_EXE_memdag");

    let run = |args: &[&str]| {
        let out = Command::new(bin)
            .args(args)
            .env("MEMDAG_DB", &db)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let out = run(&["import", "--git", "--adr", "--notes"]);
    assert!(out.contains("git: 1"), "{out}");
    run(&["import", "--git", "--adr", "--notes"]);
    let stats = run(&["stats"]);
    assert!(stats.contains("Total Memories:       6"), "{stats}");
    let found = run(&["search", "WAL"]);
    assert!(found.contains("ADR-0002"), "{found}");
}

#[test]
fn cli_write_through_via_memdag_toml_and_search_auto_refresh() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    seed_repo(&repo);
    write(
        &repo,
        "memdag.toml",
        "[write_through]\ndir = \"docs/memory\"\n",
    );
    let db = tmp.path().join("cli.db");
    let bin = env!("CARGO_BIN_EXE_memdag");
    let run = |args: &[&str]| {
        let out = Command::new(bin)
            .args(args)
            .env("MEMDAG_DB", &db)
            .env_remove("MEMDAG_NO_AUTO_IMPORT")
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // Search on a never-imported DB auto-imports the repo first.
    let found = run(&["search", "cache"]);
    assert!(found.contains("INV-2026-10-30-no-global-db"), "{found}");

    run(&[
        "record",
        "--kind",
        "decision",
        "--title",
        "Through the CLI",
        "--body",
        "b",
        "--id",
        "DEC-cli",
    ]);
    assert!(repo.join("docs/memory/DEC-cli.md").exists());
    run(&[
        "record",
        "--kind",
        "ephemeral",
        "--title",
        "scratch",
        "--body",
        "b",
    ]);
    let notes = std::fs::read_dir(repo.join("docs/memory")).unwrap().count();
    assert_eq!(notes, 5, "README + 3 seeded + DEC-cli");
}
