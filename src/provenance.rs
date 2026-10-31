//! Content fingerprints for cited paths.
//!
//! Staleness is measured against *what the files contained when the memory was recorded*,
//! not against a commit. A commit baseline misfires in the most common workflow: edit
//! `x.rs`, then record a decision about it. `git diff <HEAD> -- x.rs` is already non-empty
//! (the edit is uncommitted) and stays non-empty after the edit is committed, so the memory
//! reads STALE from birth. A content hash has no such problem, and it also survives
//! rebases, amends, branch switches and shallow clones.
//!
//! A fingerprint is the git blob hash of a file's working-tree content (`git hash-object`),
//! or for a directory a hash over the (path, blob hash) pairs of its tracked files. A path
//! that doesn't exist fingerprints as [`MISSING`].

use crate::import::{git, note_git_spawn};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Fingerprint of a path that doesn't exist (yet, or any more).
pub const MISSING: &str = "-";

/// Hex object name (SHA-1 or SHA-256 repositories) or [`MISSING`]. Used to validate
/// fingerprints read from note files, which come from the repo and are untrusted.
pub fn is_valid_fingerprint(f: &str) -> bool {
    f == MISSING || ((40..=64).contains(&f.len()) && f.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Run git with `input` on stdin; stdout on success.
fn git_with_stdin(repo: &Path, args: &[&str], input: &[u8]) -> Option<String> {
    note_git_spawn();
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let data = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&data);
    });
    let out = child.wait_with_output().ok()?;
    let _ = writer.join();
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn blob_hash(repo: &Path, rel: &str) -> Option<String> {
    let out = git(repo, &["hash-object", "--", rel]).ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|h| is_valid_fingerprint(h))
}

fn dir_hash(repo: &Path, rel: &str) -> Option<String> {
    let listed = git(repo, &["--literal-pathspecs", "ls-files", "-z", "--", rel]).ok()?;
    if !listed.status.success() {
        return None;
    }
    let mut files: Vec<String> = String::from_utf8_lossy(&listed.stdout)
        .split('\0')
        .filter(|f| !f.is_empty() && !f.contains('\n') && repo.join(f).is_file())
        .map(str::to_string)
        .collect();
    files.sort();

    let hashes = if files.is_empty() {
        String::new()
    } else {
        git_with_stdin(
            repo,
            &["hash-object", "--stdin-paths"],
            files.join("\n").as_bytes(),
        )?
    };
    let hashes: Vec<&str> = hashes.lines().collect();
    if hashes.len() != files.len() {
        return None;
    }
    let combined: String = files
        .iter()
        .zip(hashes)
        .map(|(f, h)| format!("{f}\t{h}\n"))
        .collect();
    git_with_stdin(repo, &["hash-object", "--stdin"], combined.as_bytes())
        .map(|h| h.trim().to_string())
        .filter(|h| is_valid_fingerprint(h))
}

/// Fingerprint of one repo-relative path, or `None` if git couldn't compute it (in which
/// case the caller should treat the result as "unknown", never as "changed").
pub fn fingerprint(repo: &Path, rel: &str) -> Option<String> {
    let full = repo.join(rel);
    if !full.exists() {
        return Some(MISSING.to_string());
    }
    if full.is_dir() {
        dir_hash(repo, rel)
    } else {
        blob_hash(repo, rel)
    }
}

/// Pathspec bytes per git invocation, well under Windows' 32767-character command line.
const ARG_BUDGET: usize = 16 * 1024;

/// Split `items` into runs whose total length fits in one command line.
pub(crate) fn arg_chunks<S: AsRef<str>>(items: &[S]) -> Vec<&[S]> {
    let mut chunks = Vec::new();
    let (mut start, mut len) = (0, 0);
    for (i, item) in items.iter().enumerate() {
        let l = item.as_ref().len() + 1;
        if i > start && len + l > ARG_BUDGET {
            chunks.push(&items[start..i]);
            (start, len) = (i, 0);
        }
        len += l;
    }
    if start < items.len() {
        chunks.push(&items[start..]);
    }
    chunks
}

/// A plain `a/b/c` path (optionally with one trailing `/`): a literal pathspec for it matches
/// exactly the paths equal to it or below it, and git reads it the same on a
/// `--stdin-paths` line as on the command line. Anything else is fingerprinted on its own.
pub(crate) fn is_plain_path(p: &str) -> bool {
    let base = p.strip_suffix('/').unwrap_or(p);
    !base.is_empty()
        && !p.contains(['\\', '\n', '\r', '\0'])
        && !p.starts_with('"')
        && base.split('/').all(|c| !c.is_empty() && c != ".")
}

/// Whether the literal pathspec `spec` (a plain path, see [`is_plain_path`]) matches `path`.
pub(crate) fn pathspec_covers(spec: &str, path: &str) -> bool {
    let (base, dir_only) = match spec.strip_suffix('/') {
        Some(b) => (b, true),
        None => (spec, false),
    };
    (!dir_only && path == base) || path.strip_prefix(base).is_some_and(|r| r.starts_with('/'))
}

