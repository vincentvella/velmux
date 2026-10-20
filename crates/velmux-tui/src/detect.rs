//! Agent detection: which agent runs in a pane (from its foreground process),
//! and what it is doing (from the window title and the bottom of the screen).
//!
//! The Claude rules are a trimmed version of herdr's manifest
//! (github.com/herdrdev/herdr, src/detect/manifests/claude.toml, Apache-2.0).

use std::path::{Path, PathBuf};

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

const SHELLS: &[&str] = &["zsh", "bash", "fish", "sh", "dash", "ksh", "tcsh", "nu", "elvish", "xonsh", "login"];

pub fn is_shell(program: &str) -> bool {
    SHELLS.contains(&program.trim_start_matches('-'))
}

/// What's running in the foreground of a pane, for its label.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Foreground {
    /// Program name, preferring the command over the shell wrapping it.
    pub program: Option<String>,
    /// Its working directory.
    pub cwd: Option<PathBuf>,
}

/// The pty's foreground job: the first non-shell process in the group (the
/// command a shell is running), else the shell itself.
pub fn foreground(pgid: Option<libc::pid_t>) -> Foreground {
    let Some(pgid) = pgid else { return Foreground::default() };
    let mut pids = group_members(pgid);
    pids.retain(|&p| p != pgid);
    pids.insert(0, pgid);
    let named: Vec<(libc::pid_t, String)> = pids
        .into_iter()
        .filter_map(|pid| {
            let path = exe_path(pid)?;
            let lower = path.to_string_lossy().to_lowercase();
            // Versioned installs (`…/claude/versions/2.1.303`) take their agent's name.
            let name = AGENTS
                .iter()
                .find(|a| lower.split('/').any(|part| part == **a))
                .map(|a| a.to_string())
                .or_else(|| path.file_name().map(|f| f.to_string_lossy().into_owned()))?;
            Some((pid, name))
        })
        .collect();
    let pick = named.iter().find(|(_, n)| !is_shell(n)).or(named.first());
    match pick {
        Some((pid, name)) => Foreground { program: Some(name.clone()), cwd: process_cwd(*pid) },
        None => Foreground::default(),
    }
}

#[cfg(target_os = "macos")]
fn process_cwd(pid: libc::pid_t) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, (&mut info as *mut libc::proc_vnodepathinfo).cast(), size)
    };
    if n != size {
        return None;
    }
    let raw = &info.pvi_cdir.vip_path;
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(raw.as_ptr().cast::<u8>(), std::mem::size_of_val(raw)) };
    let end = bytes.iter().position(|&b| b == 0)?;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes[..end])))
}

#[cfg(not(target_os = "macos"))]
fn process_cwd(pid: libc::pid_t) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// `cwd` relative to `root` for a label (`None` when it's the root itself);
/// outside the root it's shown from `~`, keeping the last two components.
pub fn short_cwd(cwd: &Path, root: &Path) -> Option<String> {
    if cwd == root {
        return None;
    }
    if let Ok(rel) = cwd.strip_prefix(root) {
        return Some(rel.display().to_string());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let shown = match home.as_deref().and_then(|h| cwd.strip_prefix(h).ok()) {
        Some(rel) if rel.as_os_str().is_empty() => "~".to_string(),
        Some(rel) => format!("~/{}", rel.display()),
        None => cwd.display().to_string(),
    };
    let parts: Vec<&str> = shown.split('/').collect();
    Some(if parts.len() > 3 { format!("…/{}", parts[parts.len() - 2..].join("/")) } else { shown })
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

    #[test]
    fn labels_shells_and_paths() {
        assert!(is_shell("zsh") && is_shell("-zsh") && !is_shell("bun"));
        let root = Path::new("/w/app");
        assert_eq!(short_cwd(root, root), None);
        assert_eq!(short_cwd(Path::new("/w/app/packages/web"), root).as_deref(), Some("packages/web"));
        assert_eq!(short_cwd(Path::new("/opt/a/b/c/d"), root).as_deref(), Some("…/c/d"));
        assert_eq!(short_cwd(Path::new("/opt/x"), root).as_deref(), Some("/opt/x"));
    }

    #[test]
    fn finds_this_process_cwd_and_program() {
        // The test binary itself is a fine foreground "job".
        let me = std::process::id() as libc::pid_t;
        assert_eq!(process_cwd(me).map(|p| p.canonicalize().unwrap()), Some(std::env::current_dir().unwrap().canonicalize().unwrap()));
    }

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
