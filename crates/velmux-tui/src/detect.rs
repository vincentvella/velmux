//! Agent detection: which agent runs in a pane (from its foreground process),
//! and what it is doing (from the window title and the bottom of the screen).
//!
//! The Claude rules are a trimmed version of herdr's manifest
//! (github.com/herdrdev/herdr, src/detect/manifests/claude.toml, Apache-2.0).

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
}

const AGENTS: &[&str] = &["claude", "codex", "opencode", "gemini", "cursor-agent", "aider"];

/// The known agent running in the pty's foreground process group, if any.
///
/// The group leader is often a shell (`sh -c 'claude; …'` has no job control,
/// so the agent joins the shell's group), hence every member is checked.
pub fn foreground_agent(pgid: Option<libc::pid_t>) -> Option<&'static str> {
    let pgid = pgid?;
    let mut pids = group_members(pgid);
    pids.retain(|&p| p != pgid);
    pids.insert(0, pgid);
    pids.into_iter().find_map(|pid| {
        let path = exe_path(pid)?.to_string_lossy().to_lowercase();
        // Claude installs as `…/claude/versions/<semver>`, so match path components.
        AGENTS.iter().copied().find(|a| path.split('/').any(|part| part == *a))
    })
}

#[cfg(target_os = "macos")]
fn group_members(pgid: libc::pid_t) -> Vec<libc::pid_t> {
    let mut pids = vec![0 as libc::pid_t; 256];
    let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    // The return value's unit (pids vs bytes) is inconsistently documented, so read
    // the zero-initialised buffer instead of trusting it.
    if unsafe { libc::proc_listpgrppids(pgid, pids.as_mut_ptr().cast(), bytes) } < 0 {
        return Vec::new();
    }
    pids.retain(|&p| p > 0);
    pids
}

#[cfg(not(target_os = "macos"))]
fn group_members(pgid: libc::pid_t) -> Vec<libc::pid_t> {
    let Ok(entries) = std::fs::read_dir("/proc") else { return Vec::new() };
    entries
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<libc::pid_t>().ok())
        .filter(|pid| {
            // Field 5 of /proc/<pid>/stat is the pgrp; comm (field 2) may contain spaces.
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| s.rsplit_once(") ").map(|(_, rest)| rest.to_string()))
                .and_then(|rest| rest.split_whitespace().nth(2)?.parse::<libc::pid_t>().ok())
                == Some(pgid)
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn exe_path(pid: libc::pid_t) -> Option<PathBuf> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(PathBuf::from(String::from_utf8_lossy(&buf).into_owned()))
}

#[cfg(not(target_os = "macos"))]
fn exe_path(pid: libc::pid_t) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// Classify a Claude Code screen. `lines` are the visible rows, top to bottom.
pub fn classify_claude(title: &str, lines: &[String]) -> AgentState {
    let recent: Vec<String> = lines
        .iter()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(12)
        .map(|l| l.to_lowercase())
        .collect();
    let any = |pat: &str| recent.iter().any(|l| l.contains(pat));

    let menu = any("esc to cancel") && (any("enter to confirm") || any("enter to select"));
    let permission = (any("do you want to proceed?") || any("do you want to"))
        && recent.iter().any(|l| {
            let t = l.trim_start().trim_start_matches('❯').trim_start();
            t.starts_with("1. yes")
        });
    if menu || permission || any("waiting for permission") {
        return AgentState::Blocked;
    }
    if any("esc to interrupt") {
        return AgentState::Working;
    }
    if title
        .chars()
        .next()
        .is_some_and(|c| ('\u{2800}'..='\u{28FF}').contains(&c) || ('\u{25D0}'..='\u{25D3}').contains(&c))
    {
        return AgentState::Working;
    }
    AgentState::Idle
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(String::from).collect()
    }

    #[test]
    fn claude_states() {
        let working = lines("✻ Pondering… (12s · ↑ 1.2k tokens · esc to interrupt)\n\n❯ ");
        assert_eq!(classify_claude("", &working), AgentState::Working);

        let blocked = lines(
            "Bash command\n  rm -rf dist\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel",
        );
        assert_eq!(classify_claude("", &blocked), AgentState::Blocked);

        assert_eq!(classify_claude("✳ Claude Code", &lines("❯ \n")), AgentState::Idle);
        assert_eq!(classify_claude("⠋ Claude Code", &lines("❯ \n")), AgentState::Working);
    }
}
