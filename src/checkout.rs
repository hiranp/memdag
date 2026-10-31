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
/// repository is bare) and each linked worktree git still knows about. None when git's
/// worktree list can't be read: callers must then not treat any checkout as removed.
pub fn worktree_keys(root: &Path) -> Option<Vec<String>> {
    let common = common_dir(root)?;
    let mut keys = Vec::new();
    if common.file_name().is_some_and(|n| n == ".git")
        && let Some(main) = common.parent()
    {
        keys.push(worktree_key(main));
    }
    match std::fs::read_dir(common.join("worktrees")) {
        Ok(entries) => {
            for e in entries {
                let dir = e.ok()?.path();
                // `<common>/worktrees/<name>/gitdir` holds the path of the worktree's `.git`
                // file; git >= 2.48 can write it relative to this directory.
                let Ok(text) = std::fs::read_to_string(dir.join("gitdir")) else {
                    continue;
                };
                let target = Path::new(text.trim());
                let target = if target.is_absolute() {
                    target.to_path_buf()
                } else {
                    dir.join(target)
                };
                let Some(wt) = target.parent() else { continue };
                // A locked worktree (e.g. on removable media) counts even while it's absent.
                if wt.exists() || dir.join("locked").exists() {
                    keys.push(worktree_key(wt));
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    let here = worktree_key(root);
    if !keys.contains(&here) {
        keys.push(here);
    }
    Some(keys)
}

fn is_bare(common: &Path) -> bool {
    std::fs::read_to_string(common.join("config")).is_ok_and(|c| {
        c.lines()
            .map(|l| l.split_whitespace().collect::<String>())
            .any(|l| l.eq_ignore_ascii_case("bare=true"))
    })
}

/// The database a linked worktree shares with the rest of its repository:
/// `<main checkout>/.memdag/memdag.db` (where the main checkout's database always was), or
/// `<repo>.git/memdag/memdag.db` for a bare repository. None for anything that isn't a linked
/// worktree, which keeps its own `<root>/.memdag/memdag.db` as before. That includes
/// submodules and `--separate-git-dir` checkouts: their `.git` file points at a git dir that
/// is not shared with other checkouts.
pub fn shared_db_path(root: &Path) -> Option<PathBuf> {
    if !is_linked_worktree(root) {
        return None;
    }
    let common = common_dir(root)?;
    if common.file_name().is_some_and(|n| n == ".git") {
        return Some(common.parent()?.join(".memdag").join("memdag.db"));
    }
    is_bare(&common).then(|| common.join("memdag").join("memdag.db"))
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
        assert!(
            shared_db_path(&main).is_none(),
            "the main checkout keeps its own path"
        );

        let mut keys = worktree_keys(&wt).unwrap();
        keys.sort();
        let mut want = vec![worktree_key(&main), worktree_key(&wt)];
        want.sort();
        assert_eq!(keys, want);

        git(&main, &["worktree", "remove", "../wt"]);
        assert_eq!(worktree_keys(&main).unwrap(), vec![worktree_key(&main)]);
    }

    #[test]
    fn a_relative_gitdir_and_a_locked_missing_worktree_still_count() {
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
        // What `worktree.useRelativePaths` writes: relative to <common>/worktrees/wt.
        let meta = main.join(".git/worktrees/wt");
        std::fs::write(meta.join("gitdir"), "../../../../wt/.git\n").unwrap();
        // Run from somewhere unrelated, so a cwd-relative resolution would miss it.
        let keys = worktree_keys(&main).unwrap();
        assert!(keys.contains(&worktree_key(&wt)), "{keys:?}");

        std::fs::write(meta.join("locked"), "on a usb stick").unwrap();
        std::fs::rename(&wt, tmp.path().join("elsewhere")).unwrap();
        let keys = worktree_keys(&main).unwrap();
        assert_eq!(keys.len(), 2, "locked worktree kept: {keys:?}");
    }

    #[test]
    fn a_submodule_keeps_a_database_of_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let sub_src = tmp.path().join("sub-src");
        let sup = tmp.path().join("super");
        for d in [&sub_src, &sup] {
            std::fs::create_dir(d).unwrap();
            git(d, &["init", "-q"]);
            git(
                d,
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
        }
        git(
            &sup,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub_src.to_str().unwrap(),
                "sub",
            ],
        );
        let sub = sup.join("sub");
        assert!(sub.join(".git").is_file(), "submodule has a .git file");
        assert!(!is_linked_worktree(&sub));
        assert!(shared_db_path(&sub).is_none());
        assert_ne!(repository_key(&sub), repository_key(&sup));
    }

    #[test]
    fn a_plain_directory_is_its_own_repository() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(repository_key(tmp.path()), worktree_key(tmp.path()));
        assert!(shared_db_path(tmp.path()).is_none());
    }
}
