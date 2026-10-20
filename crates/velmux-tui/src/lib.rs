//! The velmux TUI: a background server owns agent ptys and renders the UI;
//! thin clients attach, detach, and reattach without losing panes.

mod client;
mod copy;
mod detect;
mod herdr;
mod keys;
mod layout;
mod mouse;
mod pane;
mod prefix;
mod proto;
mod server;
mod session;
mod ui;

pub use client::attach;

/// `velmux keys`: the effective bindings, or `--ghostty` for the terminal config
/// that makes Cmd chords reach velmux's root (prefix-less) bindings.
pub fn print_keys(ghostty: bool) -> anyhow::Result<()> {
    let settings = velmux_pool::tui_settings(&velmux_pool::Env::from_process()?)?;
    let prefix = match settings.prefix.as_deref() {
        Some(spec) => prefix::Prefix::parse(spec)?,
        None => prefix::Prefix::default(),
    };
    let (map, warnings) = keys::Keymap::build(prefix.byte, &settings.keys);
    for w in &warnings {
        eprintln!("warning: {w}");
    }
    if ghostty {
        print!("{}", keys::ghostty_config(&map));
        return Ok(());
    }
    println!("prefix: {}", prefix.label);
    for (action, specs) in &map.specs {
        println!("  {:<18} {}", keys::name_of(*action), specs.join("  "));
    }
    Ok(())
}

/// `velmux import-herdr`: plan from herdr's session file, then hand it to the
/// server (or just print the plan with `dry_run`).
pub fn import_herdr(
    file: Option<std::path::PathBuf>,
    session: Option<&str>,
    with_latest_claude: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    use anyhow::Context;
    let path = match file {
        Some(f) => f,
        None => herdr::default_path(session)?,
    };
    let path = path.canonicalize().with_context(|| format!("no herdr session at {}", path.display()))?;
    if dry_run {
        let plan = herdr::plan(&path, &velmux_pool::Env::from_process()?, with_latest_claude)?;
        print!("would import from {}:\n{}", path.display(), plan.describe());
        return Ok(());
    }
    let reply = client::request(proto::ClientMsg::ImportHerdr { path, with_latest_claude })?;
    print!("{reply}");
    if !reply.starts_with("imported 0 ") && !reply.starts_with("import failed") {
        println!("run `velmux` to see them");
    }
    Ok(())
}
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
