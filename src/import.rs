//! Rebuild the memdag index from git-tracked text: `Decision:` commit trailers, ADRs,
//! CONTRACT §6 note files and (optionally) GitHub handoff comments.
//!
//! The SQLite DB is a disposable cache. Every source has a stable id, and imports upsert by
//! id, so re-running an import never duplicates and deleting the DB then importing again
//! reproduces it.

use crate::models::{MemoryKind, RelationType};
use crate::notes::{
    RESOLVED_SUFFIX, is_valid_commit, is_valid_note_id, is_valid_repo_path, join_paths, join_tags,
    parse_note, parse_resolution, resolution_suffix, split_front_matter, split_title,
};
use crate::provenance::is_valid_fingerprint;
use crate::store::MemoryStore;
use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::collections::{HashMap, HashSet};
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
    /// Remember the imported HEAD so auto-refresh only scans newer commits. Turn off for a
    /// partial `--since` import, or older decisions would never be picked up later.
    pub record_head: bool,
}

/// Cap on an imported item's body. Real decisions are a few KB; anything this large is more
/// likely pasted logs or an attempt to flood the model's context.
pub const MAX_IMPORT_BODY_BYTES: usize = 20_000;

/// GitHub `author_association` values trusted to write handoff comments. Anyone else
/// (CONTRIBUTOR, NONE, ...) can comment on a public issue, so their text never becomes
/// agent-visible memory.
pub const TRUSTED_ASSOCIATIONS: &[&str] = &["OWNER", "MEMBER", "COLLABORATOR"];

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
            record_head: true,
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
    /// Rows removed because their source (note, ADR, commit, comment) no longer exists.
    pub pruned: usize,
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
    /// GitHub's `author_association` (OWNER, MEMBER, COLLABORATOR, CONTRIBUTOR, NONE, ...).
    pub association: Option<String>,
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
                association: s("author_association"),
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
    /// A `<id>.resolved.md` sidecar says this note was resolved.
    resolved: bool,
    /// Provenance (notes only): files the item cites, and the commit it was recorded at.
    paths: Vec<String>,
    commit: Option<String>,
    fingerprints: Vec<String>,
    /// Which importer produced this row: `git`, `adr`, `note` or `gh`.
    source: &'static str,
}

#[derive(Debug, Default)]
struct Batch {
    items: Vec<Item>,
    /// (source, target, relation)
    edges: Vec<(String, String, RelationType)>,
    /// Sources that were scanned completely, so rows they own but didn't produce are gone.
    prune: Vec<&'static str>,
    /// Ids to keep although no item was produced for them (the file exists but couldn't be
    /// read or parsed this time; deleting its row would be a surprise).
    keep: HashMap<&'static str, HashSet<String>>,
}

impl Batch {
    fn keep(&mut self, source: &'static str, id: String) {
        self.keep.entry(source).or_default().insert(id);
    }
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
        // Read-only background calls must not take the index lock and contend with the
        // user's own git commands.
        .env("GIT_OPTIONAL_LOCKS", "0")
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

/// The last imported HEAD is per checkout: every worktree has its own HEAD.
fn head_meta_key(repo: &Path) -> String {
    format!("last_import_head:{}", crate::checkout::worktree_key(repo))
}

/// Who an import is for: the repository (shared by its worktrees) and this checkout.
struct Checkout {
    /// `memories.origin_repo`: rows of other repositories sharing the database are never
    /// touched.
    repo: String,
    /// `memory_owners.worktree` for rows this checkout's files produce.
    worktree: String,
    /// Every checkout of the repository git still knows about. Also accepted as an
    /// `origin_repo`, which held a checkout path before schema v5.
    worktrees: Vec<String>,
    /// Whether `worktrees` came from git's metadata (so claims of checkouts not in it can be
    /// dropped as removed worktrees).
    known: bool,
}

impl Checkout {
    fn of(repo: &Path) -> Self {
        let worktree = crate::checkout::worktree_key(repo);
        let listed = crate::checkout::worktree_keys(repo);
        Self {
            repo: crate::checkout::repository_key(repo),
            known: listed.is_some(),
            worktrees: listed.unwrap_or_else(|| vec![worktree.clone()]),
            worktree,
        }
    }

