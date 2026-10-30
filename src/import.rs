//! Rebuild the memdag index from git-tracked text: `Decision:` commit trailers, ADRs,
//! CONTRACT §6 note files and (optionally) GitHub handoff comments.
//!
//! The SQLite DB is a disposable cache. Every source has a stable id, and imports upsert by
//! id, so re-running an import never duplicates and deleting the DB then importing again
//! reproduces it.

use crate::models::{MemoryKind, RelationType};
use crate::notes::{
    is_valid_commit, is_valid_repo_path, join_paths, join_tags, parse_note, split_front_matter,
    split_title,
};
use crate::store::MemoryStore;
use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DEFAULT_ADR_DIR: &str = "docs/adr";

/// Env var that disables the automatic refresh on `serve` startup / CLI search.
pub const NO_AUTO_IMPORT_ENV: &str = "MEMDAG_NO_AUTO_IMPORT";

#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub git: bool,
    pub adr: bool,
    pub notes: bool,
    pub gh: bool,
    /// Only scan commits in `<since>..HEAD` (ADRs and notes are always rescanned; it's cheap).
    pub since: Option<String>,
    /// Repo-relative (or absolute) directories.
    pub adr_dir: PathBuf,
    pub notes_dir: PathBuf,
}

impl ImportOptions {
    /// All sources, including GitHub.
    pub fn all() -> Self {
        Self {
            git: true,
            adr: true,
            notes: true,
            gh: true,
            since: None,
            adr_dir: PathBuf::from(DEFAULT_ADR_DIR),
            notes_dir: PathBuf::from(crate::notes::DEFAULT_NOTES_DIR),
        }
    }

