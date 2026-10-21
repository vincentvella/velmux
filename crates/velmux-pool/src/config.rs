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
    mouse: Option<bool>,
    /// Restart the TUI server in place when the velmux binary changes.
    auto_restart: Option<bool>,
    /// TUI key bindings: action name → one spec or a list.
    keys: Option<std::collections::BTreeMap<String, KeySpecs>>,
    notify: Option<NotifyFile>,
    /// UI theme name (`velmux` has the list), and per-role colour overrides.
    theme: Option<String>,
    theme_colors: Option<std::collections::BTreeMap<String, String>>,
    sync: Option<SyncMode>,
    /// Minutes between background `git fetch`es of each repo (0 turns it off).
    fetch_every: Option<u64>,
    /// Fast-forward each repo's checkout to its upstream after a fetch.
    fast_forward: Option<bool>,
    /// Keep Claudes velmux starts from editing a repo's checkout (a hook and
    /// deny rules, passed with `--settings`).
    guard: Option<bool>,
    ports: Option<PortsFile>,
    /// Where new scratch sessions start, and where `new space` looks for repos.
    scratch_dir: Option<PathBuf>,
    /// Check velmux.com for a newer velmux now and then.
    update_check: Option<bool>,
}

/// `[ports]`: each slot gets its own block, so dev servers in different slots
/// don't collide. Slot N's value for each variable is `base + N * stride`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct PortsFile {
    enabled: Option<bool>,
    stride: Option<u16>,
    /// Variable → its value in the base checkout, e.g. `PORT = 3000`.
    env: Option<std::collections::BTreeMap<String, u16>>,
}

/// Resolved `[ports]`.
#[derive(Debug, Clone)]
pub struct Ports {
    pub enabled: bool,
    pub stride: u16,
    pub env: std::collections::BTreeMap<String, u16>,
}

impl Ports {
    /// The variables for slot `n` (the base checkout is `0`, left alone).
    pub fn for_slot(&self, n: u16) -> Vec<(String, u16)> {
        if !self.enabled || n == 0 {
            return Vec::new();
        }
        self.env.iter().filter_map(|(k, base)| Some((k.clone(), base.checked_add(n.checked_mul(self.stride)?)?))).collect()
    }

    /// The one to show and open: `PORT` if set, else the first.
    pub fn primary(&self, n: u16) -> Option<u16> {
        let all = self.for_slot(n);
        all.iter().find(|(k, _)| k == "PORT").or(all.first()).map(|(_, v)| *v)
    }
}

/// How "sync with base" brings a slot's branch up to date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncMode {
    #[default]
    Rebase,
    Merge,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifyFile {
    desktop: Option<bool>,
    sound: Option<bool>,
    done_sound: Option<String>,
    blocked_sound: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum KeySpecs {
    One(String),
    Many(Vec<String>),
}

/// Server-wide TUI settings from the global config (outside the per-repo [`Config`]).
#[derive(Debug, Default)]
pub struct TuiSettings {
    /// Prefix key spec, e.g. `"C-a"`.
    pub prefix: Option<String>,
    /// Mouse support (default on).
    pub mouse: bool,
    /// Live-restart when the velmux binary changes (default on).
    pub auto_restart: bool,
    /// Key binding overrides: action name → specs (empty = unbound).
    pub keys: std::collections::BTreeMap<String, Vec<String>>,
    pub notify: NotifySettings,
    pub theme: Option<String>,
    pub theme_colors: std::collections::BTreeMap<String, String>,
    /// Where new scratch sessions start and `new space` looks for repos:
    /// `scratch_dir`, else `~/Workspace` if it exists, else `~`.
    pub scratch_dir: PathBuf,
    /// Check velmux.com for a newer velmux (at startup, then twice a day).
    pub update_check: bool,
}

/// When an agent finishes unseen or gets blocked on you.
#[derive(Debug, Clone)]
pub struct NotifySettings {
    /// A desktop notification (through the terminal, or the OS when detached).
    pub desktop: bool,
    pub sound: bool,
    /// A macOS system sound name (Glass, Funk, Ping, …) or a sound file path.
    pub done_sound: String,
    pub blocked_sound: String,
}

impl Default for NotifySettings {
    fn default() -> NotifySettings {
        NotifySettings { desktop: true, sound: true, done_sound: "herdr-done".into(), blocked_sound: "herdr-request".into() }
    }
}

pub fn tui_settings(env: &Env) -> Result<TuiSettings> {
    let global: GlobalFile = read_toml(&env.global_config)?;
    let keys = global
        .keys
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k, match v { KeySpecs::One(s) => vec![s], KeySpecs::Many(v) => v }))
        .collect();
    let n = global.notify.unwrap_or_default();
    let d = NotifySettings::default();
    let notify = NotifySettings {
        desktop: n.desktop.unwrap_or(d.desktop),
        sound: n.sound.unwrap_or(d.sound),
        done_sound: n.done_sound.unwrap_or(d.done_sound),
        blocked_sound: n.blocked_sound.unwrap_or(d.blocked_sound),
    };
    Ok(TuiSettings {
        prefix: global.prefix,
        mouse: global.mouse.unwrap_or(true),
        auto_restart: global.auto_restart.unwrap_or(true),
        keys,
        notify,
        theme: global.theme,
        theme_colors: global.theme_colors.unwrap_or_default(),
        update_check: global.update_check.unwrap_or(true),
        scratch_dir: {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            match global.scratch_dir {
                Some(p) => match p.strip_prefix("~") {
                    Ok(rest) => home.join(rest),
                    Err(_) => p,
                },
                None if home.join("Workspace").is_dir() => home.join("Workspace"),
                None => home,
            }
        },
    })
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
    sync: Option<SyncMode>,
    fast_forward: Option<bool>,
    guard: Option<bool>,
    ports: Option<PortsFile>,
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
    pub sync: SyncMode,
    /// Minutes between background fetches (0 = never).
    pub fetch_every: u64,
    /// Fast-forward the checkout to its upstream after each fetch.
    pub fast_forward: bool,
    /// Keep velmux-started Claudes from editing the checkout.
    pub guard: bool,
    pub ports: Ports,
}

impl Config {
    pub fn load(env: &Env, base: &Path) -> Result<Config> {
        let global: GlobalFile = read_toml(&env.global_config)?;
        let repo: RepoFile = read_toml(&base.join(".velmux.toml"))?;
        Ok(Config::from_files(global, repo))
    }

    /// Built-in defaults, with no config files at all.
    pub fn defaults() -> Config {
        Config::from_files(GlobalFile::default(), RepoFile::default())
    }

    fn from_files(global: GlobalFile, repo: RepoFile) -> Config {
        Config {
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
            sync: repo.sync.or(global.sync).unwrap_or_default(),
            fetch_every: global.fetch_every.unwrap_or(15),
            fast_forward: repo.fast_forward.or(global.fast_forward).unwrap_or(true),
            guard: repo.guard.or(global.guard).unwrap_or(true),
            ports: {
                let (g, r) = (global.ports.unwrap_or_default(), repo.ports.unwrap_or_default());
                Ports {
                    enabled: r.enabled.or(g.enabled).unwrap_or(true),
                    stride: r.stride.or(g.stride).unwrap_or(10).max(1),
                    env: r.env.or(g.env).unwrap_or_else(|| [("PORT".to_string(), 3000)].into()),
                }
            },
        }
    }
}

fn read_toml<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}
