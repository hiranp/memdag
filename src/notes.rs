//! Git-tracked memory notes (`docs/memory/<id>.md`) and the repo-level `memdag.toml`.
//!
//! Note format (front-matter keys are fixed; the body's optional leading `# ` line is the title):
//!
//! ```text
//! ---
//! id: DEC-2026-10-30-short-slug
//! kind: decision | invariant | learning | blocker
//! tags: [sync, sqlite]
//! supersedes: <id or empty>
//! refs: ["#719", "ADR-0003"]
//! paths: ["src/store.rs"]   (optional: files the note is about)
//! commit: 1a2b3c4           (optional: HEAD when the note was recorded)
//! fingerprints: ["<hash>"]  (optional: content hash of each path when recorded)
//! ---
//! # Title
//!
//! body
//! ```

use crate::models::MemoryKind;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Directory notes are read from (and written to, when write-through is on) if
/// `memdag.toml` doesn't say otherwise.
pub const DEFAULT_NOTES_DIR: &str = "docs/memory";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: String,
    pub kind: MemoryKind,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub supersedes: Option<String>,
    pub refs: Vec<String>,
    /// Repo-relative files/dirs this note is about; see [`is_valid_repo_path`].
    pub paths: Vec<String>,
    /// Git commit the note was recorded at (hex), informational.
    pub commit: Option<String>,
    /// Content fingerprint per path (aligned with `paths`); the staleness baseline.
    pub fingerprints: Vec<String>,
}

impl Note {
    /// Render to the on-disk format. `parse_note(&note.render())` round-trips.
    pub fn render(&self) -> String {
        let tags = self
            .tags
            .iter()
            .map(|t| render_list_item(t, false))
            .collect::<Vec<_>>()
            .join(", ");
        let refs = self
            .refs
            .iter()
            .map(|r| render_list_item(r, true))
            .collect::<Vec<_>>()
            .join(", ");
        // Provenance keys are emitted only when set, so notes without them render exactly
        // as they did before these fields existed.
        let mut provenance = String::new();
        if !self.paths.is_empty() {
            let paths = self
                .paths
                .iter()
                .map(|p| render_list_item(p, true))
                .collect::<Vec<_>>()
                .join(", ");
            provenance.push_str(&format!("paths: [{paths}]\n"));
        }
        if let Some(c) = &self.commit {
            provenance.push_str(&format!("commit: {c}\n"));
        }
        if !self.fingerprints.is_empty() {
            let fps = self
                .fingerprints
                .iter()
                .map(|f| render_list_item(f, true))
                .collect::<Vec<_>>()
                .join(", ");
            provenance.push_str(&format!("fingerprints: [{fps}]\n"));
        }
        let mut out = format!(
            "---\nid: {}\nkind: {}\ntags: [{tags}]\nsupersedes: {}\nrefs: [{refs}]\n{provenance}---\n# {}\n",
            self.id,
            self.kind,
            self.supersedes.as_deref().unwrap_or(""),
            self.title.split_whitespace().collect::<Vec<_>>().join(" "),
        );
        let body = self.body.trim();
        if !body.is_empty() {
            out.push('\n');
            out.push_str(body);
            out.push('\n');
        }
        out
    }
}

fn render_list_item(item: &str, always_quote: bool) -> String {
    let plain = item
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'));
    if plain && !always_quote {
        item.to_string()
    } else {
        serde_json::to_string(item).unwrap_or_else(|_| format!("\"{item}\""))
    }
}

