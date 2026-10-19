use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

pub const DEFAULT_SLOTS: usize = 5;

/// Paths that survive `release` by default: dependency trees and build caches
/// are the whole point of keeping a slot warm.
pub const DEFAULT_KEEP: &[&str] = &[
    "node_modules",
    ".expo",
    ".turbo",
    ".next/cache",
    "ios/Pods",
    "android/.gradle",
    "target",
    ".venv",
    ".direnv",
];

/// Process-level inputs, injected so tests never touch the real home directory.
#[derive(Debug, Clone)]
pub struct Env {
    pub global_config: PathBuf,
    /// Claude Code's config dir (`$CLAUDE_CONFIG_DIR` or `~/.claude`).
    pub claude_dir: PathBuf,
}

impl Env {
    pub fn from_process() -> Result<Env> {
        let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
        let global_config = std::env::var_os("VELMUX_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config/velmux/config.toml"));
        let claude_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        Ok(Env { global_config, claude_dir })
    }
}

/// `~/.config/velmux/config.toml`
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalFile {
    slots: Option<usize>,
    pool_root: Option<PathBuf>,
    editor: Option<String>,
    agent: Option<String>,
    claude_history: Option<bool>,
    prefix: Option<String>,
}

/// The TUI prefix key from the global config (e.g. `"C-a"`), if set.
/// Server-wide, so it lives outside the per-repo [`Config`].
pub fn prefix_key(env: &Env) -> Result<Option<String>> {
    let global: GlobalFile = read_toml(&env.global_config)?;
    Ok(global.prefix)
}

/// `<base>/.velmux.toml`
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoFile {
    slots: Option<usize>,
    base_ref: Option<String>,
    keep: Option<Vec<String>>,
    link: Option<Vec<String>>,
    post_claim: Option<Vec<String>>,
    install: Option<bool>,
    install_cmd: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub slots: usize,
    /// Directory that holds `<repo>/slot-N`. Defaults to `<base parent>/.pools`.
    pub pool_root: Option<PathBuf>,
    pub editor: Option<String>,
    /// Command the TUI launches for an agent pane.
    pub agent: String,
    pub claude_history: bool,
    pub base_ref: Option<String>,
    pub keep: Vec<String>,
    /// Gitignored files symlinked from base into every slot (`.env*` when unset).
    pub link: Option<Vec<String>>,
    pub post_claim: Vec<String>,
    pub install: bool,
    pub install_cmd: Option<String>,
}

impl Config {
    pub fn load(env: &Env, base: &Path) -> Result<Config> {
        let global: GlobalFile = read_toml(&env.global_config)?;
        let repo: RepoFile = read_toml(&base.join(".velmux.toml"))?;
        Ok(Config {
            slots: repo.slots.or(global.slots).unwrap_or(DEFAULT_SLOTS).max(1),
            pool_root: global.pool_root,
            editor: global.editor,
            agent: global.agent.unwrap_or_else(|| "claude".into()),
            claude_history: global.claude_history.unwrap_or(true),
            base_ref: repo.base_ref,
            keep: repo
                .keep
                .unwrap_or_else(|| DEFAULT_KEEP.iter().map(|s| s.to_string()).collect()),
            link: repo.link,
            post_claim: repo.post_claim.unwrap_or_default(),
            install: repo.install.unwrap_or(true),
            install_cmd: repo.install_cmd,
        })
    }
}

fn read_toml<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}
