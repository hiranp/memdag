//! Staleness is checked for many memories with a constant number of git processes, and
//! the batched answers are exactly the one-at-a-time answers.

use memdag::db::open_in_memory;
use memdag::import::git_spawns_on_this_thread;
use memdag::models::{Memory, MemoryKind};
use memdag::provenance;
use memdag::store::{MemoryStore, RecordOptions, SearchOptions};
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

fn write(repo: &Path, rel: &str, text: &str) {
    let p = repo.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn commit_all(repo: &Path) -> String {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "--allow-empty", "-m", "c"]);
    git(repo, &["rev-parse", "HEAD"])
}

fn repo_store(dir: &Path) -> MemoryStore {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    let mut store = MemoryStore::new(open_in_memory().unwrap());
    store.set_repo_root(Some(dir.to_path_buf()));
    store
}

fn record(store: &mut MemoryStore, title: &str, paths: &[&str], commit: Option<&str>) -> Memory {
    store
        .record_memory(RecordOptions {
            id: None,
            kind: MemoryKind::Decision,
            title: title.into(),
            body: "batched staleness body".into(),
            tags: None,
            supersedes_id: None,
            session_id: None,
            embedding: None,
            paths: paths.iter().map(|p| p.to_string()).collect(),
            commit: commit.map(str::to_string),
        })
        .unwrap()
}

#[test]
fn search_checks_staleness_of_many_hits_with_a_constant_number_of_git_processes() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    let mut store = repo_store(repo);
    for i in 0..20 {
        for f in ["a", "b", "c"] {
            write(repo, &format!("m{i}/{f}.rs"), &format!("// {i} {f}\n"));
        }
    }
    let base = commit_all(repo);
    for i in 0..20 {
        let p: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|f| format!("m{i}/{f}.rs"))
            .collect();
        let p: Vec<&str> = p.iter().map(String::as_str).collect();
        let before = git_spawns_on_this_thread();
        record(&mut store, &format!("widget files {i}"), &p, None);
        // HEAD plus one hash-object for all three files.
        assert!(git_spawns_on_this_thread() - before <= 2);
    }
    // Directories and commit-only baselines in the same result set.
    for i in 0..5 {
        record(
            &mut store,
            &format!("widget dir {i}"),
            &[&format!("m{i}")],
            None,
        );
        record(
            &mut store,
            &format!("widget commit {i}"),
            &[&format!("m{i}/a.rs")],
            Some(&base),
        );
    }
    for i in [0, 3, 7] {
        write(repo, &format!("m{i}/a.rs"), "// changed\n");
    }

    let before = git_spawns_on_this_thread();
    let hits = store
        .search_memory(SearchOptions {
            query: "widget".into(),
            limit: 100,
            ..Default::default()
        })
        .unwrap();
    let spawned = git_spawns_on_this_thread() - before;
    assert_eq!(hits.len(), 30);
    // ls-files, hash-object (files + directory members), hash-object (directory digests),
    // and one diff for the five memories sharing a commit. One-at-a-time this was 77.
    assert!(spawned <= 4, "search spawned {spawned} git processes");

    let mut stale: Vec<(String, Vec<String>)> = hits
        .iter()
        .filter(|h| !h.stale_paths.is_empty())
        .map(|h| (h.memory.title.clone(), h.stale_paths.clone()))
        .collect();
    stale.sort();
    let expected: Vec<(String, Vec<String>)> = [
        ("widget commit 0", "m0/a.rs"),
        ("widget commit 3", "m3/a.rs"),
        ("widget dir 0", "m0"),
        ("widget dir 3", "m3"),
        ("widget files 0", "m0/a.rs"),
        ("widget files 3", "m3/a.rs"),
        ("widget files 7", "m7/a.rs"),
    ]
    .iter()
    .map(|(t, p)| (t.to_string(), vec![p.to_string()]))
    .collect();
    assert_eq!(stale, expected);

    let before = git_spawns_on_this_thread();
    assert_eq!(store.list_stale().unwrap().len(), 7);
    assert!(git_spawns_on_this_thread() - before <= 4);
}

