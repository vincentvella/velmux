//! Session-start hooks for the agents velmux can't otherwise follow: each
//! runs `velmux agent-hook <agent>` when a session starts, which tells the
//! server which session the pane's agent is in, so restore can resume it.
//!
//! Where each agent keeps its hooks, and their shapes, are adapted from
//! herdr's integrations (src/integration, Apache-2.0; see NOTICE). velmux's
//! entries are told apart by their `agent-hook` command, so your own hooks
//! are left alone.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::agents::Agent;

/// The session id in a hook's JSON input, the way `agent` reports it.
pub fn session_id(agent: Agent, input: &str) -> Option<String> {
    if agent == Agent::Grok {
        if let Some(id) = std::env::var("GROK_SESSION_ID").ok().filter(|s| !s.is_empty()) {
            return Some(id);
        }
    }
    let v: Value = serde_json::from_str(input).ok()?;
    let id = ["session_id", "sessionId", "conversation_id", "conversationId"].iter().find_map(|k| v[k].as_str().filter(|s| !s.is_empty()))?;
    // Letta's default conversation is per agent: `default:<agent id>`.
    if agent == Agent::Letta && id == "default" {
        return v["agent_id"].as_str().filter(|a| !a.is_empty()).map(|a| format!("default:{a}"));
    }
    (id.len() <= 512 && !id.chars().any(char::is_control)).then(|| id.to_string())
}

/// What a hook must print, for agents that read its output: Antigravity
/// wants JSON, the rest nothing (Letta would pass it on to the model).
pub fn hook_output(agent: Agent) -> &'static str {
    if agent == Agent::Antigravity {
        "{}"
    } else {
        ""
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn env_dir(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| home().join(default))
}

/// How an agent lists its hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Claude's: `hooks.<event>[] = {matcher?, hooks: [{type, command, timeout}]}`.
    Nested { matcher: Option<&'static str>, timeout: u64, quiet: bool },
    /// Copilot's: `hooks.SessionStart[] = {type, bash, timeoutSec}`.
    Direct,
    /// Cursor's `hooks.json`: `{version: 1, hooks: {sessionStart: [{command}]}}`.
    Simple,
    /// Mastra Code's `hooks.json`: `SessionStart[]` at the top.
    Flat,
    /// Antigravity's `hooks.json`: a block of velmux's own, `velmux.PreInvocation[]`.
    Block,
    /// Grok merges every `hooks/*.json`: a file of velmux's own.
    OwnFile,
    /// Kimi Code's `config.toml`: a marked block of `[[hooks]]`.
    Toml,
}

/// Where `agent` keeps its hooks: its config folder (it's set up when that
/// exists), the file, and the shape.
fn target(agent: Agent) -> Option<(PathBuf, PathBuf, Shape)> {
    let config = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| home().join(".config"));
    let (dir, file, shape) = match agent {
        Agent::Copilot => (env_dir("COPILOT_HOME", ".copilot"), "settings.json", Shape::Direct),
        Agent::Cursor => (env_dir("CURSOR_CONFIG_DIR", ".cursor"), "hooks.json", Shape::Simple),
        Agent::Droid => (home().join(".factory"), "settings.json", Shape::Nested { matcher: None, timeout: 10, quiet: false }),
        Agent::Devin => (config.join("devin"), "config.json", Shape::Nested { matcher: None, timeout: 10, quiet: false }),
        // Qwen Code's timeouts are in milliseconds.
        Agent::Qwen => (env_dir("QWEN_HOME", ".qwen"), "settings.json", Shape::Nested { matcher: Some("*"), timeout: 10000, quiet: false }),
        Agent::Qoder => (env_dir("QODER_CONFIG_DIR", ".qoder"), "settings.json", Shape::Nested { matcher: Some("*"), timeout: 10, quiet: false }),
        Agent::Letta => (home().join(".letta"), "settings.json", Shape::Nested { matcher: None, timeout: 10000, quiet: true }),
        Agent::Grok => (env_dir("GROK_HOME", ".grok"), "hooks/velmux.json", Shape::OwnFile),
        Agent::Mastracode => (home().join(".mastracode"), "hooks.json", Shape::Flat),
        Agent::Antigravity => (env_dir("ANTIGRAVITY_CLI_CONFIG_DIR", ".gemini/config"), "hooks.json", Shape::Block),
        Agent::Kimi => (env_dir("KIMI_CODE_HOME", ".kimi-code"), "config.toml", Shape::Toml),
        Agent::Claude | Agent::Codex | Agent::Gemini | Agent::Opencode => return None,
    };
    let file = dir.join(file);
    Some((dir, file, shape))
}

/// The hook's command line: velmux by the path you run it as (on `PATH`, so a
/// Homebrew upgrade doesn't strand it), else this binary.
pub fn command(agent: Agent) -> String {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let exe = std::env::split_paths(&path)
        .map(|d| d.join("velmux"))
        .find(|p| p.is_file())
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| "velmux".into());
    format!("{} agent-hook {}", crate::agents::quote(&exe.display().to_string()), agent.name())
}

