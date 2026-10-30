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

use crate::import::git;
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

/// One fingerprint per path, or `None` if any could not be computed.
pub fn fingerprints(repo: &Path, paths: &[String]) -> Option<Vec<String>> {
    paths.iter().map(|p| fingerprint(repo, p)).collect()
}

/// Paths whose current fingerprint differs from the stored one. Returns `None` when the
/// stored fingerprints can't be compared (count mismatch), so the caller can fall back.
/// A path git can't fingerprint right now is skipped rather than reported.
pub fn changed(repo: &Path, paths: &[String], stored: &[String]) -> Option<Vec<String>> {
    if paths.is_empty() || paths.len() != stored.len() {
        return None;
    }
    Some(
        paths
            .iter()
            .zip(stored)
            .filter(|(p, old)| fingerprint(repo, p).is_some_and(|now| &now != *old))
            .map(|(p, _)| p.clone())
            .collect(),
    )
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
}
