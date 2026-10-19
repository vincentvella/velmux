//! The velmux TUI: a background server owns agent ptys and renders the UI;
//! thin clients attach, detach, and reattach without losing panes.

mod client;
mod detect;
mod layout;
mod pane;
mod prefix;
mod proto;
mod server;
mod ui;

pub use client::attach;
pub use server::serve;

pub mod paths {
    use std::path::PathBuf;

    use anyhow::{Context, Result};

    pub fn state_dir() -> Result<PathBuf> {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        let dir = std::env::var_os("VELMUX_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(home).join(".local/state/velmux"));
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Unix socket paths are capped at ~104 bytes, so like tmux the socket lives
    /// in a short per-user dir rather than under the state dir.
    pub fn socket() -> Result<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        let path = match std::env::var_os("VELMUX_SOCKET") {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(format!("/tmp/velmux-{}/server.sock", unsafe { libc::getuid() })),
        };
        let dir = path.parent().context("socket path has no parent")?;
        if !dir.exists() {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(path)
    }

    pub fn log() -> Result<PathBuf> {
        Ok(state_dir()?.join("server.log"))
    }
}
