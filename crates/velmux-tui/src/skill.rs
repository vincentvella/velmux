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

const START: &str = "<!-- velmux:start (written by `velmux skill install`; rerun it to update) -->";
const END: &str = "<!-- velmux:end -->";

/// The instruction files other agents read for every project, for those set
/// up here: (agent, file).
pub fn agent_files() -> Vec<(&'static str, PathBuf)> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    let codex = crate::agents::codex_home();
    let gemini = crate::agents::gemini_home();
    let opencode = config.join("opencode");
    [("Codex", codex.clone(), codex.join("AGENTS.md")), ("Gemini CLI", gemini.clone(), gemini.join("GEMINI.md")), ("opencode", opencode.clone(), opencode.join("AGENTS.md"))]
        .into_iter()
        .filter(|(_, dir, _)| dir.is_dir())
        .map(|(name, _, file)| (name, file))
        .collect()
}

/// The skill as plain instructions (no frontmatter), for other agents' files.
fn instructions() -> &'static str {
    SKILL.splitn(3, "---\n").nth(2).unwrap_or(SKILL).trim_start()
}

/// `text` with velmux's block (between markers) put in, replacing an old one.
pub fn with_block(text: &str) -> String {
    let block = format!("{START}\n{}\n{END}\n", instructions().trim_end());
    match (text.find(START), text.find(END)) {
        (Some(a), Some(b)) if b > a => format!("{}{block}{}", &text[..a], text[b + END.len()..].trim_start_matches('\n')),
        _ if text.trim().is_empty() => block,
        _ => format!("{}\n\n{block}", text.trim_end()),
    }
}

/// `text` without velmux's block.
pub fn without_block(text: &str) -> String {
    match (text.find(START), text.find(END)) {
        (Some(a), Some(b)) if b > a => {
            let rest = format!("{}{}", text[..a].trim_end(), &text[b + END.len()..]);
            if rest.trim().is_empty() { String::new() } else { format!("{}\n", rest.trim_end()) }
        }
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn puts_and_removes_its_block_in_other_agents_files() {
        let fresh = with_block("");
        assert!(fresh.starts_with(START) && fresh.contains("velmux claim") && !fresh.contains("name: velmux"));
        let mine = "# My rules\nbe terse\n";
        let added = with_block(mine);
        assert!(added.starts_with("# My rules\nbe terse\n\n<!-- velmux:start"));
        assert_eq!(with_block(&added), added, "updating replaces, never duplicates");
        assert_eq!(without_block(&added), mine);
        assert_eq!(without_block(&fresh), "");
    }
}
