use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Run `git -C dir args…` and return trimmed stdout.
pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Like [`git`] but only reports success.
pub fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The base checkout for any path inside it or inside one of its worktrees.
pub fn base_of(path: &Path) -> Result<PathBuf> {
    let common = git(path, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .with_context(|| format!("{} is not inside a git repository", path.display()))?;
    let common = PathBuf::from(common);
    let base = if common.file_name().is_some_and(|n| n == ".git") {
        common.parent().unwrap().to_path_buf()
    } else {
        bail!("bare repositories are not supported ({})", common.display());
    };
    Ok(base.canonicalize()?)
}

/// `origin/HEAD` when a remote exists, else the base checkout's current branch.
pub fn default_base_ref(base: &Path) -> String {
    if let Ok(r) = git(base, &["rev-parse", "--abbrev-ref", "origin/HEAD"]) {
        if !r.is_empty() && r != "origin/HEAD" {
            return r;
        }
    }
    git(base, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_else(|_| "HEAD".into())
}

pub fn has_remote(dir: &Path) -> bool {
    git(dir, &["remote"]).map(|r| !r.is_empty()).unwrap_or(false)
}

pub fn current_branch(dir: &Path) -> Option<String> {
    git(dir, &["branch", "--show-current"]).ok().filter(|b| !b.is_empty())
}

pub fn is_dirty(dir: &Path) -> Result<bool> {
    Ok(!git(dir, &["status", "--porcelain"])?.is_empty())
}

pub fn local_branch_exists(dir: &Path, branch: &str) -> bool {
    git_ok(dir, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")])
}

pub fn remote_branch_exists(dir: &Path, branch: &str) -> bool {
    git_ok(dir, &["show-ref", "--verify", "--quiet", &format!("refs/remotes/origin/{branch}")])
}

/// Every stash whose message contains `marker`, newest first: (`stash@{n}`, unix time).
pub fn stashes_with(dir: &Path, marker: &str) -> Vec<(String, u64)> {
    let Ok(list) = git(dir, &["stash", "list", "--format=%gd%x00%ct%x00%s"]) else { return Vec::new() };
    list.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\0');
            let (refname, time, subject) = (parts.next()?, parts.next()?, parts.next()?);
            subject.contains(marker).then(|| (refname.to_string(), time.parse().unwrap_or(0)))
        })
        .collect()
}

/// How many files a stash touches, untracked ones included.
pub fn stash_files(dir: &Path, stash: &str) -> usize {
    let tracked = git(dir, &["stash", "show", "--name-only", stash]).unwrap_or_default();
    let untracked = git(dir, &["show", "--name-only", "--format=", &format!("{stash}^3")]).unwrap_or_default();
    tracked.lines().chain(untracked.lines()).filter(|l| !l.is_empty()).count()
}

/// (behind, ahead): commits on `base` that `HEAD` lacks, and the reverse.
pub fn behind_ahead(dir: &Path, base: &str) -> (usize, usize) {
    let Ok(out) = git(dir, &["rev-list", "--left-right", "--count", &format!("{base}...HEAD")]) else { return (0, 0) };
    let mut n = out.split_whitespace().map(|x| x.parse().unwrap_or(0));
    (n.next().unwrap_or(0), n.next().unwrap_or(0))
}

/// A rebase or merge stopped part-way in this worktree, if any.
pub fn conflict(dir: &Path) -> Option<&'static str> {
    let exists = |p: &str| git(dir, &["rev-parse", "--git-path", p]).ok().is_some_and(|path| dir.join(path).exists());
    if exists("rebase-merge") || exists("rebase-apply") {
        Some("rebase")
    } else if exists("MERGE_HEAD") {
        Some("merge")
    } else {
        None
    }
}

/// Commits on `branch` that aren't on `base`.
pub fn ahead(dir: &Path, base: &str, branch: &str) -> usize {
    git(dir, &["rev-list", "--count", &format!("{base}..{branch}")]).ok().and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// Index (`stash@{n}`) of the newest stash whose message contains `marker`.
pub fn find_stash(dir: &Path, marker: &str) -> Option<String> {
    let list = git(dir, &["stash", "list", "--format=%gd%x00%s"]).ok()?;
    list.lines().find_map(|line| {
        let (refname, subject) = line.split_once('\0')?;
        subject.contains(marker).then(|| refname.to_string())
    })
}