fn ours(entry: &Value) -> bool {
    entry.to_string().contains(" agent-hook ")
}

/// The array at `keys` in `root`, made if missing.
fn array_at<'a>(root: &'a mut Value, keys: &[&str]) -> Option<&'a mut Vec<Value>> {
    let mut v = root;
    for (i, k) in keys.iter().enumerate() {
        let obj = v.as_object_mut()?;
        let default = if i + 1 == keys.len() { json!([]) } else { json!({}) };
        v = obj.entry(k.to_string()).or_insert(default);
    }
    v.as_array_mut()
}

/// The path to `shape`'s session-start list, and velmux's entry in it.
fn entry(shape: Shape, command: &str) -> (Vec<&'static str>, Value) {
    match shape {
        Shape::Nested { matcher, timeout, quiet } => {
            let mut hook = json!({ "type": "command", "command": command, "timeout": timeout });
            if quiet {
                hook["quiet"] = json!(true);
            }
            let mut e = json!({ "hooks": [hook] });
            if let Some(m) = matcher {
                e["matcher"] = json!(m);
            }
            (vec!["hooks", "SessionStart"], e)
        }
        Shape::OwnFile => (vec!["hooks", "SessionStart"], json!({ "hooks": [{ "type": "command", "command": command, "timeout": 10 }] })),
        Shape::Direct => (vec!["hooks", "SessionStart"], json!({ "type": "command", "bash": command, "timeoutSec": 10 })),
        Shape::Simple => (vec!["hooks", "sessionStart"], json!({ "command": command })),
        Shape::Flat => (vec!["SessionStart"], json!({ "type": "command", "command": command, "timeout": 10000, "description": "Tell velmux which session this is" })),
        Shape::Block => (vec!["velmux", "PreInvocation"], json!({ "type": "command", "command": command, "timeout": 10 })),
        Shape::Toml => unreachable!("not JSON"),
    }
}

/// `text` (a JSON settings file) with velmux's hook for `agent` put in, or
/// taken out; `None` when it isn't JSON (it's left alone).
fn edit_json(text: &str, shape: Shape, command: &str, install: bool) -> Option<String> {
    let mut root: Value = if text.trim().is_empty() { json!({}) } else { serde_json::from_str(text).ok()? };
    root.as_object()?;
    if shape == Shape::Simple && install && root.get("version").is_none() {
        root["version"] = json!(1);
    }
    let (keys, new) = entry(shape, command);
    let list = array_at(&mut root, &keys)?;
    list.retain(|e| !ours(e));
    if install {
        list.push(new);
    } else if list.is_empty() {
        // Tidy up the list it leaves empty, and the `hooks` (or velmux's
        // own block) holding it.
        let parent = keys[..keys.len() - 1].iter().try_fold(&mut root, |v, k| v.get_mut(*k))?;
        parent.as_object_mut()?.remove(keys[keys.len() - 1]);
        if keys.len() == 2 && parent.as_object().is_some_and(|o| o.is_empty()) {
            root.as_object_mut()?.remove(keys[0]);
        }
    }
    Some(serde_json::to_string_pretty(&root).ok()? + "\n")
}

const TOML_START: &str = "# >>> velmux (session hook; `velmux skill uninstall` removes it)";
const TOML_END: &str = "# <<< velmux";

fn edit_toml(text: &str, command: &str, install: bool) -> String {
    let mut out = match (text.find(TOML_START), text.find(TOML_END)) {
        (Some(a), Some(b)) if b > a => format!("{}{}", &text[..a], text[b + TOML_END.len()..].trim_start_matches('\n')),
        _ => text.to_string(),
    };
    if install {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        let cmd = command.replace('\\', "\\\\").replace('"', "\\\"");
        out += &format!("{TOML_START}\n[[hooks]]\nevent = \"SessionStart\"\ncommand = \"{cmd}\"\ntimeout = 10\n{TOML_END}\n");
    }
    out
}

/// Put in (or with `install` false take out) velmux's session hook for every
/// hooked agent set up here. Returns what changed, as `(agent label, file)`,
/// and what couldn't be (one agent's odd settings don't stop the rest).
pub fn sync(install: bool) -> (Vec<(&'static str, PathBuf)>, Vec<String>) {
    let (mut changed, mut problems) = (Vec::new(), Vec::new());
    for agent in crate::agents::ALL.into_iter().filter(|a| a.hooked()) {
        let Some((dir, file, shape)) = target(agent) else { continue };
        if !dir.is_dir() {
            continue;
        }
        let result = edited(&file, shape, &command(agent), install).and_then(|new| {
            let Some(new) = new else { return Ok(false) };
            if shape == Shape::OwnFile && !install {
                std::fs::remove_file(&file).with_context(|| format!("removing {}", file.display()))?;
            } else {
                std::fs::create_dir_all(file.parent().unwrap())?;
                std::fs::write(&file, new).with_context(|| format!("writing {}", file.display()))?;
            }
            Ok(true)
        });
        match result {
            Ok(true) => changed.push((agent.label(), file)),
            Ok(false) => {}
            Err(e) => problems.push(format!("{}: {e:#}", agent.label())),
        }
    }
    (changed, problems)
}

/// The new contents of `file`, when they'd change.
fn edited(file: &Path, shape: Shape, command: &str, install: bool) -> Result<Option<String>> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !install {
                return Ok(None);
            }
            String::new()
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
    };
    let new = match shape {
        Shape::Toml => edit_toml(&text, command, install),
        _ => match edit_json(&text, shape, command, install) {
            Some(new) => new,
            // Settings with comments, or broken: not ours to rewrite.
            None => anyhow::bail!("{} isn't plain JSON; add velmux's hook by hand: {command}", file.display()),
        },
    };
    Ok((new != text).then_some(new))
}