    fn is_same_repo(&self, origin: Option<&str>) -> bool {
        origin.is_none_or(|o| o == self.repo || self.worktrees.iter().any(|w| w == o))
    }
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
    if let Some(s) = since
        && (s.starts_with('-') || s.contains(char::is_whitespace))
    {
        bail!("--since '{s}' is not a valid revision");
    }
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
            resolved: false,
            paths: Vec::new(),
            commit: None,
            fingerprints: Vec::new(),
            source: "git",
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
        resolved: false,
        paths: Vec::new(),
        commit: None,
        fingerprints: Vec::new(),
        source: "adr",
    });
}

/// `*.md` files in `dir`, sorted. A missing directory is empty; any other read error is an
/// error, so the caller knows the scan was incomplete and must not prune.
fn md_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut files = Vec::new();
    for entry in rd {
        let p = entry?.path();
        if p.extension().is_some_and(|e| e == "md") && p.is_file() {
            files.push(p);
        }
    }
    files.sort();
    Ok(files)
}

/// Returns whether the directory was scanned completely.
fn collect_adrs(dir: &Path, batch: &mut Batch, report: &mut ImportReport) -> bool {
    let files = match md_files(dir) {
        Ok(f) => f,
        Err(e) => {
            report.warnings.push(format!("{}: {e}", dir.display()));
            return false;
        }
    };
    for path in files {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let Some(num) = adr_number(&name) else {
            continue;
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                parse_adr(num, &text, batch);
                report.adr += 1;
            }
            Err(e) => {
                report.warnings.push(format!("{}: {e}", path.display()));
                batch.keep("adr", format!("ADR-{num}"));
            }
        }
    }
    true
}

/// Returns whether the directory was scanned completely.
fn collect_notes(dir: &Path, batch: &mut Batch, report: &mut ImportReport) -> bool {
    let files = match md_files(dir) {
        Ok(f) => f,
        Err(e) => {
            report.warnings.push(format!("{}: {e}", dir.display()));
            return false;
        }
    };
    let mut resolutions: Vec<(String, Option<String>, PathBuf)> = Vec::new();
    for path in files {
        let stem = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let Ok(text) = std::fs::read_to_string(&path) else {
            report
                .warnings
                .push(format!("{}: unreadable", path.display()));
            batch.keep("note", stem);
            continue;
        };
        if let Some(target) = stem.strip_suffix(RESOLVED_SUFFIX) {
            match parse_resolution(&text) {
                Ok((id, note)) if id == target => resolutions.push((id, note, path)),
                Ok((id, _)) => report.warnings.push(format!(
                    "{}: resolves '{id}' but must be named {id}{RESOLVED_SUFFIX}.md; skipped",
                    path.display()
                )),
                Err(e) => report.warnings.push(format!("{}: {e}", path.display())),
            }
            continue;
        }
        // README.md and other prose without front-matter are not notes.
        if split_front_matter(&text).is_none() {
            continue;
        }
        let note = match parse_note(&text, &stem) {
            Ok(n) => n,
            Err(e) => {
                report.warnings.push(format!("{}: {e}", path.display()));
                batch.keep("note", stem);
                continue;
            }
        };
        // The file name is the identity. Trusting a front-matter `id:` that differs would let
        // any note overwrite an unrelated memory by claiming its id.
        if note.id != stem || !is_valid_note_id(&note.id) {
            report.warnings.push(format!(
                "{}: note id '{}' must equal the file name and be a valid id; skipped",
                path.display(),
                note.id
            ));
            batch.keep("note", stem);
            continue;
        }
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
            resolved: false,
            // Note files come from the repo, so re-validate before trusting them. Fingerprints
            // are aligned with paths, so they survive only if every path does.
            fingerprints: if note.paths.iter().all(|p| is_valid_repo_path(p))
                && note.fingerprints.len() == note.paths.len()
                && note.fingerprints.iter().all(|f| is_valid_fingerprint(f))
            {
                note.fingerprints
            } else {
                Vec::new()
            },
            paths: note
                .paths
                .into_iter()
                .filter(|p| is_valid_repo_path(p))
                .collect(),
            commit: note.commit.filter(|c| is_valid_commit(c)),
            source: "note",
        });
        report.notes += 1;
    }

    // Merge each resolution into its note, reproducing what `resolve_memory` did to the row.
    for (id, note, path) in resolutions {
        match batch
            .items
            .iter_mut()
            .find(|i| i.source == "note" && i.id == id)
        {
            Some(item) => {
                item.body.push_str(&resolution_suffix(note.as_deref()));
                item.resolved = true;
            }
            None => report.warnings.push(format!(
                "{}: no note '{id}' to resolve; skipped",
                path.display()
            )),
        }
    }
    true
}

