//! Which repository and which worktree a checkout is.
//!
//! All linked worktrees of a repository share one git "common dir" (`<main>/.git`, or the
//! bare repository). memdag uses it to give every worktree of a repository one shared
//! database, so agents in different worktrees see the same tasks, blockers and links, while
//! each imported row records which worktrees' checkouts contain its source.
//!
//! Everything here reads git's own files (`.git`, `gitdir`, `commondir`) instead of starting
//! `git`, because it runs on every process start.

use std::path::{Path, PathBuf};

fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// The git dir of the checkout rooted at `root`: `root/.git` itself, or what a `.git` file
/// (`gitdir: <path>`, written for linked worktrees and submodules) points to.
fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(canonical(&dot_git));
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let target = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    let target = Path::new(target);
    let abs = if target.is_absolute() {
        target.to_path_buf()
    } else {
        root.join(target)
    };
    Some(canonical(&abs))
}

/// The repository's common dir, shared by all of its worktrees.
pub fn common_dir(root: &Path) -> Option<PathBuf> {
    let gd = git_dir(root)?;
    match std::fs::read_to_string(gd.join("commondir")) {
        Ok(rel) => {
            let rel = Path::new(rel.trim());
            let abs = if rel.is_absolute() {
                rel.to_path_buf()
            } else {
                gd.join(rel)
            };
            Some(canonical(&abs))
        }
        // The main worktree, or a submodule: its git dir is the common dir.
        Err(_) => Some(gd),
    }
}

/// Whether `root` is a linked worktree (its `.git` is a file pointing into another
/// repository's `worktrees/` directory) rather than the main checkout.
pub fn is_linked_worktree(root: &Path) -> bool {
    root.join(".git").is_file()
        && git_dir(root)
            .zip(common_dir(root))
            .is_some_and(|(gd, cd)| gd != cd)
}

/// Key for the repository (shared by its worktrees): the canonical common dir. Falls back to
/// the checkout path outside git.
pub fn repository_key(root: &Path) -> String {
    common_dir(root)
        .unwrap_or_else(|| canonical(root))
        .display()
        .to_string()
}

/// Key for one checkout (worktree): its canonical root path.
pub fn worktree_key(root: &Path) -> String {
    canonical(root).display().to_string()
}

/// Keys of every checkout of the repository `root` belongs to: the main worktree (unless the
/// repository is bare) and each linked worktree git still knows about.
pub fn worktree_keys(root: &Path) -> Vec<String> {
    let Some(common) = common_dir(root) else {
        return vec![worktree_key(root)];
    };
    let mut keys = Vec::new();
    if common.file_name().is_some_and(|n| n == ".git")
        && let Some(main) = common.parent()
    {
        keys.push(worktree_key(main));
    }
    if let Ok(entries) = std::fs::read_dir(common.join("worktrees")) {
        for e in entries.flatten() {
            // `<common>/worktrees/<name>/gitdir` holds the path of the worktree's `.git` file.
            if let Ok(text) = std::fs::read_to_string(e.path().join("gitdir"))
                && let Some(root) = Path::new(text.trim()).parent()
                && root.exists()
            {
                keys.push(worktree_key(root));
            }
        }
    }
    let here = worktree_key(root);
    if !keys.contains(&here) {
        keys.push(here);
    }
    keys
}

/// Where a repository's shared database lives: `<main checkout>/.memdag/memdag.db` (where
/// memdag always kept it, so existing main-checkout databases stay in place), or
/// `<common dir>/memdag/memdag.db` for a bare repository.
pub fn shared_db_path(root: &Path) -> Option<PathBuf> {
    let common = common_dir(root)?;
    if common.file_name().is_some_and(|n| n == ".git")
        && let Some(main) = common.parent()
    {
        return Some(main.join(".memdag").join("memdag.db"));
    }
    Some(common.join("memdag").join("memdag.db"))
}

#[cfg(test)]
mod tests {
    use super::*;
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
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn worktrees_share_a_repository_and_a_database() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q"]);
        git(
            &main,
            &[
                "-c",
                "user.email=t@e",
                "-c",
                "user.name=T",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "i",
            ],
        );
        git(&main, &["worktree", "add", "-q", "../wt", "-b", "b"]);
        let wt = tmp.path().join("wt");

        assert_eq!(repository_key(&main), repository_key(&wt));
        assert_ne!(worktree_key(&main), worktree_key(&wt));
        assert!(is_linked_worktree(&wt) && !is_linked_worktree(&main));
        let shared = canonical(&main).join(".memdag/memdag.db");
        assert_eq!(shared_db_path(&wt).unwrap(), shared);
        assert_eq!(shared_db_path(&main).unwrap(), shared);

        let mut keys = worktree_keys(&wt);
        keys.sort();
        let mut want = vec![worktree_key(&main), worktree_key(&wt)];
        want.sort();
        assert_eq!(keys, want);

        git(&main, &["worktree", "remove", "../wt"]);
        assert_eq!(worktree_keys(&main), vec![worktree_key(&main)]);
    }

    #[test]
    fn a_plain_directory_is_its_own_repository() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(repository_key(tmp.path()), worktree_key(tmp.path()));
        assert!(shared_db_path(tmp.path()).is_none());
    }
}
