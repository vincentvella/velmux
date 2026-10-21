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
    pids.into_iter().find_map(agent_of)
}

/// The agent process `pid` is, if any: by its executable's path (Claude installs
/// as `…/claude/versions/<semver>`, so path components are matched), or, for
/// agents that run under Node (npm installs of Codex and Gemini), by the
/// script it runs.
fn agent_of(pid: libc::pid_t) -> Option<&'static str> {
    let path = exe_path(pid)?.to_string_lossy().to_lowercase();
    if let Some(a) = AGENTS.iter().copied().find(|a| path.split('/').any(|part| part == *a)) {
        return Some(a);
    }
    let runtime = path.rsplit('/').next().unwrap_or_default();
    if matches!(runtime, "node" | "bun" | "deno") {
        let args = process_args(pid)?;
        return args.iter().skip(1).take(3).find_map(|a| script_agent(&a.to_lowercase()));
    }
    None
}

/// The agent a Node script path belongs to.
fn script_agent(arg: &str) -> Option<&'static str> {
    if arg.contains("@openai/codex") || arg.ends_with("/codex.js") || arg.ends_with("/codex") {
        Some("codex")
    } else if arg.contains("gemini-cli") || arg.ends_with("/gemini.js") || arg.ends_with("/gemini") {
        Some("gemini")
    } else {
        None
    }
}

/// A process's arguments (`argv`), program first.
#[cfg(target_os = "macos")]
fn process_args(pid: libc::pid_t) -> Option<Vec<String>> {
    // KERN_PROCARGS2: argc (i32), the exec path, padding NULs, then argv.
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) } != 0 || size < 4 {
        return None;
    }
    let mut buf = vec![0u8; size];
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    buf.truncate(size);
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?) as usize;
    let rest = &buf[4..];
    let exec_end = rest.iter().position(|&b| b == 0)?;
    let start = exec_end + rest[exec_end..].iter().position(|&b| b != 0)?;
    Some(rest[start..].split(|&b| b == 0).take(argc).map(|a| String::from_utf8_lossy(a).into_owned()).collect())
}