fn collect_gh(comments: Vec<GhComment>, batch: &mut Batch, report: &mut ImportReport) {
    for c in comments {
        // The marker must open the comment: a reply that quotes a handoff would otherwise
        // import a duplicate (and quoting is how someone untrusted gets text near the marker).
        if !c.body.trim_start().starts_with(HANDOFF_MARKER) {
            continue;
        }
        // Fail closed: a comment with no association, or from outside the team, is ignored.
        let trusted = c
            .association
            .as_deref()
            .is_some_and(|a| TRUSTED_ASSOCIATIONS.contains(&a));
        if !trusted {
            report.warnings.push(format!(
                "gh: ignored handoff comment {} from an untrusted author ({})",
                c.id,
                c.association.as_deref().unwrap_or("unknown association")
            ));
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
            resolved: false,
            paths: Vec::new(),
            commit: None,
            fingerprints: Vec::new(),
            source: "gh",
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

    if opts.git {
        match collect_git(repo, opts.since.as_deref(), &mut batch, &mut report) {
            // Only a full history scan proves a commit is gone (amended, rebased away, or on
            // a branch that isn't checked out); an incremental `--since` scan proves nothing.
            Ok(()) if opts.since.is_none() && report.head.is_some() => batch.prune.push("git"),
            Ok(()) => {}
            Err(e) => report.warnings.push(format!("git: {e:#}")),
        }
    }
    if opts.adr && collect_adrs(&repo.join(&opts.adr_dir), &mut batch, &mut report) {
        batch.prune.push("adr");
    }
    if opts.notes && collect_notes(&repo.join(&opts.notes_dir), &mut batch, &mut report) {
        batch.prune.push("note");
    }
    if opts.gh {
        match gh.fetch_comments(repo) {
            Ok(comments) => {
                collect_gh(comments, &mut batch, &mut report);
                batch.prune.push("gh");
            }
            Err(e) => report.warnings.push(format!("gh: skipped ({e:#})")),
        }
    }

    apply(store, &batch, &mut report, &Checkout::of(repo))?;

    if opts.record_head
        && let Some(head) = &report.head
    {
        store.connection().execute(
            "INSERT INTO memdag_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![head_meta_key(repo), head],
        )?;
    }
    Ok(report)
}

/// Kinds that note files may own; imported notes may adopt a local row only of these kinds.
fn is_note_kind(kind: &str) -> bool {
    matches!(kind, "decision" | "invariant" | "learning" | "blocker")
}

fn apply(
    store: &mut MemoryStore,
    batch: &Batch,
    report: &mut ImportReport,
    checkout: &Checkout,
) -> Result<()> {
    let repo = checkout.repo.as_str();

    let tx = store
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("Failed to begin import transaction")?;

    // Before schema v5, `origin_repo` held the importing checkout's path. Move such rows to the
    // repository key, or the pruning below (which matches the repository) would never reach
    // one whose source was deleted before the upgrade.
    for w in &checkout.worktrees {
        if w != repo {
            tx.execute(
                "UPDATE memories SET origin_repo = ?1 WHERE origin_repo = ?2 AND source IS NOT NULL",
                params![repo, w],
            )?;
        }
    }

    // Ids whose import was refused; edges that start from them are dropped too, or a
    // refused item could still rewrite the graph through its `supersedes:` line.
    let mut refused: HashSet<&str> = HashSet::new();
    let mut applied: Vec<&Item> = Vec::new();

    for item in &batch.items {
        // Imported text reaches the agent's context, so it gets the same guards as a direct
        // write, plus a size cap.
        if let Err(e) = crate::store::check_for_secrets(&item.title, &item.body, None) {
            report
                .warnings
                .push(format!("{} not imported: {e}", item.id));
            refused.insert(&item.id);
            continue;
        }
        if item.body.len() > MAX_IMPORT_BODY_BYTES {
            report.warnings.push(format!(
                "{} not imported: body is {} bytes (limit {MAX_IMPORT_BODY_BYTES})",
                item.id,
                item.body.len()
            ));
            refused.insert(&item.id);
            continue;
        }

        // An import may refresh only rows it owns (same source, same repo), or adopt a
        // source-less row that a note file legitimately describes. Anything else is an id
        // collision with a locally recorded memory, a different source, or another repo
        // sharing this database, and must not be overwritten.
        let existing: Option<(Option<String>, String, Option<String>)> = tx
            .query_row(
                "SELECT source, kind, origin_repo FROM memories WHERE id = ?1",
                params![item.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((src, kind, origin)) = &existing {
            let same_source = src.as_deref() == Some(item.source);
            let same_repo = checkout.is_same_repo(origin.as_deref());
            let adoptable = src.is_none() && item.source == "note" && is_note_kind(kind);
            if !(same_source && same_repo) && !adoptable {
                let owner = match (src.as_deref(), same_source) {
                    (Some(s), true) => format!("{s} memory from another repository"),
                    (Some(s), false) => format!("{s} memory"),
                    (None, _) => "locally recorded memory".into(),
                };
                report.warnings.push(format!(
                    "{} not imported: id already used by a {owner}",
                    item.id
                ));
                refused.insert(&item.id);
                continue;
            }
        }

        let status = if item.superseded {
            "superseded"
        } else if item.resolved {
            "resolved"
        } else {
            "active"
        };
        report.changed += tx.execute(
            r#"
            INSERT INTO memories (id, kind, title, body, tags, status, created_at, updated_at, paths, commit_sha, fingerprints, source, origin_repo)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, COALESCE(datetime(?7), CURRENT_TIMESTAMP), CURRENT_TIMESTAMP, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT(id) DO UPDATE SET
                kind = excluded.kind,
                title = excluded.title,
                body = excluded.body,
                tags = excluded.tags,
                paths = excluded.paths,
                commit_sha = excluded.commit_sha,
                fingerprints = excluded.fingerprints,
                source = excluded.source,
                origin_repo = excluded.origin_repo,
                -- Worker keywords describe the old text; keep them only if it was appended to.
                keywords = CASE
                    WHEN memories.title IS NOT excluded.title
                      OR substr(excluded.body, 1, length(memories.body)) IS NOT memories.body
                    THEN NULL ELSE memories.keywords END,
                status = CASE
                    WHEN ?8 THEN 'superseded'
                    WHEN ?14 AND memories.status IS NOT 'superseded' THEN 'resolved'
                    ELSE memories.status END,
                updated_at = CURRENT_TIMESTAMP
            WHERE memories.kind IS NOT excluded.kind
               OR memories.title IS NOT excluded.title
               OR memories.body IS NOT excluded.body
               OR memories.tags IS NOT excluded.tags
               OR memories.paths IS NOT excluded.paths
               OR memories.commit_sha IS NOT excluded.commit_sha
               OR memories.fingerprints IS NOT excluded.fingerprints
               OR memories.source IS NOT excluded.source
               OR memories.origin_repo IS NOT excluded.origin_repo
               OR (?8 AND memories.status IS NOT 'superseded')
               OR (?14 AND memories.status NOT IN ('resolved', 'superseded'))
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
                join_paths(&item.fingerprints),
                item.source,
                repo,
                item.resolved,
            ],
        )?;
        // This checkout's files contain the item's source.
        tx.execute(
            "INSERT OR IGNORE INTO memory_owners (id, worktree) VALUES (?1, ?2)",
            params![item.id, checkout.worktree],
        )?;
        applied.push(item);
        // A changed title/body drops the stored vector via a trigger; `memdag embed`
        // (or the auto-refresh) recomputes it.
    }

    // Targets that may have lost their last `supersedes` edge in this run.
    let mut unsuperseded: Vec<String> = Vec::new();

    // Supersession is re-derived from the files: an applied row keeps exactly the outgoing
    // `supersedes` edges its source still states. (Other relation types may have been added
    // by agents through link_entities and are left alone.)
    let supersedes = RelationType::Supersedes.as_str();
    let desired: HashSet<(&str, &str)> = batch
        .edges
        .iter()
        .filter(|(_, _, r)| *r == RelationType::Supersedes)
        .map(|(s, t, _)| (s.as_str(), t.as_str()))
        .collect();
    for item in &applied {
        let current: Vec<String> = tx
            .prepare(
                "SELECT target_id FROM memory_relations WHERE source_id = ?1 AND relation_type = ?2
                 UNION SELECT target_id FROM pending_relations WHERE source_id = ?1 AND relation_type = ?2",
            )?
            .query_map(params![item.id, supersedes], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for target in current {
            if !desired.contains(&(item.id.as_str(), target.as_str())) {
                tx.execute(
                    "DELETE FROM memory_relations WHERE source_id = ?1 AND target_id = ?2 AND relation_type = ?3",
                    params![item.id, target, supersedes],
                )?;
                tx.execute(
                    "DELETE FROM pending_relations WHERE source_id = ?1 AND target_id = ?2 AND relation_type = ?3",
                    params![item.id, target, supersedes],
                )?;
                unsuperseded.push(target);
            }
        }
    }

    for (source, target, rel) in &batch.edges {
        if source != target && !refused.contains(source.as_str()) {
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
        if rel == supersedes {
            // An imported file may supersede imported memories and durable note-kind rows,
            // but not hide a locally recorded task, blocker-in-progress or scratch memory.
            tx.execute(
                "UPDATE memories SET status = 'superseded', updated_at = CURRENT_TIMESTAMP
                 WHERE id = ?1 AND status IS NOT 'superseded'
                   AND (source IS NOT NULL
                        OR kind IN ('decision', 'invariant', 'learning', 'blocker'))",
                params![target],
            )?;
        }
        tx.execute(
            "DELETE FROM pending_relations
             WHERE source_id = ?1 AND target_id = ?2 AND relation_type = ?3",
            params![source, target, rel],
        )?;
    }

    // Prune: a fully scanned source tells us which of its rows this checkout no longer has
    // (deleted note/ADR, amended or rebased-away commit, deleted comment). Only this
    // checkout's claim goes; the row itself goes once no worktree of the repository has it,
    // so one worktree never removes what exists on another's branch.
    for &source in &batch.prune {
        let mut keep: HashSet<&str> = batch
            .items
            .iter()
            .filter(|i| i.source == source)
            .map(|i| i.id.as_str())
            .collect();
        if let Some(k) = batch.keep.get(source) {
            keep.extend(k.iter().map(String::as_str));
        }
        let claimed: Vec<String> = tx
            .prepare(
                "SELECT m.id FROM memories m JOIN memory_owners o ON o.id = m.id
                 WHERE m.source = ?1 AND o.worktree = ?2",
            )?
            .query_map(params![source, checkout.worktree], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for id in claimed.iter().filter(|id| !keep.contains(id.as_str())) {
            tx.execute(
                "DELETE FROM memory_owners WHERE id = ?1 AND worktree = ?2",
                params![id, checkout.worktree],
            )?;
        }
    }
    // Claims of worktrees git no longer knows about (removed with `git worktree remove`, or
    // deleted by hand) are dropped too, or their rows would stay forever.
    if checkout.known {
        let stale: Vec<String> = tx
            .prepare(
                "SELECT DISTINCT o.worktree FROM memory_owners o JOIN memories m ON m.id = o.id
                 WHERE m.origin_repo = ?1",
            )?
            .query_map(params![repo], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?
            .into_iter()
            .filter(|w| !checkout.worktrees.contains(w))
            .collect();
        for w in stale {
            tx.execute(
                "DELETE FROM memory_owners WHERE worktree = ?1
                   AND id IN (SELECT id FROM memories WHERE origin_repo = ?2)",
                params![w, repo],
            )?;
        }
    }
    // Rows of this repository that no checkout contains any more.
    let orphans: Vec<String> = tx
        .prepare(
            "SELECT m.id FROM memories m
             WHERE m.source IS NOT NULL AND m.origin_repo = ?1
               AND NOT EXISTS (SELECT 1 FROM memory_owners o WHERE o.id = m.id)",
        )?
        .query_map(params![repo], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for id in &orphans {
        let targets: Vec<String> = tx
            .prepare(
                "SELECT target_id FROM memory_relations WHERE source_id = ?1 AND relation_type = ?2",
            )?
            .query_map(params![id, supersedes], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        unsuperseded.extend(targets);
        tx.execute(
            "DELETE FROM pending_relations WHERE source_id = ?1",
            params![id],
        )?;
        // Relations and owners cascade; FTS and vector rows go via triggers.
        report.pruned += tx.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
    }

    // A row superseded only by an edge that is now gone becomes current again, as does an
    // imported row whose own source no longer says it's superseded.
    let resolved: HashMap<&str, bool> = applied
        .iter()
        .map(|i| (i.id.as_str(), i.resolved))
        .collect();
    let candidates = unsuperseded.iter().map(String::as_str).chain(
        applied
            .iter()
            .filter(|i| !i.superseded)
            .map(|i| i.id.as_str()),
    );
    let mut seen = HashSet::new();
    for id in candidates.filter(|id| seen.insert(*id)) {
        report.changed += tx.execute(
            "UPDATE memories
             SET status = CASE WHEN ?2 THEN 'resolved' ELSE 'active' END,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = ?1 AND status = 'superseded'
               AND NOT EXISTS (SELECT 1 FROM memory_relations
                               WHERE target_id = ?1 AND relation_type = 'supersedes')",
            params![id, resolved.get(id).copied().unwrap_or(false)],
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

/// [`refresh_if_stale`], then embed up to 50 rows that lost their vectors, logging to stderr.
/// Never fails: a broken import must not stop a search or an MCP session.
pub fn auto_refresh(store: &mut MemoryStore, repo: &Path, notes_dir: &Path) {
    match refresh_if_stale(store, repo, notes_dir) {
        Ok(Some(r)) => {
            for w in &r.warnings {
                eprintln!("[memdag] auto-import warning: {w}");
            }
            if r.changed > 0 || r.pruned > 0 {
                eprintln!(
                    "[memdag] auto-imported {} changed and {} removed memories from {}",
                    r.changed,
                    r.pruned,
                    repo.display()
                );
            }
            // Changed rows lost their vectors (trigger); refill a bounded number so hybrid
            // search keeps covering imported memories without a long blocking job.
            if r.changed > 0 && store.has_embedder() {
                match store.embed_missing_up_to(50) {
                    Ok(0) => {}
                    Ok(n) => eprintln!("[memdag] embedded {n} imported memories"),
                    Err(e) => eprintln!("[memdag] embedding after import skipped: {e:#}"),
                }
            }
        }
        Ok(None) => {}
        Err(e) => eprintln!("[memdag] auto-import failed (continuing): {e:#}"),
    }
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