/// Blob hashes of working-tree files, in one `git hash-object --stdin-paths`.
fn hash_paths(repo: &Path, paths: &[&str]) -> Option<Vec<String>> {
    if paths.is_empty() {
        return Some(Vec::new());
    }
    let mut input = paths.join("\n");
    input.push('\n');
    let out = git_with_stdin(repo, &["hash-object", "--stdin-paths"], input.as_bytes())?;
    let hashes: Vec<String> = out.lines().map(|h| h.trim().to_string()).collect();
    (hashes.len() == paths.len()).then_some(hashes)
}

/// Removes a scratch directory when dropped.
struct ScratchDir(std::path::PathBuf);

impl ScratchDir {
    fn new() -> Option<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        for _ in 0..8 {
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            let dir =
                std::env::temp_dir().join(format!("memdag-fp-{}-{nanos}-{n}", std::process::id()));
            if std::fs::create_dir(&dir).is_ok() {
                return Some(Self(dir));
            }
        }
        None
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Blob hashes of in-memory contents, unfiltered like `git hash-object --stdin`. Several
/// contents go through scratch files and one `git hash-object --no-filters --stdin-paths`.
fn hash_contents(repo: &Path, contents: &[String]) -> Option<Vec<String>> {
    let hash_one = |c: &String| {
        git_with_stdin(repo, &["hash-object", "--stdin"], c.as_bytes())
            .map(|h| h.trim().to_string())
    };
    if contents.len() <= 1 {
        return contents.iter().map(hash_one).collect();
    }
    let batched = (|| {
        let scratch = ScratchDir::new()?;
        let mut input = String::new();
        for (i, c) in contents.iter().enumerate() {
            let file = scratch.0.join(i.to_string());
            std::fs::write(&file, c).ok()?;
            let name = file.to_str().filter(|n| is_stdin_path_safe(n))?;
            input.push_str(name);
            input.push('\n');
        }
        let out = git_with_stdin(
            repo,
            &["hash-object", "--no-filters", "--stdin-paths"],
            input.as_bytes(),
        )?;
        let hashes: Vec<String> = out.lines().map(|h| h.trim().to_string()).collect();
        (hashes.len() == contents.len()).then_some(hashes)
    })();
    batched.or_else(|| contents.iter().map(hash_one).collect())
}

/// A name git reads back unchanged from a `--stdin-paths` line.
fn is_stdin_path_safe(p: &str) -> bool {
    !p.is_empty() && !p.starts_with('"') && !p.contains(['\n', '\r', '\0'])
}

/// Tracked files under each directory, exactly as `git ls-files -- <dir>` would list them
/// for that directory alone, from as few `ls-files` as the command-line budget allows.
fn dir_members(repo: &Path, dirs: &[&str]) -> Option<Vec<Vec<String>>> {
    let mut members = vec![Vec::new(); dirs.len()];
    let mut offset = 0;
    for chunk in arg_chunks(dirs) {
        let mut args = vec!["--literal-pathspecs", "ls-files", "-z", "--"];
        args.extend_from_slice(chunk);
        let listed = git(repo, &args).ok()?;
        if !listed.status.success() {
            return None;
        }
        for f in String::from_utf8_lossy(&listed.stdout)
            .split('\0')
            .filter(|f| !f.is_empty())
        {
            for (i, d) in chunk.iter().enumerate() {
                if pathspec_covers(d, f) {
                    members[offset + i].push(f.to_string());
                }
            }
        }
        offset += chunk.len();
    }
    // The same filter as `dir_hash`, with one stat per distinct file.
    let mut is_file: HashMap<String, bool> = HashMap::new();
    for files in &mut members {
        files.retain(|f| {
            !f.contains('\n')
                && *is_file
                    .entry(f.clone())
                    .or_insert_with(|| repo.join(f).is_file())
        });
        files.sort();
    }
    Some(members)
}

/// Fingerprints of many repo-relative paths, the same as [`fingerprint`] gives one at a time
/// but with a constant number of git processes: one `ls-files` for all directories, one
/// `hash-object` for every file and directory member, one `hash-object` for the directory
/// digests. (Paths that aren't plain, and failed batches, fall back to one path at a time.)
/// A path missing from the map couldn't be fingerprinted: treat it as unknown.
pub fn fingerprints_many<S: AsRef<str>>(repo: &Path, paths: &[S]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut seen = HashSet::new();
    let (mut files, mut dirs) = (Vec::new(), Vec::new());
    for p in paths.iter().map(AsRef::as_ref) {
        if !seen.insert(p) {
            continue;
        }
        let full = repo.join(p);
        if !full.exists() {
            out.insert(p.to_string(), MISSING.to_string());
        } else if !is_plain_path(p) {
            if let Some(f) = fingerprint(repo, p) {
                out.insert(p.to_string(), f);
            }
        } else if full.is_dir() {
            dirs.push(p);
        } else {
            files.push(p);
        }
    }
    let one_at_a_time = |out: &mut HashMap<String, String>, files: &[&str], dirs: &[&str]| {
        for f in files {
            if let Some(h) = blob_hash(repo, f) {
                out.insert(f.to_string(), h);
            }
        }
        for d in dirs {
            if let Some(h) = dir_hash(repo, d) {
                out.insert(d.to_string(), h);
            }
        }
    };

    let Some(members) = dir_members(repo, &dirs) else {
        one_at_a_time(&mut out, &files, &dirs);
        return out;
    };
    let mut to_hash: Vec<&str> = Vec::new();
    let mut queued = HashSet::new();
    for p in files
        .iter()
        .copied()
        .chain(members.iter().flatten().map(String::as_str))
    {
        if queued.insert(p) {
            to_hash.push(p);
        }
    }
    let Some(hashes) = hash_paths(repo, &to_hash) else {
        one_at_a_time(&mut out, &files, &dirs);
        return out;
    };
    let blob: HashMap<&str, &str> = to_hash
        .iter()
        .copied()
        .zip(hashes.iter().map(String::as_str))
        .collect();
    for f in &files {
        if let Some(h) = blob.get(f).filter(|h| is_valid_fingerprint(h)) {
            out.insert(f.to_string(), h.to_string());
        }
    }
    let digests: Vec<String> = members
        .iter()
        .map(|files| {
            files
                .iter()
                .map(|f| format!("{f}\t{}\n", blob[f.as_str()]))
                .collect()
        })
        .collect();
    if let Some(hashes) = hash_contents(repo, &digests) {
        for (d, h) in dirs.iter().zip(hashes) {
            if is_valid_fingerprint(&h) {
                out.insert(d.to_string(), h);
            }
        }
    }
    out
}

/// One fingerprint per path, or `None` if any could not be computed.
pub fn fingerprints(repo: &Path, paths: &[String]) -> Option<Vec<String>> {
    let now = fingerprints_many(repo, paths);
    paths.iter().map(|p| now.get(p).cloned()).collect()
}

/// Paths whose current fingerprint differs from the stored one. Returns `None` when the
/// stored fingerprints can't be compared (count mismatch), so the caller can fall back.
/// A path git can't fingerprint right now is skipped rather than reported.
pub fn changed(repo: &Path, paths: &[String], stored: &[String]) -> Option<Vec<String>> {
    if !comparable(paths, stored) {
        return None;
    }
    Some(changed_since(
        paths,
        stored,
        &fingerprints_many(repo, paths),
    ))
}

/// Whether stored fingerprints line up with the cited paths.
pub fn comparable(paths: &[String], stored: &[String]) -> bool {
    !paths.is_empty() && paths.len() == stored.len()
}

/// [`changed`] against fingerprints already computed with [`fingerprints_many`].
pub fn changed_since(
    paths: &[String],
    stored: &[String],
    current: &HashMap<String, String>,
) -> Vec<String> {
    paths
        .iter()
        .zip(stored)
        .filter(|(p, old)| current.get(*p).is_some_and(|now| now != *old))
        .map(|(p, _)| p.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_validation() {
        assert!(is_valid_fingerprint(MISSING));
        assert!(is_valid_fingerprint(&"a".repeat(40)));
        assert!(is_valid_fingerprint(&"F".repeat(64)));
        assert!(!is_valid_fingerprint("HEAD"));
        assert!(!is_valid_fingerprint(&"a".repeat(39)));
        assert!(!is_valid_fingerprint("--output=x"));
    }

    #[test]
    fn pathspec_matching_for_batched_output() {
        assert!(pathspec_covers("src", "src"));
        assert!(pathspec_covers("src", "src/a.rs"));
        assert!(pathspec_covers("src/", "src/a/b.rs"));
        assert!(!pathspec_covers("src/", "src"));
        assert!(!pathspec_covers("src", "src-x/a.rs"));
        assert!(!pathspec_covers("src", "srca"));
        assert!(is_plain_path("src/a.rs") && is_plain_path("src/"));
        for odd in ["./a", "a//b", "a/./b", "a\\b", "\"a", "/", ""] {
            assert!(!is_plain_path(odd), "{odd}");
        }
    }

    #[test]
    fn arg_chunks_cover_everything_within_budget() {
        let items: Vec<String> = (0..2000).map(|i| format!("dir/{i:0>20}")).collect();
        let chunks = arg_chunks(&items);
        assert!(chunks.len() > 1);
        assert_eq!(chunks.iter().map(|c| c.len()).sum::<usize>(), items.len());
        assert!(
            chunks
                .iter()
                .all(|c| c.iter().map(|s| s.len() + 1).sum::<usize>() <= ARG_BUDGET)
        );
        assert!(arg_chunks::<String>(&[]).is_empty());
    }
}