/// Parse a note file. `fallback_id` (usually the file stem) is used when `id:` is absent.
pub fn parse_note(text: &str, fallback_id: &str) -> Result<Note> {
    let (front, body) = split_front_matter(text)
        .ok_or_else(|| anyhow::anyhow!("missing `---` front-matter block"))?;

    let mut id = None;
    let mut kind = None;
    let mut tags = Vec::new();
    let mut supersedes = None;
    let mut refs = Vec::new();
    let mut paths = Vec::new();
    let mut commit = None;
    let mut fingerprints = Vec::new();

    let mut lines = front.lines().peekable();
    while let Some(line) = lines.next() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if line.starts_with([' ', '\t', '-']) {
            continue;
        }
        let key = key.trim();
        let mut value = value.trim().to_string();
        // YAML block list: `key:` followed by `  - item` lines.
        if value.is_empty() {
            let mut items = Vec::new();
            while let Some(next) = lines.peek() {
                let t = next.trim_start();
                if let Some(item) = t.strip_prefix("- ") {
                    items.push(unquote(item.trim()));
                    lines.next();
                } else {
                    break;
                }
            }
            if !items.is_empty() {
                value = format!(
                    "[{}]",
                    items
                        .iter()
                        .map(|i| serde_json::to_string(i).unwrap())
                        .collect::<Vec<_>>()
                        .join(",")
                );
            }
        }
        match key {
            "id" => id = Some(unquote(&value)).filter(|s| !s.is_empty()),
            "kind" => {
                kind = Some(MemoryKind::from_str(&unquote(&value)).map_err(|e| anyhow::anyhow!(e))?)
            }
            "tags" => tags = parse_list(&value),
            "supersedes" => supersedes = Some(unquote(&value)).filter(|s| !s.is_empty()),
            "refs" => refs = parse_list(&value),
            "paths" => paths = parse_list(&value),
            "commit" => commit = Some(unquote(&value)).filter(|s| !s.is_empty()),
            "fingerprints" => fingerprints = parse_list(&value),
            _ => {}
        }
    }

    let kind = kind.ok_or_else(|| anyhow::anyhow!("front-matter has no `kind:`"))?;
    if kind == MemoryKind::Ephemeral {
        bail!("ephemeral memories are never git-tracked");
    }
    let (title, body) = split_title(body);
    Ok(Note {
        id: id.unwrap_or_else(|| fallback_id.to_string()),
        kind,
        title,
        body,
        tags,
        supersedes,
        refs,
        paths,
        commit,
        fingerprints,
    })
}

/// Returns (front-matter, rest) if `text` starts with a `---` line closed by another.
pub(crate) fn split_front_matter(text: &str) -> Option<(&str, &str)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// Title is a leading `# ` heading if there is one, else the first non-empty line.
pub(crate) fn split_title(body: &str) -> (String, String) {
    let body = body.trim();
    let (first, rest) = body.split_once('\n').unwrap_or((body, ""));
    if let Some(h) = first.trim().strip_prefix("# ") {
        return (h.trim().to_string(), rest.trim().to_string());
    }
    let title: String = first.trim().chars().take(80).collect();
    (title, body.to_string())
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        if s.starts_with('"')
            && let Ok(v) = serde_json::from_str::<String>(s)
        {
            return v;
        }
        return s[1..s.len() - 1].to_string();
    }
    s.to_string()
}

/// Parse an inline `[a, "b", 'c']` list (or a bare comma-separated value).
fn parse_list(value: &str) -> Vec<String> {
    let v = value.trim();
    let inner = v
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(v);
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in inner.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if escaped {
                    escaped = false;
                } else if c == '\\' && q == '"' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            None if c == ',' => {
                items.push(std::mem::take(&mut cur));
            }
            None => cur.push(c),
        }
    }
    items.push(cur);
    items
        .iter()
        .map(|i| unquote(i))
        .filter(|i| !i.is_empty())
        .collect()
}