#[cfg(not(target_os = "macos"))]
fn process_args(pid: libc::pid_t) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(raw.split(|&b| b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect())
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
    /// Its process id (for Claude, the key to its session record).
    pub pid: Option<libc::pid_t>,
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
            // Agents take their own name (`…/claude/versions/2.1.303`, `node …/codex.js`).
            let name = agent_of(pid).map(String::from).or_else(|| path.file_name().map(|f| f.to_string_lossy().into_owned()))?;
            Some((pid, name))
        })
        .collect();
    let pick = named.iter().find(|(_, n)| !is_shell(n)).or(named.first());
    match pick {
        Some((pid, name)) => Foreground { program: Some(name.clone()), cwd: process_cwd(*pid), pid: Some(*pid) },
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

/// What agent `agent` is doing: from what it reports (Codex's OSC 7501
/// `status`), its window title, or its screen (`lines`, top to bottom). Agents
/// without rules count as working while their output is flowing (`busy`).
pub fn classify(agent: &str, title: &str, lines: &[String], status: Option<&str>, busy: bool) -> AgentState {
    let recent: Vec<String> = lines.iter().rev().filter(|l| !l.trim().is_empty()).take(12).map(|l| l.to_lowercase()).collect();
    let any = |pat: &str| recent.iter().any(|l| l.contains(pat));
    let title_l = title.trim().to_lowercase();
    let spinner = |t: &str| t.chars().next().is_some_and(|c| ('\u{2800}'..='\u{28FF}').contains(&c));
    match agent {
        "claude" => classify_claude(title, lines),
        "codex" => match status {
            Some("working") => AgentState::Working,
            Some("blocked") => AgentState::Blocked,
            Some(_) => AgentState::Idle,
            None if any("press enter to confirm or esc to cancel") || title_l.contains("action required") => AgentState::Blocked,
            None if any("would you like to") && recent.iter().any(|l| l.trim_start().trim_start_matches('›').trim_start().starts_with("1. yes")) => {
                AgentState::Blocked
            }
            None if any("to interrupt)") || spinner(title) => AgentState::Working,
            None => AgentState::Idle,
        },
        "gemini" => {
            if title_l.starts_with('✋') || title_l.contains("action required") || any("waiting for user confirmation") {
                AgentState::Blocked
            } else if title_l.starts_with('✦') || title_l.starts_with('⏲') || any("(esc to cancel") {
                AgentState::Working
            } else {
                AgentState::Idle
            }
        }
        "opencode" => {
            if any("permission required") {
                AgentState::Blocked
            } else if any("esc interrupt") || any("esc again to interrupt") {
                AgentState::Working
            } else {
                AgentState::Idle
            }
        }
        _ if busy => AgentState::Working,
        _ => AgentState::Idle,
    }
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

    #[test]
    fn codex_states() {
        // Its own status report wins.
        assert_eq!(classify("codex", "", &lines("› "), Some("working"), false), AgentState::Working);
        assert_eq!(classify("codex", "", &lines("› "), Some("blocked"), false), AgentState::Blocked);
        assert_eq!(classify("codex", "", &lines("› "), Some("idle"), false), AgentState::Idle);
        // Else the screen.
        assert_eq!(classify("codex", "", &lines("⠙ Working (12s • esc to interrupt)\n› "), None, false), AgentState::Working);
        let approval = lines("Would you like to run the following command?\n  $ rm -rf dist\n› 1. Yes, proceed (y)\n  3. No, and tell Codex what to do differently (esc)\nPress enter to confirm or esc to cancel");
        assert_eq!(classify("codex", "", &approval, None, false), AgentState::Blocked);
        assert_eq!(classify("codex", "[ ! ] Action Required", &lines("› "), None, false), AgentState::Blocked);
        assert_eq!(classify("codex", "", &lines("› "), None, true), AgentState::Idle, "output alone isn't work");
    }

    #[test]
    fn gemini_states() {
        assert_eq!(classify("gemini", "✋  Action Required (app)      ", &lines(""), None, false), AgentState::Blocked);
        assert_eq!(classify("gemini", "✦  Working… (app)", &lines(""), None, false), AgentState::Working);
        assert_eq!(classify("gemini", "◇  Ready (app)", &lines(""), None, false), AgentState::Idle);
        // Title off: the screen.
        assert_eq!(classify("gemini", "Gemini CLI (app)", &lines("⠏ Waiting for user confirmation..."), None, false), AgentState::Blocked);
        assert_eq!(classify("gemini", "Gemini CLI (app)", &lines("⠋ Thinking... (esc to cancel, 12s)"), None, false), AgentState::Working);
    }

    #[test]
    fn opencode_states() {
        assert_eq!(classify("opencode", "OC | fix", &lines("⠋ esc interrupt"), None, false), AgentState::Working);
        assert_eq!(classify("opencode", "OC | fix", &lines("△ Permission required\n  Edit src/a.ts\nAllow once  Allow always  Reject"), None, false), AgentState::Blocked);
        assert_eq!(classify("opencode", "OC | fix", &lines("> "), None, false), AgentState::Idle);
        // Agents without rules: working while output flows.
        assert_eq!(classify("aider", "", &lines(""), None, true), AgentState::Working);
    }

    #[test]
    fn reads_process_arguments() {
        let me = std::process::id() as libc::pid_t;
        let args = process_args(me).expect("argv of this test process");
        assert!(args[0].contains("velmux_tui") || args[0].contains("deps"), "{args:?}");
        assert_eq!(script_agent("/opt/homebrew/lib/node_modules/@openai/codex/bin/codex.js"), Some("codex"));
        assert_eq!(script_agent("/usr/local/lib/node_modules/@google/gemini-cli/bundle/gemini.js"), Some("gemini"));
        assert_eq!(script_agent("/srv/app/server.js"), None);
    }
}
