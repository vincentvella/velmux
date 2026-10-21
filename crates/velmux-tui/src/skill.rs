//! The Claude Code skill that teaches Claude to work in velmux.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const SKILL: &str = include_str!("skill.md");

/// For every repo: `<claude dir>/skills/velmux/SKILL.md`.
pub fn user_path(claude_dir: &Path) -> PathBuf {
    claude_dir.join("skills/velmux/SKILL.md")
}

/// For one repo, committed with it: `<repo>/.claude/skills/velmux/SKILL.md`.
pub fn project_path(base: &Path) -> PathBuf {
    base.join(".claude/skills/velmux/SKILL.md")
}

/// Installed for every repo, or for `base`.
pub fn installed(claude_dir: &Path, base: Option<&Path>) -> bool {
    user_path(claude_dir).exists() || base.is_some_and(|b| project_path(b).exists())
}

/// Write (or update) the skill at `path`; says which it did.
pub fn install(path: &Path) -> Result<&'static str> {
    let verb = match std::fs::read_to_string(path) {
        Ok(old) if old == SKILL => "already up to date:",
        Ok(_) => "updated",
        Err(_) => "installed",
    };
    std::fs::create_dir_all(path.parent().unwrap()).with_context(|| format!("creating {}", path.display()))?;
    std::fs::write(path, SKILL).with_context(|| format!("writing {}", path.display()))?;
    Ok(verb)
}