/// Split a free-form memdag tag string ("a b, c") into distinct tags, order preserved.
pub fn split_tags(tags: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tags.split(|c: char| c == ',' || c.is_whitespace()) {
        let t = t.trim();
        if !t.is_empty() && !out.iter().any(|o| o == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// Join tags into the space-separated form stored in `memories.tags` (None when empty).
pub fn join_tags(tags: &[String]) -> Option<String> {
    let mut seen: Vec<&str> = Vec::new();
    for t in tags {
        let t = t.trim();
        if !t.is_empty() && !seen.contains(&t) {
            seen.push(t);
        }
    }
    (!seen.is_empty()).then(|| seen.join(" "))
}

/// Join repo paths into the newline-separated form stored in `memories.paths`.
pub fn join_paths(paths: &[String]) -> Option<String> {
    let mut seen: Vec<&str> = Vec::new();
    for p in paths {
        let p = p.trim();
        if !p.is_empty() && !seen.contains(&p) {
            seen.push(p);
        }
    }
    (!seen.is_empty()).then(|| seen.join("\n"))
}

/// Inverse of [`join_paths`].
pub fn split_paths(stored: Option<&str>) -> Vec<String> {
    stored
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Cited paths must stay inside the repo and can't be mistaken for a git option:
/// relative, no `..` component, not starting with `-`, no NUL/newline.
pub fn is_valid_repo_path(p: &str) -> bool {
    let p = p.trim();
    !p.is_empty()
        && !p.starts_with(['/', '\\', '-'])
        && !p.contains(['\0', '\n', '\r'])
        && !p.split(['/', '\\']).any(|c| c == "..")
        && !(p.len() >= 2 && p.as_bytes()[1] == b':') // Windows drive letter
}

/// A git revision recorded as provenance must be a plain hex object name.
pub fn is_valid_commit(c: &str) -> bool {
    (7..=64).contains(&c.len()) && c.chars().all(|ch| ch.is_ascii_hexdigit())
}

/// Validate and normalise user-supplied provenance (`/` separators, no leading `./`).
pub fn clean_provenance(
    paths: &[String],
    commit: Option<&str>,
) -> Result<(Vec<String>, Option<String>)> {
    let mut out = Vec::new();
    for p in paths {
        if !is_valid_repo_path(p) {
            bail!("Invalid path '{p}': must be repo-relative, without '..' or a leading '-'");
        }
        let norm = p.trim().replace('\\', "/");
        let norm = norm.trim_start_matches("./").to_string();
        if !norm.is_empty() && !out.contains(&norm) {
            out.push(norm);
        }
    }
    let commit = match commit.map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) if is_valid_commit(c) => Some(c.to_ascii_lowercase()),
        Some(c) => bail!("Invalid commit '{c}': expected a 7-64 character hex object name"),
        None => None,
    };
    Ok((out, commit))
}

/// A memory id is used as a file name, so keep it portable (Windows included).
pub fn is_valid_note_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 120
        && !id.starts_with('.')
        && !id.to_ascii_lowercase().ends_with(RESOLVED_SUFFIX)
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `<id>.resolved.md` records that note `<id>` was resolved, so the resolution survives a
/// rebuild from git. The note itself stays append-only; the suffix is reserved for this.
pub const RESOLVED_SUFFIX: &str = ".resolved";

/// The body text `resolve_memory` appends for a resolution note (and the importer
/// reproduces from a sidecar). `None`/blank means "resolved, no note".
pub fn resolution_suffix(note: Option<&str>) -> String {
    match note.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => format!("\n\n[Resolved]: {n}"),
        None => String::new(),
    }
}

/// Render a resolution sidecar for note `id`.
pub fn render_resolution(id: &str, note: Option<&str>) -> String {
    let mut out = format!("---\nresolves: {id}\n---\n");
    if let Some(n) = note.map(str::trim).filter(|n| !n.is_empty()) {
        out.push_str(n);
        out.push('\n');
    }
    out
}

/// Parse a resolution sidecar: `(resolved id, note)`.
pub fn parse_resolution(text: &str) -> Result<(String, Option<String>)> {
    let (front, body) = split_front_matter(text)
        .ok_or_else(|| anyhow::anyhow!("missing `---` front-matter block"))?;
    let id = front
        .lines()
        .find_map(|l| l.strip_prefix("resolves:"))
        .map(unquote)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("resolution has no `resolves:` id"))?;
    let note = Some(body.trim().to_string()).filter(|b| !b.is_empty());
    Ok((id, note))
}

