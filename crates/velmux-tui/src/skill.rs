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
    remember(path, SKILL);
    Ok(verb)
}

/// FNV-1a 64 of `text`: a fingerprint of what velmux wrote.
fn fingerprint(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Where velmux keeps the fingerprint of the skill it wrote at `path`.
fn record(path: &Path) -> Option<PathBuf> {
    let name = format!("skill-{}", velmux_pool::claude::sanitize(path));
    crate::paths::state_dir().ok().map(|d| d.join(name))
}

fn remember(path: &Path, text: &str) {
    if let Some(r) = record(path) {
        let _ = std::fs::write(r, fingerprint(text));
    }
}

/// How the installed skill at `path` stands against the one velmux ships.
#[derive(Debug, PartialEq, Eq)]
pub enum Freshness {
    Missing,
    Current,
    /// Older than this velmux's, and as velmux wrote it: safe to replace.
    Stale,
    /// Different from this velmux's, and edited (or written before velmux kept
    /// track): left alone; `velmux skill install` replaces it.
    Changed,
}

pub fn freshness(path: &Path) -> Freshness {
    let Ok(text) = std::fs::read_to_string(path) else { return Freshness::Missing };
    if text == SKILL {
        return Freshness::Current;
    }
    let ours = record(path).and_then(|r| std::fs::read_to_string(r).ok()).is_some_and(|f| f.trim() == fingerprint(&text));
    if ours { Freshness::Stale } else { Freshness::Changed }
}

/// After an update: bring what velmux installed up to date. The skill at
/// `path`, if it's still as velmux wrote it, and velmux's block in other
/// agents' files (its own, between markers). Returns what to tell you: what
/// it updated, or that the skill needs `velmux skill install`.
pub fn refresh(path: &Path) -> Vec<String> {
    let mut said = Vec::new();
    match freshness(path) {
        Freshness::Stale => {
            if std::fs::write(path, SKILL).is_ok() {
                remember(path, SKILL);
                said.push("updated velmux's Claude skill".to_string());
            }
        }
        Freshness::Changed => said.push("velmux's Claude skill is out of date: velmux skill install updates it".to_string()),
        Freshness::Missing | Freshness::Current => {}
    }
    for (agent, file) in agent_files() {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        if !text.contains(START) {
            continue;
        }
        let new = with_block(&text);
        if new != text && std::fs::write(&file, new).is_ok() {
            said.push(format!("updated velmux's instructions for {agent}"));
        }
    }
    said
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

/// Everything `velmux skill install` does beyond the skill itself: velmux's
/// instructions in other agents' files and their session hooks. Returns what
/// changed (lines to show) and what couldn't be done.
pub fn install_others() -> (Vec<String>, Vec<String>) {
    let mut done = Vec::new();
    let mut problems = Vec::new();
    for (agent, file) in agent_files() {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let new = with_block(&text);
        if new != text {
            match std::fs::write(&file, new) {
                Ok(()) => done.push(format!("added velmux's instructions for {agent} to {}", file.display())),
                Err(e) => problems.push(format!("writing {}: {e}", file.display())),
            }
        }
    }
    let (changed, hook_problems) = crate::hooks::sync(true);
    done.extend(changed.into_iter().map(|(agent, file)| format!("added velmux's session hook for {agent} to {}", file.display())));
    problems.extend(hook_problems);
    (done, problems)
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
    fn tells_its_own_stale_skill_from_one_youve_changed() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("VELMUX_STATE_DIR", tmp.path().join("state"));
        std::fs::create_dir_all(tmp.path().join("state")).unwrap();
        let path = tmp.path().join("skills/velmux/SKILL.md");
        assert_eq!(freshness(&path), Freshness::Missing);
        install(&path).unwrap();
        assert_eq!(freshness(&path), Freshness::Current);
        // An older one velmux wrote: replaced on refresh.
        std::fs::write(&path, "old skill").unwrap();
        remember(&path, "old skill");
        assert_eq!(freshness(&path), Freshness::Stale);
        assert_eq!(refresh(&path).first().map(String::as_str), Some("updated velmux's Claude skill"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SKILL);
        // One you changed: left alone, with a word about it.
        std::fs::write(&path, "my own edits").unwrap();
        assert_eq!(freshness(&path), Freshness::Changed);
        assert!(refresh(&path)[0].contains("out of date"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "my own edits");
    }

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
