//! What the server persists so a restart (upgrade, crash, reboot) brings every
//! project, layout and pane back, with Claude resuming its conversation in the
//! slot it ran in.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::layout::Layout;
use crate::pane::Kind;
use crate::paths;

#[derive(Serialize, Deserialize)]
pub struct Saved {
    pub version: u32,
    /// Base checkouts, in sidebar order.
    pub projects: Vec<PathBuf>,
    /// Folders in the "unclassified" group (outside any repo).
    #[serde(default)]
    pub folders: Vec<SavedFolder>,
    pub views: Vec<SavedView>,
}

#[derive(Serialize, Deserialize)]
pub struct SavedFolder {
    pub id: String,
    pub path: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub struct SavedView {
    /// Base checkout (empty for unclassified folders).
    pub project: PathBuf,
    /// Slot id, or the folder id when `folder`.
    pub slot: Option<String>,
    #[serde(default)]
    pub folder: bool,
    /// The first tab (kept so older velmux can still read the file).
    pub layout: Layout,
    pub panes: Vec<SavedPane>,
    /// Every tab; when present, `layout` / `panes` mirror the first one.
    #[serde(default)]
    pub tabs: Vec<SavedTab>,
    #[serde(default)]
    pub active_tab: usize,
    /// Name given with rename (or carried over from herdr).
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SavedTab {
    pub layout: Layout,
    pub panes: Vec<SavedPane>,
    #[serde(default)]
    pub name: Option<String>,
}

impl SavedView {
    /// The view's tabs, whether saved by this version or one without tabs.
    pub fn take_tabs(&mut self) -> Vec<SavedTab> {
        if self.tabs.is_empty() {
            vec![SavedTab { layout: self.layout.clone(), panes: std::mem::take(&mut self.panes), name: None }]
        } else {
            std::mem::take(&mut self.tabs)
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SavedPane {
    pub id: u64,
    pub kind: Kind,
    /// Claude session the pane was launched with (`--session-id` / `--resume`).
    pub claude_session: Option<String>,
    /// Unix seconds; transcripts newer than this may belong to the pane (`/clear`).
    pub started_at: u64,
    /// Working directory, when it isn't the location's root (e.g. a subfolder).
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Exact command, when the pane runs something other than the configured agent.
    #[serde(default)]
    pub command: Option<String>,
}

/// State passed to the next server across a live restart.
#[derive(Serialize, Deserialize)]
pub struct Handoff {
    pub saved: Saved,
    pub live: Vec<LivePane>,
    pub focus: Option<u64>,
}

/// A pane whose pty stays open across `exec`.
#[derive(Serialize, Deserialize)]
pub struct LivePane {
    pub id: u64,
    pub fd: i32,
    pub pid: u32,
    pub rows: u16,
    pub cols: u16,
    /// The screen and terminal modes, replayed into the new parser.
    pub screen: String,
    pub title: String,
}

pub fn handoff_file() -> Result<PathBuf> {
    Ok(paths::state_dir()?.join("handoff.json"))
}

fn file() -> Result<PathBuf> {
    Ok(paths::state_dir()?.join("session.json"))
}

pub fn save(saved: &Saved) -> Result<()> {
    let path = file()?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(saved)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

pub fn load() -> Option<Saved> {
    let text = std::fs::read_to_string(file().ok()?).ok()?;
    serde_json::from_str(&text).ok().filter(|s: &Saved| s.version == 1)
}

/// Whether the configured agent command launches Claude Code.
pub fn is_claude(agent: &str) -> bool {
    agent
        .split_whitespace()
        .next()
        .and_then(|prog| Path::new(prog).file_name())
        .is_some_and(|name| name == "claude")
}

/// Pin a command's program to an absolute path using `path_var` (the server's
/// PATH, inherited from the terminal that started velmux). Panes run through a
/// login shell, whose profile can reorder PATH: without this, `claude` in a pane
/// may be a different install than `claude` in your terminal.
pub fn resolve_program(command: &str, path_var: Option<&std::ffi::OsStr>) -> String {
    use std::os::unix::fs::PermissionsExt;
    let trimmed = command.trim_start();
    let prog_len = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let (prog, rest) = trimmed.split_at(prog_len);
    if prog.is_empty() || prog.contains('/') {
        return command.to_string();
    }
    let Some(path_var) = path_var else { return command.to_string() };
    let found = std::env::split_paths(path_var).map(|dir| dir.join(prog)).find(|p| {
        p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    });
    match found {
        Some(p) if !p.to_string_lossy().contains(char::is_whitespace) => format!("{}{rest}", p.display()),
        _ => command.to_string(),
    }
}

/// A fresh v4 UUID for `claude --session-id`.
pub fn new_session_id() -> String {
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        // Fall back to time + pid; uniqueness is all Claude needs.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        b[..16].copy_from_slice(&(nanos ^ ((std::process::id() as u128) << 64)).to_le_bytes());
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_claude_commands() {
        assert!(is_claude("claude"));
        assert!(is_claude("/Users/me/.local/bin/claude --model opus"));
        assert!(!is_claude("codex"));
        assert!(!is_claude("claude-wrapper"));
    }

    #[test]
    fn resolves_programs_with_the_given_path_order() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let (first, second) = (tmp.path().join("a"), tmp.path().join("b"));
        for dir in [&first, &second] {
            std::fs::create_dir_all(dir).unwrap();
            let bin = dir.join("claude");
            std::fs::write(&bin, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(first.join("notexec"), "").unwrap();
        let path = std::env::join_paths([&first, &second]).unwrap();
        let p = Some(path.as_os_str());
        assert_eq!(resolve_program("claude --resume x", p), format!("{}/claude --resume x", first.display()));
        assert_eq!(resolve_program("claude", p), format!("{}/claude", first.display()));
        // Explicit paths, unknown or non-executable programs are left alone.
        assert_eq!(resolve_program("/opt/x/claude -c", p), "/opt/x/claude -c");
        assert_eq!(resolve_program("nope --flag", p), "nope --flag");
        assert_eq!(resolve_program("notexec", p), "notexec");
        assert_eq!(resolve_program("claude", None), "claude");
    }

    #[test]
    fn session_ids_are_v4_uuids() {
        let id = new_session_id();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(id, new_session_id());
    }
}