/// Create `path` with `bytes`, atomically and without ever replacing an existing file: write
/// a hidden temp file in the same directory, fsync it, then hard-link it into place (fails if
/// `path` exists). A crash leaves either no note or the whole note, never a truncated one;
/// the temp name is dot-prefixed with a `.tmp` extension so importers skip leftovers.
pub fn write_new_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("Failed to create {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        match std::fs::hard_link(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("{} already exists", path.display())
            }
            // Filesystems without hard links: rename is still atomic, and the exists check
            // above makes clobbering a race rather than the normal path.
            Err(_) => {
                if path.exists() {
                    bail!("{} already exists", path.display());
                }
                std::fs::rename(&tmp, path)
                    .with_context(|| format!("Failed to create {}", path.display()))
            }
        }
    })();
    std::fs::remove_file(&tmp).ok();
    result?;
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        d.sync_all().ok();
    }
    Ok(())
}

/// Lowercase, hyphenated, at most ~6 words / 40 chars.
pub fn slugify(title: &str) -> String {
    let mut slug = String::new();
    for word in title
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(6)
    {
        let word: String = word
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_lowercase();
        if word.is_empty() {
            continue;
        }
        if !slug.is_empty() {
            if slug.len() + 1 + word.len() > 40 {
                break;
            }
            slug.push('-');
        }
        slug.push_str(&word);
    }
    if slug.is_empty() {
        "note".to_string()
    } else {
        slug
    }
}

/// Settings read from `<repo>/memdag.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoConfig {
    /// `[write_through] dir = "docs/memory"` — repo-relative; enables write-through when set.
    pub write_through_dir: Option<PathBuf>,
}

impl RepoConfig {
    pub fn load(repo_root: &Path) -> Result<Self> {
        let path = repo_root.join("memdag.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        Ok(Self::parse(&raw))
    }

    /// Minimal TOML subset: `[section]` headers and `key = "value" | true | false` pairs.
    pub fn parse(raw: &str) -> Self {
        let mut section = String::new();
        let mut dir = None;
        let mut enabled = true;
        for line in raw.lines() {
            let line = strip_toml_comment(line).trim();
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name.trim().to_string();
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), unquote(value.trim()));
            if section == "write_through" {
                match key {
                    "dir" if !value.is_empty() => dir = Some(PathBuf::from(value)),
                    "enabled" => enabled = value != "false",
                    _ => {}
                }
            }
        }
        Self {
            write_through_dir: dir.filter(|_| enabled),
        }
    }

    /// Directory notes are imported from: the write-through dir, else `docs/memory`.
    pub fn notes_dir(&self) -> PathBuf {
        self.write_through_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_NOTES_DIR))
    }
}

