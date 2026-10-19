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
    pub views: Vec<SavedView>,
}

#[derive(Serialize, Deserialize)]
pub struct SavedView {
    pub project: PathBuf,
    pub slot: Option<String>,
    pub layout: Layout,
    pub panes: Vec<SavedPane>,
}

#[derive(Serialize, Deserialize)]
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
    fn session_ids_are_v4_uuids() {
        let id = new_session_id();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(id, new_session_id());
    }
}