    /// The fast, local sources used by auto-refresh (no `gh`).
    pub fn local() -> Self {
        Self {
            gh: false,
            ..Self::all()
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ImportReport {
    pub git: usize,
    pub adr: usize,
    pub notes: usize,
    pub gh: usize,
    /// Rows inserted or whose content changed.
    pub changed: usize,
    /// Relations newly created.
    pub edges: usize,
    /// Relations still waiting for their target to be imported.
    pub pending: usize,
    /// HEAD recorded as imported (set when the git source ran).
    pub head: Option<String>,
    pub warnings: Vec<String>,
}

/// One GitHub issue comment, as much of it as the importer needs.
#[derive(Debug, Clone, Default)]
pub struct GhComment {
    pub id: u64,
    pub issue_number: Option<u64>,
    pub body: String,
    pub created_at: Option<String>,
    pub author: Option<String>,
    pub url: Option<String>,
}

/// Where issue comments come from. [`GhCli`] in production; tests inject a fake.
pub trait CommentSource {
    fn fetch_comments(&self, repo: &Path) -> Result<Vec<GhComment>>;
}

/// Fetches comments via `gh api --paginate repos/{owner}/{repo}/issues/comments`, run inside
/// the repo so `gh` resolves `{owner}/{repo}` from its remote.
pub struct GhCli;

impl CommentSource for GhCli {
    fn fetch_comments(&self, repo: &Path) -> Result<Vec<GhComment>> {
        let out = Command::new("gh")
            .args([
                "api",
                "--paginate",
                "repos/{owner}/{repo}/issues/comments?per_page=100",
            ])
            .current_dir(repo)
            .output()
            .context("`gh` CLI not found on PATH")?;
        if !out.status.success() {
            bail!(
                "`gh api` failed (is gh authenticated? try `gh auth status`): {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        parse_gh_comments(&String::from_utf8_lossy(&out.stdout))
    }
}

/// `gh --paginate` prints one JSON array per page (concatenated); accept that or one array.
pub fn parse_gh_comments(raw: &str) -> Result<Vec<GhComment>> {
    let mut out = Vec::new();
    for value in serde_json::Deserializer::from_str(raw).into_iter::<serde_json::Value>() {
        let value = value.context("Failed to parse `gh api` output as JSON")?;
        let items = match value {
            serde_json::Value::Array(a) => a,
            other => vec![other],
        };
        for c in items {
            let Some(id) = c.get("id").and_then(|v| v.as_u64()) else {
                continue;
            };
            let s = |k: &str| c.get(k).and_then(|v| v.as_str()).map(str::to_string);
            out.push(GhComment {
                id,
                issue_number: s("issue_url")
                    .and_then(|u| u.rsplit('/').next().and_then(|n| n.parse().ok())),
                body: s("body").unwrap_or_default(),
                created_at: s("created_at"),
                author: c
                    .get("user")
                    .and_then(|u| u.get("login"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                url: s("html_url"),
            });
        }
    }
    Ok(out)
}

pub const HANDOFF_MARKER: &str = "<!-- handoff -->";

#[derive(Debug, Clone)]
struct Item {
    id: String,
    kind: MemoryKind,
    title: String,
    body: String,
    tags: Vec<String>,
    created_at: Option<String>,
    /// The source itself says it's superseded (e.g. an ADR's Status line).
    superseded: bool,
    /// Provenance (notes only): files the item cites, and the commit it was recorded at.
    paths: Vec<String>,
    commit: Option<String>,
}

#[derive(Debug, Default)]
struct Batch {
    items: Vec<Item>,
    /// (source, target, relation)
    edges: Vec<(String, String, RelationType)>,
}

/// Nearest ancestor (inclusive) containing `.git` (a dir, or a file in a worktree).
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

pub(crate) fn git(repo: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("`git` not found on PATH")
}

/// Current HEAD sha, or None if not a git repo / no commits yet / git missing.
pub fn git_head(repo: &Path) -> Option<String> {
    let out = git(repo, &["rev-parse", "--verify", "-q", "HEAD"]).ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn is_ancestor(repo: &Path, ancestor: &str, head: &str) -> bool {
    git(repo, &["merge-base", "--is-ancestor", ancestor, head])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn head_meta_key(repo: &Path) -> String {
    let path = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    format!("last_import_head:{}", path.display())
}

/// A ref that names another memory (ADR-0003, DEC-..., GIT-abc1234) rather than an issue
/// (`#719`) or URL; only these become `references` edges.
fn looks_like_memory_id(r: &str) -> bool {
    let Some((prefix, rest)) = r.split_once('-') else {
        return false;
    };
    !prefix.is_empty()
        && prefix.chars().all(|c| c.is_ascii_uppercase())
        && !rest.is_empty()
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn split_refs(value: &str) -> Vec<String> {
    value
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\''))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn ref_edges(source: &str, refs: &[String], batch: &mut Batch) {
    for r in refs.iter().filter(|r| looks_like_memory_id(r)) {
        batch
            .edges
            .push((source.to_string(), r.clone(), RelationType::References));
    }
}

/// Drop a trailing paragraph made only of `Token: value` trailer lines.
fn strip_trailers(body: &str) -> String {
    let body = body.trim_end();
    let (head, last) = match body.rfind("\n\n") {
        Some(i) => (&body[..i], &body[i + 2..]),
        None => ("", body),
    };
    let is_trailer_block = !last.trim().is_empty()
        && last.lines().all(|l| {
            l.starts_with([' ', '\t'])
                || l.split_once(": ").is_some_and(|(k, _)| {
                    !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                })
        });
    if is_trailer_block {
        head.trim_end().to_string()
    } else {
        body.to_string()
    }
}

const FIELD: char = '\x1f';
const MULTI: char = '\x1d';

fn collect_git(
    repo: &Path,
    since: Option<&str>,
    batch: &mut Batch,
    report: &mut ImportReport,
) -> Result<()> {
    let Some(head) = git_head(repo) else {
        report
            .warnings
            .push("git: not a git repository or no commits yet; skipped".into());
        return Ok(());
    };
    let range = match since {
        Some(s) => format!("{s}..{head}"),
        None => head.clone(),
    };
    let format = "--format=%H%x1f%cI%x1f%s%x1f%b%x1f\
                  %(trailers:key=Decision,valueonly,separator=%x1d)%x1f\
                  %(trailers:key=Refs,valueonly,separator=%x1d)";
    let out = git(
        repo,
        &[
            "log",
            "-z",
            "--no-color",
            "-E",
            "--grep=^Decision:",
            format,
            &range,
            "--",
        ],
    )?;
    if !out.status.success() {
        bail!(
            "git log {range} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    for record in raw.split('\0') {
        let f: Vec<&str> = record.split(FIELD).collect();
        if f.len() < 6 {
            continue;
        }
        let sha = f[0].trim();
        let decisions: Vec<&str> = f[4]
            .split(MULTI)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        let Some(title) = decisions.first() else {
            continue;
        };
        if sha.len() < 7 {
            continue;
        }
        let id = format!("GIT-{}", &sha[..7]);
        let refs: Vec<String> = f[5].split(MULTI).flat_map(split_refs).collect();

        let mut body = f[2].trim().to_string();
        let rest = strip_trailers(f[3]);
        if !rest.trim().is_empty() {
            body.push_str("\n\n");
            body.push_str(rest.trim());
        }
        for extra in &decisions[1..] {
            body.push_str(&format!("\n\nDecision: {extra}"));
        }

        let mut tags = refs.clone();
        tags.push("git".into());
        ref_edges(&id, &refs, batch);
        batch.items.push(Item {
            id,
            kind: MemoryKind::Decision,
            title: title.to_string(),
            body,
            tags,
            created_at: Some(f[1].trim().to_string()),
            superseded: false,
            paths: Vec::new(),
            commit: None,
        });
        report.git += 1;
    }
    report.head = Some(head);
    Ok(())
}

/// `NNNN-*.md` -> `NNNN`.
fn adr_number(file_name: &str) -> Option<&str> {
    let num = file_name.get(..4)?;
    (num.chars().all(|c| c.is_ascii_digit())
        && file_name[4..].starts_with('-')
        && file_name.ends_with(".md"))
    .then_some(num)
}

/// Value of a `- Status: x` / `Status: x` / `**Status**: x` metadata line.
fn adr_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|l| {
        let l = l.trim().trim_start_matches(['-', '*', ' ']);
        let (k, v) = l.split_once(':')?;
        k.trim_matches(['*', ' '])
            .eq_ignore_ascii_case(key)
            .then(|| v.trim().trim_start_matches(['*', ' ']).trim())
    })
}

fn parse_adr(num: &str, text: &str, batch: &mut Batch) {
    let id = format!("ADR-{num}");
    let (title, body) = split_title(text);
    // "# 0003. Use WAL" -> "Use WAL"
    let title = title
        .trim_start_matches(|c: char| c.is_ascii_digit())
        .trim_start_matches(['.', ':'])
        .trim()
        .to_string();

    let mut superseded = false;
    if let Some(status) = adr_field(text, "Status") {
        let lower = status.to_ascii_lowercase();
        if lower.starts_with("superseded") {
            superseded = true;
            if let Some(by) = lower.split_once("by").map(|(_, b)| b) {
                let digits: String = by
                    .chars()
                    .skip_while(|c| !c.is_ascii_digit())
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(n) = digits.parse::<u32>() {
                    batch
                        .edges
                        .push((format!("ADR-{n:04}"), id.clone(), RelationType::Supersedes));
                }
            }
        }
    }
    let refs = adr_field(text, "Refs").map(split_refs).unwrap_or_default();
    ref_edges(&id, &refs, batch);
    let mut tags = refs;
    tags.push("adr".into());

    batch.items.push(Item {
        id,
        kind: MemoryKind::Decision,
        title: if title.is_empty() {
            format!("ADR {num}")
        } else {
            title
        },
        body,
        tags,
        created_at: adr_field(text, "Date").map(str::to_string),
        superseded,
        paths: Vec::new(),
        commit: None,
    });
}

fn md_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "md") && p.is_file())
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

fn collect_adrs(dir: &Path, batch: &mut Batch, report: &mut ImportReport) {
    for path in md_files(dir) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let Some(num) = adr_number(&name) else {
            continue;
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                parse_adr(num, &text, batch);
                report.adr += 1;
            }
            Err(e) => report.warnings.push(format!("{}: {e}", path.display())),
        }
    }
}

fn collect_notes(dir: &Path, batch: &mut Batch, report: &mut ImportReport) {
    for path in md_files(dir) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            report
                .warnings
                .push(format!("{}: unreadable", path.display()));
            continue;
        };
        // README.md and other prose without front-matter are not notes.
        if split_front_matter(&text).is_none() {
            continue;
        }
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let note = match parse_note(&text, &stem) {
            Ok(n) => n,
            Err(e) => {
                report.warnings.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        if let Some(old) = &note.supersedes {
            batch
                .edges
                .push((note.id.clone(), old.clone(), RelationType::Supersedes));
        }
        ref_edges(&note.id, &note.refs, batch);
        let mut tags = note.tags;
        tags.extend(note.refs);
        batch.items.push(Item {
            id: note.id,
            kind: note.kind,
            title: note.title,
            body: note.body,
            tags,
            created_at: None,
            superseded: false,
            // Note files come from the repo, so re-validate before trusting them.
            paths: note
                .paths
                .into_iter()
                .filter(|p| is_valid_repo_path(p))
                .collect(),
            commit: note.commit.filter(|c| is_valid_commit(c)),
        });
        report.notes += 1;
    }
}

fn collect_gh(comments: Vec<GhComment>, batch: &mut Batch, report: &mut ImportReport) {
    for c in comments {
        if !c.body.contains(HANDOFF_MARKER) {
            continue;
        }
        let body = c.body.replace(HANDOFF_MARKER, "").trim().to_string();
        let title = body
            .lines()
            .map(|l| l.replace("**", "").trim().to_string())
            .find(|l| !l.is_empty())
            .unwrap_or_else(|| "Handoff".into());
        let title = match c.issue_number {
            Some(n) => format!("#{n} {title}"),
            None => title,
        };
        let mut tags = Vec::new();
        if let Some(n) = c.issue_number {
            tags.push(format!("#{n}"));
        }
        tags.push("handoff".into());
        if let Some(a) = &c.author {
            tags.push(a.clone());
        }
        let mut body = body;
        if let Some(url) = &c.url {
            body.push_str(&format!("\n\nSource: {url}"));
        }
        batch.items.push(Item {
            id: format!("GH-{}", c.id),
            kind: MemoryKind::Learning,
            title,
            body,
            tags,
            created_at: c.created_at,
            superseded: false,
            paths: Vec::new(),
            commit: None,
        });
        report.gh += 1;
    }
}

/// Import the selected sources from `repo` into `store`. Missing sources (no git, no `gh`,
/// no `docs/adr/`) are warnings, not errors.
pub fn import_repo(
    store: &mut MemoryStore,
    repo: &Path,
    opts: &ImportOptions,
    gh: &dyn CommentSource,
) -> Result<ImportReport> {
    let mut report = ImportReport::default();
    let mut batch = Batch::default();

    if opts.git
        && let Err(e) = collect_git(repo, opts.since.as_deref(), &mut batch, &mut report)
    {
        report.warnings.push(format!("git: {e:#}"));
    }
    if opts.adr {
        collect_adrs(&repo.join(&opts.adr_dir), &mut batch, &mut report);
    }
    if opts.notes {
        collect_notes(&repo.join(&opts.notes_dir), &mut batch, &mut report);
    }
    if opts.gh {
        match gh.fetch_comments(repo) {
            Ok(comments) => collect_gh(comments, &mut batch, &mut report),
            Err(e) => report.warnings.push(format!("gh: skipped ({e:#})")),
        }
    }

    apply(store, &batch, &mut report)?;

    if let Some(head) = &report.head {
        store.connection().execute(
            "INSERT INTO memdag_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![head_meta_key(repo), head],
        )?;
    }
    Ok(report)
}

fn apply(store: &mut MemoryStore, batch: &Batch, report: &mut ImportReport) -> Result<()> {
    let tx = store
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("Failed to begin import transaction")?;

    for item in &batch.items {
        let status = if item.superseded {
            "superseded"
        } else {
            "active"
        };
        let changed = tx.execute(
            r#"
            INSERT INTO memories (id, kind, title, body, tags, status, created_at, updated_at, paths, commit_sha)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, COALESCE(datetime(?7), CURRENT_TIMESTAMP), CURRENT_TIMESTAMP, ?9, ?10)
            ON CONFLICT(id) DO UPDATE SET
                kind = excluded.kind,
                title = excluded.title,
                body = excluded.body,
                tags = excluded.tags,
                paths = excluded.paths,
                commit_sha = excluded.commit_sha,
                status = CASE WHEN ?8 THEN 'superseded' ELSE memories.status END,
                updated_at = CURRENT_TIMESTAMP
            WHERE memories.kind IS NOT excluded.kind
               OR memories.title IS NOT excluded.title
               OR memories.body IS NOT excluded.body
               OR memories.tags IS NOT excluded.tags
               OR memories.paths IS NOT excluded.paths
               OR memories.commit_sha IS NOT excluded.commit_sha
               OR (?8 AND memories.status IS NOT 'superseded')
            "#,
            params![
                item.id,
                item.kind.as_str(),
                item.title,
                item.body,
                join_tags(&item.tags),
                status,
                item.created_at,
                item.superseded,
                join_paths(&item.paths),
                item.commit,
            ],
        )?;
        report.changed += changed;
        if changed > 0 {
            // The text may have changed, so any stored vector is stale; `memdag embed`
            // recomputes missing ones. Harmless when the row is new (nothing to delete).
            tx.execute("DELETE FROM memories_vec WHERE id = ?1", params![item.id])?;
        }
    }

    for (source, target, rel) in &batch.edges {
        if source != target {
            tx.execute(
                "INSERT OR IGNORE INTO pending_relations (source_id, target_id, relation_type)
                 VALUES (?1, ?2, ?3)",
                params![source, target, rel.as_str()],
            )?;
        }
    }

    // Resolve every pending edge whose endpoints now both exist (including ones left over
    // from earlier incremental imports).
    let ready: Vec<(String, String, String)> = tx
        .prepare(
            "SELECT p.source_id, p.target_id, p.relation_type FROM pending_relations p
             WHERE EXISTS (SELECT 1 FROM memories WHERE id = p.source_id)
               AND EXISTS (SELECT 1 FROM memories WHERE id = p.target_id)",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    for (source, target, rel) in &ready {
        report.edges += tx.execute(
            "INSERT OR IGNORE INTO memory_relations (source_id, target_id, relation_type)
             VALUES (?1, ?2, ?3)",
            params![source, target, rel],
        )?;
        if rel == RelationType::Supersedes.as_str() {
            tx.execute(
                "UPDATE memories SET status = 'superseded', updated_at = CURRENT_TIMESTAMP
                 WHERE id = ?1 AND status IS NOT 'superseded'",
                params![target],
            )?;
        }
        tx.execute(
            "DELETE FROM pending_relations
             WHERE source_id = ?1 AND target_id = ?2 AND relation_type = ?3",
            params![source, target, rel],
        )?;
    }
    report.pending = tx.query_row("SELECT count(*) FROM pending_relations", [], |r| {
        r.get::<_, i64>(0)
    })? as usize;

    tx.commit().context("Failed to commit import")?;
    Ok(())
}

/// The HEAD recorded by the last git import of `repo`, if any.
pub fn last_imported_head(store: &MemoryStore, repo: &Path) -> Result<Option<String>> {
    Ok(store
        .connection()
        .query_row(
            "SELECT value FROM memdag_meta WHERE key = ?1",
            params![head_meta_key(repo)],
            |r| r.get(0),
        )
        .optional()?)
}

/// If `repo`'s HEAD moved since the last import, run an incremental local import
/// (git since the last HEAD when it's an ancestor, else all of git; ADRs and notes in full).
/// Returns None when nothing needed doing. One `git rev-parse` when the index is fresh.
pub fn refresh_if_stale(
    store: &mut MemoryStore,
    repo: &Path,
    notes_dir: &Path,
) -> Result<Option<ImportReport>> {
    if std::env::var(NO_AUTO_IMPORT_ENV).is_ok_and(|v| !v.is_empty() && v != "0") {
        return Ok(None);
    }
    let Some(head) = git_head(repo) else {
        return Ok(None);
    };
    let last = last_imported_head(store, repo)?;
    if last.as_deref() == Some(head.as_str()) {
        return Ok(None);
    }
    let opts = ImportOptions {
        since: last.filter(|l| is_ancestor(repo, l, &head)),
        notes_dir: notes_dir.to_path_buf(),
        ..ImportOptions::local()
    };
    import_repo(store, repo, &opts, &GhCli).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_trailer_paragraph_only() {
        assert_eq!(
            strip_trailers("Why.\n\nRefs: #1\nDecision: x\nAssisted-by: opencode\n"),
            "Why."
        );
        assert_eq!(
            strip_trailers("Why.\n\nMore prose here."),
            "Why.\n\nMore prose here."
        );
        assert_eq!(strip_trailers("Refs: #1"), "");
    }

    #[test]
    fn memory_id_refs() {
        assert!(looks_like_memory_id("ADR-0003"));
        assert!(looks_like_memory_id("DEC-2026-10-30-x"));
        assert!(!looks_like_memory_id("#719"));
        assert!(!looks_like_memory_id("hiranp/email-agent#752"));
        assert!(!looks_like_memory_id("https://x"));
    }

    #[test]
    fn adr_parsing() {
        let mut b = Batch::default();
        parse_adr(
            "0001",
            "# 0001. Use WAL\n- Status: superseded by [0004](0004-x.md)\n- Date: 2026-10-01\n- Refs: #7, ADR-0002\n## Context\nc\n",
            &mut b,
        );
        let item = &b.items[0];
        assert_eq!(item.id, "ADR-0001");
        assert_eq!(item.title, "Use WAL");
        assert!(item.superseded);
        assert_eq!(item.created_at.as_deref(), Some("2026-10-01"));
        assert_eq!(item.tags, vec!["#7", "ADR-0002", "adr"]);
        assert!(b.edges.contains(&(
            "ADR-0004".into(),
            "ADR-0001".into(),
            RelationType::Supersedes
        )));
        assert!(b.edges.contains(&(
            "ADR-0001".into(),
            "ADR-0002".into(),
            RelationType::References
        )));
        assert_eq!(adr_number("0012-x.md"), Some("0012"));
        assert_eq!(adr_number("README.md"), None);
    }

    #[test]
    fn gh_paginated_output() {
        let raw = r#"[{"id":1,"body":"a","issue_url":"https://api.github.com/repos/o/r/issues/752","user":{"login":"u"}}]
[{"id":2,"body":"b"}]"#;
        let c = parse_gh_comments(raw).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].issue_number, Some(752));
        assert_eq!(c[0].author.as_deref(), Some("u"));
    }
}