fn strip_toml_comment(line: &str) -> &str {
    let mut in_str = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => return &line[..i],
            _ => {}
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Note {
        Note {
            id: "DEC-2026-10-30-use-wal".into(),
            kind: MemoryKind::Decision,
            title: "Use WAL".into(),
            body: "Because readers.\n\nSecond para.".into(),
            tags: vec!["sqlite".into(), "c++".into()],
            supersedes: Some("DEC-old".into()),
            refs: vec!["#719".into(), "ADR-0003".into()],
            paths: Vec::new(),
            commit: None,
            fingerprints: Vec::new(),
        }
    }

    #[test]
    fn render_parse_round_trip() {
        let n = sample();
        let text = n.render();
        assert!(text.contains("refs: [\"#719\", \"ADR-0003\"]"));
        assert!(text.contains("tags: [sqlite, \"c++\"]"));
        assert_eq!(parse_note(&text, "x").unwrap(), n);
    }

    #[test]
    fn provenance_round_trips_and_is_omitted_when_empty() {
        let plain = sample().render();
        assert!(!plain.contains("paths:") && !plain.contains("commit:"));

        let mut n = sample();
        n.paths = vec!["src/store.rs".into(), "docs/a b.md".into()];
        n.commit = Some("1a2b3c4d".into());
        n.fingerprints = vec!["a".repeat(40), "-".into()];
        let text = n.render();
        assert!(text.contains("fingerprints: ["));
        assert!(text.contains("paths: [\"src/store.rs\", \"docs/a b.md\"]"));
        assert!(text.contains("commit: 1a2b3c4d\nfingerprints: ["));
        assert!(text.contains("\"-\"]\n---\n# Use WAL"));
        assert_eq!(parse_note(&text, "x").unwrap(), n);
    }

    #[test]
    fn provenance_validation() {
        assert!(is_valid_repo_path("src/store.rs"));
        assert!(!is_valid_repo_path("../etc/passwd"));
        assert!(!is_valid_repo_path("a/../../b"));
        assert!(!is_valid_repo_path("/abs"));
        assert!(!is_valid_repo_path("--output=x"));
        assert!(!is_valid_repo_path("C:\\x"));
        assert!(is_valid_commit("1a2b3c4"));
        assert!(!is_valid_commit("HEAD"));
        assert!(!is_valid_commit("--all"));
        let (p, c) =
            clean_provenance(&["./src\\a.rs".into(), "src/a.rs".into()], Some("ABCDEF1")).unwrap();
        assert_eq!(p, vec!["src/a.rs"]);
        assert_eq!(c.as_deref(), Some("abcdef1"));
        assert!(clean_provenance(&["../x".into()], None).is_err());
        assert!(clean_provenance(&[], Some("main")).is_err());
        assert_eq!(split_paths(join_paths(&p).as_deref()), p);
    }

    #[test]
    fn parses_contract_example_and_block_lists() {
        let text = "---\nid: LRN-1\nkind: learning\ntags:\n  - a\n  - \"b c\"\nsupersedes:\nrefs: [\"#1\"]\n---\nPlain first line\nmore\n";
        let n = parse_note(text, "x").unwrap();
        assert_eq!(n.kind, MemoryKind::Learning);
        assert_eq!(n.tags, vec!["a", "b c"]);
        assert_eq!(n.supersedes, None);
        assert_eq!(n.title, "Plain first line");
        assert_eq!(n.body, "Plain first line\nmore");
    }

    #[test]
    fn missing_id_uses_fallback_and_ephemeral_is_rejected() {
        let n = parse_note("---\nkind: invariant\n---\n# T\n", "INV-file").unwrap();
        assert_eq!(n.id, "INV-file");
        assert!(parse_note("---\nkind: ephemeral\n---\nx", "e").is_err());
        assert!(parse_note("no front matter", "e").is_err());
    }

    #[test]
    fn config_parse() {
        let c = RepoConfig::parse("# c\n[write_through]\ndir = \"docs/memory\" # trailing\n");
        assert_eq!(c.write_through_dir, Some(PathBuf::from("docs/memory")));
        let off = RepoConfig::parse("[write_through]\ndir = \"x\"\nenabled = false\n");
        assert_eq!(off.write_through_dir, None);
        assert_eq!(
            RepoConfig::parse("").notes_dir(),
            PathBuf::from("docs/memory")
        );
    }

    #[test]
    fn slug_and_ids() {
        assert_eq!(
            slugify("Use SQLite WAL, not rollback-journal!"),
            "use-sqlite-wal-not-rollback-journal"
        );
        assert!(is_valid_note_id("DEC-2026-10-30-x"));
        assert!(!is_valid_note_id("../etc"));
        assert!(!is_valid_note_id("a/b"));
    }
}