/// Agents set up here whose session hook isn't installed yet.
pub fn missing() -> Vec<&'static str> {
    crate::agents::ALL
        .into_iter()
        .filter(|a| a.hooked())
        .filter_map(|a| {
            let (dir, file, _) = target(a)?;
            let has = std::fs::read_to_string(&file).is_ok_and(|t| t.contains(" agent-hook "));
            (dir.is_dir() && !has).then_some(a.label())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: &str = "'/opt/bin/velmux' agent-hook droid";

    #[test]
    fn reads_the_session_from_each_shape_of_input() {
        assert_eq!(session_id(Agent::Droid, r#"{"session_id":"s1","hook_event_name":"SessionStart"}"#).as_deref(), Some("s1"));
        assert_eq!(session_id(Agent::Copilot, r#"{"sessionId":"s2"}"#).as_deref(), Some("s2"));
        assert_eq!(session_id(Agent::Antigravity, r#"{"conversationId":"c3"}"#).as_deref(), Some("c3"));
        assert_eq!(session_id(Agent::Letta, r#"{"conversation_id":"default","agent_id":"agent-7"}"#).as_deref(), Some("default:agent-7"));
        assert_eq!(session_id(Agent::Droid, r#"{"prompt":"hi"}"#), None);
        assert_eq!(session_id(Agent::Droid, "not json"), None);
    }

    #[test]
    fn adds_its_hook_beside_yours_and_takes_it_out_again() {
        let mine = r#"{"model":"x","hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"my-hook"}]}]}}"#;
        let shape = Shape::Nested { matcher: Some("*"), timeout: 10, quiet: false };
        let added = edit_json(mine, shape, CMD, true).unwrap();
        let v: Value = serde_json::from_str(&added).unwrap();
        let list = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1], json!({"matcher":"*","hooks":[{"type":"command","command":CMD,"timeout":10}]}));
        assert_eq!(v["model"], "x");
        // Installing again doesn't add a second one.
        assert_eq!(edit_json(&added, shape, CMD, true).unwrap(), added);
        let removed: Value = serde_json::from_str(&edit_json(&added, shape, CMD, false).unwrap()).unwrap();
        assert_eq!(removed, serde_json::from_str::<Value>(mine).unwrap());
    }

    #[test]
    fn writes_each_agents_shape() {
        let v = |shape| serde_json::from_str::<Value>(&edit_json("", shape, CMD, true).unwrap()).unwrap();
        assert_eq!(v(Shape::Direct), json!({"hooks":{"SessionStart":[{"type":"command","bash":CMD,"timeoutSec":10}]}}));
        assert_eq!(v(Shape::Simple), json!({"version":1,"hooks":{"sessionStart":[{"command":CMD}]}}));
        assert_eq!(v(Shape::Flat)["SessionStart"][0]["timeout"], 10000);
        assert_eq!(v(Shape::Block), json!({"velmux":{"PreInvocation":[{"type":"command","command":CMD,"timeout":10}]}}));
        let quiet = v(Shape::Nested { matcher: None, timeout: 10000, quiet: true });
        assert_eq!(quiet["hooks"]["SessionStart"][0], json!({"hooks":[{"type":"command","command":CMD,"timeout":10000,"quiet":true}]}));
        // Antigravity's block goes when it's empty; settings with comments are left alone.
        let block = edit_json(&edit_json("{}", Shape::Block, CMD, true).unwrap(), Shape::Block, CMD, false).unwrap();
        assert_eq!(block.trim(), "{}");
        assert_eq!(edit_json("// mine\n{}", Shape::Direct, CMD, true), None);
    }

    #[test]
    fn keeps_a_marked_block_in_kimis_config() {
        let mine = "default_model = \"k2\"\n";
        let added = edit_toml(mine, CMD, true);
        assert!(added.starts_with(mine) && added.contains("event = \"SessionStart\"") && added.contains("command = \"'/opt/bin/velmux' agent-hook droid\""));
        assert_eq!(edit_toml(&added, CMD, true), added);
        assert_eq!(edit_toml(&added, CMD, false), mine);
    }
}
