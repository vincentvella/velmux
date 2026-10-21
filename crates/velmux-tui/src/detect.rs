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

const AGENTS: &[&str] = &[
    "claude", "codex", "opencode", "gemini", "cursor-agent", "aider", "copilot", "droid", "devin", "qwen", "grok", "qodercli", "kimi", "letta",
    "mastracode", "agy", "kilo", "pi", "omp", "hermes",
];

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
    // The first few also by a folder they're installed in (`…/codex/bin/codex`);
    // the rest by the executable's own name only (a user called devin…).
    let file = path.rsplit('/').next().unwrap_or_default();
    if let Some(a) = AGENTS.iter().enumerate().find(|(i, a)| file == **a || *i < 6 && path.split('/').any(|part| part == **a)).map(|(_, a)| *a) {
        return Some(a);
    }
    let runtime = path.rsplit('/').next().unwrap_or_default();
    if matches!(runtime, "node" | "bun" | "deno") {
        let args = process_args(pid)?;
        return args.iter().skip(1).take(3).find_map(|a| script_agent(&a.to_lowercase()));
    }
    // Hermes runs as a Python script (`python …/bin/hermes`).
    if runtime.starts_with("python") {
        let args = process_args(pid)?;
        return args.iter().skip(1).take(3).any(|a| a.ends_with("/hermes")).then_some("hermes");
    }
    None
}

/// Agents that run as Node scripts, by package (from herdr's detection).
const SCRIPTS: &[(&str, &str)] = &[
    ("@github/copilot", "copilot"),
    ("@qwen-code/qwen-code", "qwen"),
    ("@moonshot-ai/kimi-code", "kimi"),
    ("@letta-ai/letta-code", "letta"),
    ("/mastracode/dist/", "mastracode"),
    ("@oh-my-pi/pi-coding-agent", "omp"),
    ("/pi-coding-agent/", "pi"),
];

/// The agent a Node script path belongs to.
fn script_agent(arg: &str) -> Option<&'static str> {
    if arg.contains("@openai/codex") || arg.ends_with("/codex.js") || arg.ends_with("/codex") {
        Some("codex")
    } else if arg.contains("gemini-cli") || arg.ends_with("/gemini.js") || arg.ends_with("/gemini") {
        Some("gemini")
    } else if let Some(&(_, agent)) = SCRIPTS.iter().find(|(pkg, _)| arg.contains(pkg)) {
        Some(agent)
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
    if agent == "claude" {
        return classify_claude(title, lines);
    }
    let lines: Vec<&str> = lines.iter().map(|l| l.trim_end()).collect();
    match agent {
        "codex" => classify_codex(title, &lines, status),
        "gemini" => classify_gemini(title, &lines),
        "opencode" => classify_opencode(&lines),
        _ if busy => AgentState::Working,
        _ => AgentState::Idle,
    }
}

// The Codex, Gemini CLI and opencode rules below are adapted from herdr's
// agent-detection manifests (distribution/agent-detection/*.toml, Apache-2.0;
// see NOTICE). Its text matches are case-insensitive, its regexes aren't.

/// `text` contains every one of `pats` (lowercase), ignoring case.
fn has(text: &str, pats: &[&str]) -> bool {
    pats.iter().all(|p| text.contains(p))
}

/// Whitespace runs as single spaces (herdr's `\s+`).
fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn non_empty<'a, 'b>(lines: &'b [&'a str]) -> impl DoubleEndedIterator<Item = &'a str> + 'b {
    lines.iter().copied().filter(|l| !l.trim().is_empty())
}

