//! The velmux TUI: a background server owns agent ptys and renders the UI;
//! thin clients attach, detach, and reattach without losing panes.

mod client;
mod copy;
mod detect;
mod herdr;
mod keys;
mod layout;
mod mouse;
mod notify;
mod pane;
mod picker;
mod prefix;
mod proto;
mod pty_raw;
mod server;
mod search;
mod session;
pub mod skill;
pub mod ghostty;
pub mod guard;
mod theme;
mod ui;

pub use client::attach;

/// The velmux pane this process runs in, when a server is there to ask.
pub fn current_pane() -> Option<u64> {
    let pane = std::env::var("VELMUX_PANE").ok()?.parse().ok()?;
    std::os::unix::net::UnixStream::connect(paths::socket().ok()?).ok()?;
    Some(pane)
}

/// `velmux claim` inside a pane: the server claims a slot and moves this pane
/// there. Returns the slot's path.
pub fn claim_for_pane(pane: u64, branch: Option<String>) -> anyhow::Result<String> {
    let reply = client::request(proto::ClientMsg::ClaimFor { pane, branch })?;
    match reply.strip_prefix("error: ") {
        Some(e) => anyhow::bail!("{e}"),
        None => Ok(reply),
    }
}

/// `velmux release` inside a pane: release its slot, moving the pane back to base.
pub fn release_for_pane(pane: u64) -> anyhow::Result<String> {
    let reply = client::request(proto::ClientMsg::ReleaseFor { pane })?;
    match reply.strip_prefix("error: ") {
        Some(e) => anyhow::bail!("{e}"),
        None => Ok(reply),
    }
}

/// `velmux restart`: re-exec the running server (picking up a rebuilt binary)
/// without stopping any pane.
pub fn restart_server() -> anyhow::Result<()> {
    if std::os::unix::net::UnixStream::connect(paths::socket()?).is_err() {
        println!("no velmux server running");
        return Ok(());
    }
    println!("{}", client::request(proto::ClientMsg::Restart)?);
    Ok(())
}

/// `velmux debug`: the focused pane as the server sees it.
pub fn debug_server() -> anyhow::Result<()> {
    print!("{}", client::request(proto::ClientMsg::Debug)?);
    Ok(())
}

/// `velmux stop`: save and stop the server.
pub fn stop_server() -> anyhow::Result<()> {
    if std::os::unix::net::UnixStream::connect(paths::socket()?).is_err() {
        println!("no velmux server running");
        return Ok(());
    }
    println!("{}", client::request(proto::ClientMsg::Stop)?);
    Ok(())
}

/// `velmux keys --test`: show the bytes each key press sends and what velmux
/// would do with them. Run it in a plain terminal tab (not inside velmux, which
/// would act on the keys first). ctrl+c exits.
pub fn key_test() -> anyhow::Result<()> {
    use std::io::{Read, Write};
    let settings = velmux_pool::tui_settings(&velmux_pool::Env::from_process()?)?;
    let prefix = match settings.prefix.as_deref() {
        Some(spec) => prefix::Prefix::parse(spec)?,
        None => prefix::Prefix::default(),
    };
    let (map, _) = keys::Keymap::build(prefix.byte, &settings.keys);
    if std::env::var_os("VELMUX").is_some() {
        eprintln!("note: this is a velmux pane, so velmux sees your keys first; run this in a plain terminal tab");
    }
    println!("press keys to see what your terminal sends (prefix is {}); ctrl+c to quit", prefix.label);
    crossterm::terminal::enable_raw_mode()?;
    let mut stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let mut buf = [0u8; 256];
    let mut after_prefix = false;
    loop {
        let n = stdin.read(&mut buf)?;
        let bytes = &buf[..n];
        if bytes == [0x03] {
            break;
        }
        let shown: String = bytes
            .iter()
            .map(|&b| match b {
                0x1b => "ESC".to_string(),
                0x20..=0x7e => (b as char).to_string(),
                b => format!("\\x{b:02x}"),
            })
            .collect::<Vec<_>>()
            .join(" ");
        let verdict = if after_prefix {
            after_prefix = false;
            match map.after_prefix(bytes) {
                Some((a, _)) => format!("after prefix → {a:?}"),
                None => "after prefix → (unbound)".into(),
            }
        } else if bytes == [prefix.byte] {
            after_prefix = true;
            "prefix".into()
        } else {
            match map.find_root(bytes) {
                Some((0, a, len)) if len == bytes.len() => format!("prefix-less → {a:?}"),
                Some((at, a, _)) => format!("prefix-less → {a:?} (with extra bytes around it at {at})"),
                None => "→ goes to the pane".into(),
            }
        };
        write!(out, "{shown:<28} {verdict}\r\n")?;
        out.flush()?;
    }
    crossterm::terminal::disable_raw_mode()?;
    Ok(())
}

/// `velmux keys`: the effective bindings, or `--ghostty` for the terminal config
/// that makes Cmd chords reach velmux's root (prefix-less) bindings.
pub fn print_keys(ghostty: bool, install: bool) -> anyhow::Result<()> {
    let (map, prefix, warnings) = keymap()?;
    for w in &warnings {
        eprintln!("warning: {w}");
    }
    if install {
        let path = ghostty::install(&ghostty_lines(&map))?;
        println!("wrote velmux's keybinds into {}; reload Ghostty's config (⌘⇧, on macOS)", path.display());
        return Ok(());
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

/// The effective keymap from the global config.
fn keymap() -> anyhow::Result<(keys::Keymap, prefix::Prefix, Vec<String>)> {
    let settings = velmux_pool::tui_settings(&velmux_pool::Env::from_process()?)?;
    let prefix = match settings.prefix.as_deref() {
        Some(spec) => prefix::Prefix::parse(spec)?,
        None => prefix::Prefix::default(),
    };
    let (map, warnings) = keys::Keymap::build(prefix.byte, &settings.keys);
    Ok((map, prefix, warnings))
}

/// The Ghostty keybind lines for `map` (without the header comment).
pub(crate) fn ghostty_lines(map: &keys::Keymap) -> String {
    keys::ghostty_config(map).lines().skip(1).collect::<Vec<_>>().join("\n")
}

/// `velmux doctor`'s view of the terminal: Ghostty, and whether it sends the keys.
pub fn terminal_check() -> (bool, bool) {
    (ghostty::detected(), ghostty::configured())
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
