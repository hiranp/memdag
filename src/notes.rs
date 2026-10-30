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
        let mut out = format!(
            "---\nid: {}\nkind: {}\ntags: [{tags}]\nsupersedes: {}\nrefs: [{refs}]\n---\n# {}\n",
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

/// A memory id is used as a file name, so keep it portable (Windows included).
pub fn is_valid_note_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 120
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
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