/// A braille spinner frame as a word of its own in a window title.
fn spinner_word(title: &str) -> bool {
    title.split(' ').any(|w| w.chars().count() == 1 && w.chars().all(|c| "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(c)))
}

fn codex_prompt(line: &str) -> bool {
    line == "›" || line.starts_with("› ")
}

/// A line that starts a new block of Codex output (a response, an error…).
fn codex_block(line: &str) -> bool {
    line.starts_with(['•', '■', '✗', '✓'])
}

/// Codex's input prompt, when nothing has been printed after it.
fn codex_current_prompt(lines: &[&str]) -> Option<usize> {
    let i = lines.iter().rposition(|l| codex_prompt(l))?;
    (!lines[i + 1..].iter().any(|l| codex_block(l))).then_some(i)
}

/// `1h 2m 3s`, `12s`: Codex's elapsed timer.
fn codex_timer(s: &str) -> bool {
    let words: Vec<&str> = s.split(' ').collect();
    let unit = |w: &str, units: &str| w.len() > 1 && w[..w.len() - 1].bytes().all(|b| b.is_ascii_digit()) && units.contains(&w[w.len() - 1..]);
    words.split_last().is_some_and(|(last, rest)| unit(last, "s") && rest.iter().all(|w| unit(w, "hm")))
}

/// Codex's live status line: `Working (12s • esc to interrupt)`, with an
/// optional `• ` bullet and `· …` after it.
fn codex_status_line(line: &str) -> bool {
    let line = line.strip_prefix(['•', '◦']).filter(|r| r.starts_with([' ', '\t'])).map_or(line, |r| r.trim_start_matches([' ', '\t']));
    if line.starts_with(|c: char| c.is_whitespace() || "›•◦■✗✓─".contains(c)) || line.starts_with("Reconnect failed — check the endpoint, then relaunch (") {
        return false;
    }
    line.match_indices(" (").any(|(i, _)| {
        let rest = &line[i + 2..];
        rest.match_indices(')').any(|(j, _)| {
            let (inner, after) = (&rest[..j], &rest[j + 1..]);
            let tail_ok = after.is_empty() || after.starts_with(" · ");
            let inner_ok = codex_timer(inner)
                || inner.split_once(" • ").is_some_and(|(t, hint)| codex_timer(t) && hint.strip_suffix(" to interrupt").is_some_and(|h| !h.is_empty()));
            tail_ok && inner_ok
        })
    })
}

/// Lines Codex leaves under its status line while working: plain text, queued
/// messages, a prompt with an animated sparkle.
fn codex_trailer(line: &str) -> bool {
    let sparkle = |l: &str| l.strip_prefix('›').and_then(|r| r.chars().next()).is_some_and(|c| "⠁⠂⠄⠈⠐⠠⡀⢀".contains(c));
    let queued = ["Queued follow-up inputs", "Messages to be submitted after next tool call", "Messages to be submitted at end of turn"];
    !line.starts_with(['•', '◦', '›', '■', '✗', '✓', '─'])
        || sparkle(line)
        || line.strip_prefix('•').is_some_and(|r| queued.iter().any(|q| squash(r).starts_with(q)))
}

/// Codex: the state it reports (OSC 7501) and its title, with herdr's screen
/// rules for what those miss (its start-up dialogs, approvals and questions).
pub fn classify_codex(title: &str, lines: &[&str], status: Option<&str>) -> AgentState {
    if title.to_lowercase().contains("action required") {
        return AgentState::Blocked;
    }
    // Start-up dialogs: trusting the folder, updating.
    let top: Vec<&str> = non_empty(lines).take(20).collect();
    let top_l = top.join("\n").to_lowercase();
    let trust_where = top.first().is_some_and(|l| l.starts_with("> You are in ")) || top_l.contains("folder access");
    let trust_ask = squash(&top.join("\n")).contains("Do you trust the contents of this directory?")
        || has(&top_l, &["trust this folder?", "codex can read, edit, and run files here"]) && (top_l.contains("trust and continue") || top_l.contains("enter continue"));
    let mut bottom: Vec<&str> = non_empty(lines).rev().take(20).collect();
    bottom.reverse();
    let bottom_s = squash(&bottom.join("\n"));
    let bottom_l = bottom_s.to_lowercase();
    let update = has(&bottom_l, &["update available!", "update now"]) && (bottom_s.contains("Skip until next version") || bottom_s.ends_with("Press enter to continue"))
        || bottom.iter().any(|l| squash(l).starts_with("Update available ·")) && bottom_s.ends_with("enter continue · esc skip");
    if trust_where && trust_ask || update {
        return AgentState::Blocked;
    }
    match status {
        Some("working") => return AgentState::Working,
        Some("blocked") => return AgentState::Blocked,
        Some(_) => return AgentState::Idle,
        None => {}
    }
    if spinner_word(title) {
        return AgentState::Working;
    }
    // A live approval or question below the last prompt.
    let after = lines.iter().rposition(|l| codex_prompt(l)).map_or(lines, |i| &lines[i + 1..]).join("\n").to_lowercase();
    let strong = ["press enter to confirm or esc to cancel", "enter to submit answer", "enter to submit all", "allow command?"].iter().any(|p| after.contains(p))
        || has(&after, &["all results", "filesystem only", "plugins"]);
    if strong {
        return AgentState::Blocked;
    }
    let current = codex_current_prompt(lines);
    if current.is_none() {
        let sparkling = lines.iter().rposition(|l| l.starts_with('›')).is_some_and(|i| codex_trailer(lines[i]) && lines[i..].iter().all(|l| codex_trailer(l) || l.is_empty()));
        let all = lines.join("\n").to_lowercase();
        let asks = all.contains("[y/n]")
            || all.contains("yes (y)")
            || (all.contains("do you want to") || all.contains("would you like to")) && (all.contains("yes") || all.contains('❯'));
        if asks && !sparkling {
            return AgentState::Blocked;
        }
    }
    // The status line, with nothing but its trailers after it.
    let before = current.map_or(lines, |i| &lines[..i]);
    for line in before.iter().rev().filter(|l| !l.is_empty()) {
        if codex_status_line(line) {
            return AgentState::Working;
        }
        if !codex_trailer(line) {
            break;
        }
    }
    AgentState::Idle
}

/// Gemini CLI: its window title, else its confirmation dialogs and the cancel hint.
pub fn classify_gemini(title: &str, lines: &[&str]) -> AgentState {
    let title_l = title.trim().to_lowercase();
    let all = lines.join("\n").to_lowercase();
    let confirm = all.contains("│ apply this change")
        || all.contains("│ allow execution")
        || all.contains("yes") && ["waiting for user confirmation", "│ do you want to proceed", "do you want to proceed?"].iter().any(|p| all.contains(p))
        || lines.iter().any(|l| l.trim_start().strip_prefix('❯').is_some_and(|r| r.to_lowercase().contains("yes") || r.to_lowercase().contains("allow")));
    if title_l.starts_with('✋') || title_l.contains("action required") || all.contains("waiting for user confirmation") || confirm {
        AgentState::Blocked
    } else if title_l.starts_with('✦') || title_l.starts_with('⏲') || all.contains("esc to cancel") {
        AgentState::Working
    } else {
        AgentState::Idle
    }
}

/// opencode: its permission prompt, else its interrupt hint or progress bar.
pub fn classify_opencode(lines: &[&str]) -> AgentState {
    let all = lines.join("\n").to_lowercase();
    let picker = all.contains("esc dismiss")
        && ["enter confirm", "enter submit", "enter toggle"].iter().any(|p| all.contains(p))
        && (all.contains("↑↓ select") || all.contains("⇆ tab"));
    if all.contains("△ permission required") || all.contains("permission required") || picker {
        return AgentState::Blocked;
    }
    let interrupt = ["esc to interrupt", "ctrl+c to interrupt", "press esc to interrupt", "esc interrupt", "esc again to interrupt"].iter().any(|p| all.contains(p));
    let progress = lines.iter().any(|l| {
        let mut run = 0;
        l.chars().any(|c| {
            run = if c == '■' || c == '⬝' { run + 1 } else { 0 };
            run >= 4
        })
    });
    if interrupt || progress {
        AgentState::Working
    } else {
        AgentState::Idle
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
    fn codex_screens() {
        let codex = |title: &str, screen: &str| classify("codex", title, &lines(screen), None, false);
        // Start-up dialogs block even when it reports idle.
        let trust = "> You are in /w/app\n\n  Do you trust the contents of this\n  directory? Working with untrusted contents\n› 1. Yes, continue\n  2. No, quit";
        assert_eq!(classify("codex", "", &lines(trust), Some("idle"), false), AgentState::Blocked);
        let folder = "  Folder access\n  Trust this folder?\n  Codex can read, edit, and run files here.\n  enter continue · esc quit";
        assert_eq!(codex("", folder), AgentState::Blocked);
        let update = "  ✨ Update available! 0.150.0 -> 0.160.0\n› 1. Update now\n  2. Skip\n  3. Skip until next version\n  Press enter to continue";
        assert_eq!(codex("", update), AgentState::Blocked);
        let picker = "  Update available · 0.170.0\n  Release notes\n  enter continue · esc skip";
        assert_eq!(codex("", picker), AgentState::Blocked);
        // Questions and approvals after the prompt.
        assert_eq!(codex("", "› fix it\n\n  Which database?\n  1. sqlite\n  enter to submit answer"), AgentState::Blocked);
        assert_eq!(codex("", "› ship it\n  allow command?\n  $ git push"), AgentState::Blocked);
        // A yes/no question with no prompt under it.
        assert_eq!(codex("", "• Ran tests\n  Do you want to commit? [y/n]"), AgentState::Blocked);
        // The title spinner.
        assert_eq!(codex("⠹ app", "› "), AgentState::Working);
        assert_eq!(codex("app ⠹⠹", "› "), AgentState::Idle, "a lone frame only");
        // The live status line, also with queued input and a sparkle prompt under it.
        assert_eq!(codex("", "• Working (1m 05s • esc to interrupt)\n› "), AgentState::Working);
        assert_eq!(codex("", "• Exploring (8s)\n• Queued follow-up inputs\n  also fix the tests\n›⠂ "), AgentState::Working);
        assert_eq!(codex("", "◦ Thinking (2h 1m 3s • ctrl+c to interrupt) · 3 background tasks\n\n› "), AgentState::Working);
        // A response after it, or a failed reconnect: done.
        assert_eq!(codex("", "• Working (12s • esc to interrupt)\n• Done, edited 2 files\n› "), AgentState::Idle);
        assert_eq!(codex("", "• Reconnect failed — check the endpoint, then relaunch (30s)\n› "), AgentState::Idle);
        assert_eq!(codex("", "• Explained the code (see notes)\n› "), AgentState::Idle, "parentheses aren't a timer");
    }

    #[test]
    fn gemini_and_opencode_screens() {
        let gemini = |screen: &str| classify("gemini", "Gemini CLI (app)", &lines(screen), None, false);
        assert_eq!(gemini("╭──\n│ Apply this change?\n│ ● 1. Yes, allow once"), AgentState::Blocked);
        assert_eq!(gemini("│ Allow execution of: 'rm -rf dist'?"), AgentState::Blocked);
        assert_eq!(gemini("  ❯ 1. Allow once\n    2. No"), AgentState::Blocked);
        assert_eq!(gemini("> Type your message"), AgentState::Idle);
        let opencode = |screen: &str| classify("opencode", "OC | fix", &lines(screen), None, false);
        assert_eq!(opencode("  Allow edit to src/a.ts?\n  ↑↓ select  enter confirm  esc dismiss"), AgentState::Blocked);
        assert_eq!(opencode("  ■■■■⬝⬝⬝⬝  Build"), AgentState::Working);
        assert_eq!(opencode("  ctrl+c to interrupt"), AgentState::Working);
        assert_eq!(opencode("  ■■ two"), AgentState::Idle);
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
        assert_eq!(script_agent("/opt/homebrew/lib/node_modules/@qwen-code/qwen-code/dist/index.js"), Some("qwen"));
    }
}