#[test]
fn batched_fingerprints_and_staleness_match_one_at_a_time() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    let mut store = repo_store(repo);
    write(repo, "src/a.rs", "fn a() {}\n");
    write(repo, "src/b.rs", "fn b() {}\n");
    write(repo, "src/sub/c.rs", "fn c() {}\n");
    write(repo, "src-x/d.rs", "fn d() {}\n");
    write(repo, "docs/readme.md", "hello\n");
    write(repo, "empty/.keep", "");
    let base = commit_all(repo);
    std::fs::remove_file(repo.join("empty/.keep")).unwrap();
    write(repo, "untracked/u.rs", "fn u() {}\n");

    let mixes: &[&[&str]] = &[
        &["src/a.rs", "src/b.rs", "empty"],
        &["src", "src/sub", "src/a.rs"],
        &["src/sub/", "src-x"],
        // At most one absent path (or directory without tracked files) per memory: equal
        // fingerprints within a memory are collapsed when stored (`join_paths`), which
        // drops the memory to its commit baseline.
        &["missing.rs", "docs"],
        &["docs/readme.md", "untracked"],
        &["src-x/d.rs", "src/missing/dir"],
    ];
    let mut by_content = Vec::new();
    let mut by_commit = Vec::new();
    for (i, paths) in mixes.iter().enumerate() {
        by_content.push(record(&mut store, &format!("content {i}"), paths, None));
        by_commit.push(record(
            &mut store,
            &format!("commit {i}"),
            paths,
            Some(&base),
        ));
    }
    // Paths as a hand-written note may spell them (not normalised by `clean_provenance`).
    let mut odd = by_content[1].clone();
    odd.paths = vec!["./src/a.rs".into(), "src//sub".into(), "src/./b.rs".into()];
    odd.fingerprints = provenance::fingerprints(repo, &odd.paths).unwrap();
    by_content.push(odd.clone());
    odd.fingerprints.clear();
    odd.commit = Some(base.clone());
    by_commit.push(odd);
    // A memory with an invalid path is never checked.
    let mut invalid = by_commit[0].clone();
    invalid.paths.push("../outside".into());
    by_commit.push(invalid);

    // Change some content: a file, a file inside a directory, a new file in a directory,
    // and a cited path that now appears. Leave others untouched.
    write(repo, "src/b.rs", "fn b() { changed }\n");
    write(repo, "src/sub/c.rs", "fn c() { changed }\n");
    write(repo, "src-x/new.rs", "fn n() {}\n");
    git(repo, &["add", "src-x/new.rs"]);
    write(repo, "missing.rs", "now here\n");

    let all: Vec<String> = mixes
        .iter()
        .flat_map(|m| m.iter().map(|p| p.to_string()))
        .chain(["./src/a.rs", "src//sub", "src/./b.rs"].map(String::from))
        .collect();
    let many = provenance::fingerprints_many(repo, &all);
    for p in &all {
        assert_eq!(
            many.get(p),
            provenance::fingerprint(repo, p).as_ref(),
            "fingerprint of {p}"
        );
    }

    // Without the oddly spelled memories, each kind of baseline is checked in one batch.
    let is_plain = |m: &&Memory| {
        m.paths
            .iter()
            .all(|p| !p.contains("./") && !p.contains("//"))
    };
    let plain: Vec<&Memory> = by_content
        .iter()
        .chain(&by_commit)
        .filter(is_plain)
        .collect();
    let before = git_spawns_on_this_thread();
    let batched = store.stale_paths_many(&plain);
    // ls-files, two hash-objects, one diff: the batched paths really ran.
    let spawned = git_spawns_on_this_thread() - before;
    assert!(spawned <= 4, "{spawned}");
    let single: Vec<Vec<String>> = plain.iter().map(|m| store.stale_paths(m)).collect();
    assert_eq!(batched, single);

    let memories: Vec<&Memory> = by_content.iter().chain(&by_commit).collect();
    let batched = store.stale_paths_many(&memories);
    let single: Vec<Vec<String>> = memories.iter().map(|m| store.stale_paths(m)).collect();
    assert_eq!(batched, single);
    assert!(
        batched.iter().filter(|s| !s.is_empty()).count() >= 6,
        "the mix must exercise changed paths: {batched:?}"
    );
    assert_eq!(single[0], vec!["src/b.rs"]);
    assert_eq!(single[1], vec!["src", "src/sub"]);
    assert_eq!(single[3], vec!["missing.rs"]);
    assert!(single[4].is_empty(), "untouched: {:?}", single[4]);
}

#[test]
fn no_repo_means_no_staleness_and_no_git() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = repo_store(tmp.path());
    write(tmp.path(), "a.rs", "a\n");
    commit_all(tmp.path());
    let mem = record(&mut store, "a", &["a.rs"], None);
    write(tmp.path(), "a.rs", "b\n");
    store.set_repo_root(None);
    let before = git_spawns_on_this_thread();
    assert_eq!(
        store.stale_paths_many(&[&mem, &mem]),
        vec![Vec::<String>::new(); 2]
    );
    assert_eq!(git_spawns_on_this_thread(), before);
}
