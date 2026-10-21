//! Keeping Claudes that velmux starts out of a repo's checkout: a Claude Code
//! `PreToolUse` hook (`velmux guard`, which explains what to do instead) and a
//! `permissions.deny` rule (enforced by Claude itself, so it holds where hooks
//! are turned off), passed with `--settings`. Both merge with your own settings.

use std::path::{Path, PathBuf};

/// The `--settings` contents for Claudes in `base`'s repo (its checkout and
/// slots): `exe guard` before every file edit, and edits under `base` denied.
/// The deny rule is left out if the pool lives inside the checkout, since it
/// would block the slots too.
pub fn settings(exe: &Path, base: &Path, pool_dir: &Path) -> serde_json::Value {
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    let mut value = serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                // Edit rules cover every built-in tool that writes files.
                "matcher": "Edit|Write|MultiEdit|NotebookEdit",
                "hooks": [{ "type": "command", "command": format!("{} guard", quote(&exe.display().to_string())) }]
            }]
        }
    });
    if !pool_dir.starts_with(base) {
        // `//` makes it an absolute path (a single `/` is relative to this file).
        let rule = format!("Edit(//{}/**)", base.display().to_string().trim_start_matches('/'));
        value["permissions"] = serde_json::json!({ "deny": [rule] });
    }
    value
}

/// What keeps the guard from working here, from Claude's own settings: your
/// user settings, and (stronger) your organisation's managed settings.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Policy {
    /// Hooks off (or only managed ones run): the guard hook doesn't.
    pub hooks_off: Option<String>,
    /// Only managed permission rules apply: the deny rule doesn't.
    pub rules_off: Option<String>,
}

/// Claude's managed settings files, merged in its order (the main file, then
/// `managed-settings.d/*.json` alphabetically).
fn managed_files() -> Vec<PathBuf> {
    let dir = if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ClaudeCode")
    } else {
        PathBuf::from("/etc/claude-code")
    };
    let mut files = vec![dir.join("managed-settings.json")];
    if let Ok(entries) = std::fs::read_dir(dir.join("managed-settings.d")) {
        let mut extra: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        extra.sort();
        files.extend(extra);
    }
    files
}

pub fn policy(claude_dir: &Path) -> Policy {
    let read = |p: &Path| std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    let managed: Vec<(PathBuf, serde_json::Value)> = managed_files().into_iter().filter_map(|p| read(&p).map(|v| (p, v))).collect();
    let user = [claude_dir.join("settings.json"), claude_dir.join("settings.local.json")];
    let user: Vec<(PathBuf, serde_json::Value)> = user.into_iter().filter_map(|p| read(&p).map(|v| (p, v))).collect();
    let set = |files: &[(PathBuf, serde_json::Value)], key: &str| files.iter().find(|(_, v)| v[key] == true).map(|(p, _)| p.display().to_string());
    let hooks_off = set(&managed, "disableAllHooks")
        .map(|p| format!("disableAllHooks in {p}"))
        .or_else(|| set(&managed, "allowManagedHooksOnly").map(|p| format!("allowManagedHooksOnly in {p}")))
        .or_else(|| set(&user, "disableAllHooks").map(|p| format!("disableAllHooks in {p}")));
    let rules_off = set(&managed, "allowManagedPermissionRulesOnly").map(|p| format!("allowManagedPermissionRulesOnly in {p}"));
    Policy { hooks_off, rules_off }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_edits_under_the_checkout_by_absolute_path() {
        let s = settings(Path::new("/opt/bin/velmux"), Path::new("/Users/me/ws/app"), Path::new("/Users/me/ws/.pools/app"));
        assert_eq!(s["permissions"]["deny"][0], "Edit(//Users/me/ws/app/**)");
        assert_eq!(s["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "'/opt/bin/velmux' guard");
        // A pool inside the checkout: the rule would block the slots, so only the hook.
        let inside = settings(Path::new("/v"), Path::new("/w/app"), Path::new("/w/app/.pools"));
        assert!(inside.get("permissions").is_none());
    }

    #[test]
    fn reads_what_turns_it_off() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(policy(tmp.path()).hooks_off, None);
        std::fs::write(tmp.path().join("settings.json"), r#"{"disableAllHooks": true}"#).unwrap();
        assert!(policy(tmp.path()).hooks_off.unwrap().contains("disableAllHooks"));
    }
}
