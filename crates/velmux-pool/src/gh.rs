//! Pull requests for a repo's branches, through the GitHub CLI (`gh`).

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::git;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

/// The PR's checks, rolled up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Checks {
    None,
    Pending,
    Passing,
    Failing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pr {
    pub number: u64,
    pub url: String,
    pub state: PrState,
    pub draft: bool,
    pub checks: Checks,
    /// Names of the checks that failed.
    pub failed: Vec<String>,
}

/// `gh` is installed.
pub fn available() -> bool {
    static FOUND: OnceLock<bool> = OnceLock::new();
    *FOUND.get_or_init(|| {
        Command::new("gh").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    })
}

/// The repo's `origin` is on GitHub.
pub fn on_github(dir: &Path) -> bool {
    git::git(dir, &["remote", "get-url", "origin"]).is_ok_and(|url| url.contains("github.com"))
}

/// The newest PR for each head branch (open, merged or closed).
pub fn prs(dir: &Path) -> Result<HashMap<String, Pr>> {
    let out = Command::new("gh")
        .args(["pr", "list", "--state", "all", "--limit", "100", "--json", "number,headRefName,url,state,isDraft,statusCheckRollup"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .context("running gh")?;
    if !out.status.success() {
        bail!("gh pr list: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    parse(&String::from_utf8_lossy(&out.stdout))
}

fn parse(json: &str) -> Result<HashMap<String, Pr>> {
    let list: Vec<serde_json::Value> = serde_json::from_str(json).context("parsing gh output")?;
    let mut by_branch = HashMap::new();
    // Newest first, so the first PR seen for a branch is the current one.
    for v in list {
        let Some(branch) = v["headRefName"].as_str() else { continue };
        if by_branch.contains_key(branch) {
            continue;
        }
        let state = match v["state"].as_str() {
            Some("MERGED") => PrState::Merged,
            Some("CLOSED") => PrState::Closed,
            _ => PrState::Open,
        };
        let (checks, failed) = rollup(v["statusCheckRollup"].as_array().map(Vec::as_slice).unwrap_or(&[]));
        by_branch.insert(
            branch.to_string(),
            Pr {
                number: v["number"].as_u64().unwrap_or(0),
                url: v["url"].as_str().unwrap_or_default().to_string(),
                state,
                draft: v["isDraft"].as_bool().unwrap_or(false),
                checks,
                failed,
            },
        );
    }
    Ok(by_branch)
}

/// Check runs and commit statuses → one verdict: any failure fails, else
/// anything unfinished is pending.
fn rollup(items: &[serde_json::Value]) -> (Checks, Vec<String>) {
    let (mut pending, mut passed, mut failed) = (false, false, Vec::new());
    for c in items {
        let name = c["name"].as_str().or(c["context"].as_str()).unwrap_or("check").to_string();
        let verdict = match c["__typename"].as_str() {
            Some("StatusContext") => c["state"].as_str().unwrap_or(""),
            _ if c["status"].as_str().is_some_and(|s| s != "COMPLETED") => "PENDING",
            _ => c["conclusion"].as_str().unwrap_or(""),
        };
        match verdict {
            "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => failed.push(name),
            "PENDING" | "EXPECTED" | "QUEUED" | "IN_PROGRESS" | "" => pending = true,
            _ => passed = true,
        }
    }
    let checks = if !failed.is_empty() {
        Checks::Failing
    } else if pending {
        Checks::Pending
    } else if passed {
        Checks::Passing
    } else {
        Checks::None
    };
    (checks, failed)
}

/// Push `branch` (setting its upstream) and open GitHub's new-PR page for it.
pub fn push_and_open(dir: &Path, branch: &str) -> Result<()> {
    git::git(dir, &["push", "--quiet", "-u", "origin", branch])?;
    let out = Command::new("gh").args(["pr", "create", "--web", "--head", branch]).current_dir(dir).stdin(Stdio::null()).output().context("running gh")?;
    if !out.status.success() {
        bail!("gh pr create: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolls_up_checks_newest_pr_per_branch() {
        let json = r#"[
          {"headRefName":"feat/a","number":7,"url":"u7","state":"OPEN","isDraft":true,"statusCheckRollup":[
            {"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":"SUCCESS"},
            {"__typename":"CheckRun","name":"lint","status":"IN_PROGRESS","conclusion":""}]},
          {"headRefName":"feat/b","number":6,"url":"u6","state":"OPEN","isDraft":false,"statusCheckRollup":[
            {"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":"FAILURE"},
            {"__typename":"StatusContext","context":"deploy","state":"SUCCESS"}]},
          {"headRefName":"feat/c","number":5,"url":"u5","state":"MERGED","isDraft":false,"statusCheckRollup":[
            {"__typename":"StatusContext","context":"ci","state":"SUCCESS"}]},
          {"headRefName":"feat/a","number":3,"url":"u3","state":"CLOSED","isDraft":false,"statusCheckRollup":[]}
        ]"#;
        let prs = parse(json).unwrap();
        let a = &prs["feat/a"];
        assert_eq!((a.number, a.draft, a.checks), (7, true, Checks::Pending), "newest PR wins");
        let b = &prs["feat/b"];
        assert_eq!((b.checks, b.failed.clone()), (Checks::Failing, vec!["ci".to_string()]));
        assert_eq!((prs["feat/c"].state, prs["feat/c"].checks), (PrState::Merged, Checks::Passing));
        assert_eq!(rollup(&[]).0, Checks::None);
    }
}
