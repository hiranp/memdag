use memdag::db::open_in_memory;
use memdag::models::{MemoryKind, MemoryStatus, SessionLearning};
use memdag::notes::parse_note;
use memdag::store::{MemoryStore, RecordOptions};

fn opts(kind: MemoryKind, title: &str, body: &str) -> RecordOptions {
    RecordOptions {
        id: None,
        kind,
        title: title.into(),
        body: body.into(),
        tags: Some("sqlite, wal  sync".into()),
        supersedes_id: None,
        session_id: None,
        embedding: None,
        paths: Vec::new(),
        commit: None,
    }
}

fn md_files(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn write_through_writes_contract_note_then_indexes_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("docs/memory");
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    store.set_write_through(Some(dir.clone()));

    let mem = store
        .record_memory(opts(
            MemoryKind::Decision,
            "Use WAL mode",
            "  Readers never block.  ",
        ))
        .unwrap();
    assert!(
        mem.id.starts_with("DEC-") && mem.id.ends_with("-use-wal-mode"),
        "{}",
        mem.id
    );

    let path = dir.join(format!("{}.md", mem.id));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with(&format!("---\nid: {}\nkind: decision\n", mem.id)));
    assert!(text.contains(&format!(
        "tags: [sqlite, wal, sync]\nsupersedes: \nrefs: []\nrecorded: {}\n---\n# Use WAL mode\n",
        mem.created_at
    )));
    assert!(!text.contains("session:"), "no session id was given");

    // DB row equals what the file parses to.
    let note = parse_note(&text, "x").unwrap();
    assert_eq!(mem.title, note.title);
    assert_eq!(note.recorded.as_deref(), Some(mem.created_at.as_str()));
    assert_eq!(mem.body, "Readers never block.");
    assert_eq!(mem.tags.as_deref(), Some("sqlite wal sync"));
}

#[test]
fn ephemeral_and_tasks_are_never_written_and_off_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("notes");

    let mut plain = MemoryStore::new(open_in_memory().unwrap());
    plain
        .record_memory(opts(MemoryKind::Decision, "no wt", "b"))
        .unwrap();
    assert!(!dir.exists());

    let mut store = MemoryStore::new(open_in_memory().unwrap());
    store.set_write_through(Some(dir.clone()));
    store
        .record_memory(opts(MemoryKind::Ephemeral, "scratch", "b"))
        .unwrap();
    store
        .record_memory(opts(MemoryKind::Task, "todo", "b"))
        .unwrap();
    assert!(md_files(&dir).is_empty());

    for kind in [
        MemoryKind::Invariant,
        MemoryKind::Learning,
        MemoryKind::Blocker,
    ] {
        store.record_memory(opts(kind, "durable", "b")).unwrap();
    }
    assert_eq!(md_files(&dir).len(), 3);
}

#[test]
fn supersession_is_append_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("m");
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    store.set_write_through(Some(dir.clone()));

    let mut o = opts(MemoryKind::Decision, "Old way", "x");
    o.id = Some("DEC-old".into());
    store.record_memory(o).unwrap();
    let old_text = std::fs::read_to_string(dir.join("DEC-old.md")).unwrap();

    let mut n = opts(MemoryKind::Decision, "New way", "y");
    n.id = Some("DEC-new".into());
    n.supersedes_id = Some("DEC-old".into());
    store.record_memory(n).unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.join("DEC-old.md")).unwrap(),
        old_text,
        "old note must not be rewritten"
    );
    let new_text = std::fs::read_to_string(dir.join("DEC-new.md")).unwrap();
    assert!(new_text.contains("supersedes: DEC-old\n"));
    let (old, _) = store.get_memory("DEC-old").unwrap().unwrap();
    assert_eq!(old.status, MemoryStatus::Superseded);
}

#[test]
fn failed_records_leave_no_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("m");
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    store.set_write_through(Some(dir.clone()));

    let mut bad = opts(MemoryKind::Decision, "t", "b");
    bad.supersedes_id = Some("missing".into());
    assert!(store.record_memory(bad).is_err());

    let mut bad_id = opts(MemoryKind::Decision, "t", "b");
    bad_id.id = Some("../escape".into());
    assert!(store.record_memory(bad_id).is_err());

    let mut dup = opts(MemoryKind::Decision, "t", "b");
    dup.id = Some("DEC-x".into());
    store.record_memory(dup.clone()).unwrap();
    assert!(store.record_memory(dup).is_err());

    assert_eq!(md_files(&dir), vec!["DEC-x.md"]);
}

#[test]
fn generated_ids_do_not_collide_and_consolidate_writes_through() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("m");
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    store.set_write_through(Some(dir.clone()));

    let a = store
        .record_memory(opts(MemoryKind::Learning, "Same", "1"))
        .unwrap();
    let b = store
        .record_memory(opts(MemoryKind::Learning, "Same", "2"))
        .unwrap();
    assert_ne!(a.id, b.id);
    assert!(b.id.ends_with("-same-2"));

    let summary = store
        .consolidate_session(
            "s1",
            vec![SessionLearning {
                title: "Session insight".into(),
                body: "b".into(),
                tags: None,
                kind: MemoryKind::Learning,
                paths: Vec::new(),
                commit: None,
            }],
            true,
        )
        .unwrap();
    let id = &summary.recorded_ids[0];
    assert!(dir.join(format!("{id}.md")).exists());
    assert_eq!(md_files(&dir).len(), 3);
}
