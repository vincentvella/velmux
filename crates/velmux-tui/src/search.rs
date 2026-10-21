//! Searching agent conversations: an index of each transcript's title, your
//! prompts and the agent's replies (tool calls and results left out), rebuilt
//! in the background for files that changed. Claude Code, Codex and Gemini CLI
//! transcripts; a doc's `id` is a session reference (see `agents`).

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// What's kept of each side of a conversation, so a huge transcript can't
/// swamp memory (prompts first: they say what the session was about).
const PROMPT_CAP: usize = 48 << 10;
const REPLY_CAP: usize = 48 << 10;

#[derive(Debug, Clone)]
pub struct Doc {
    pub id: String,
    pub path: PathBuf,
    pub mtime: SystemTime,
    pub size: u64,
    /// `/rename` title, else Claude's own, else the first prompt.
    pub title: String,
    /// Where it ran (the first message's folder and branch).
    pub cwd: Option<PathBuf>,
    pub branch: Option<String>,
    /// Prompts, then replies, one message per line.
    pub text: String,
    /// `text` with ASCII lowercased (same byte offsets), for matching.
    lower: String,
}

/// Index every transcript in `dirs`, reusing `old` docs whose file is unchanged.
pub fn scan(dirs: &[PathBuf], old: Vec<Doc>) -> Vec<Doc> {
    let mut old: HashMap<PathBuf, Doc> = old.into_iter().map(|d| (d.path.clone(), d)).collect();
    let mut docs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "jsonl") {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            let (mtime, size) = (meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len());
            // Slot folders can be links to the base's: index each file once.
            if !seen.insert(path.canonicalize().unwrap_or_else(|_| path.clone())) {
                continue;
            }
            match old.remove(&path) {
                Some(d) if d.mtime == mtime && d.size == size => docs.push(d),
                _ => docs.extend(index_file(&path, mtime, size)),
            }
        }
    }
    docs.sort_by_key(|d| std::cmp::Reverse(d.mtime));
    docs
}

fn index_file(path: &Path, mtime: SystemTime, size: u64) -> Option<Doc> {
    let id = path.file_stem()?.to_string_lossy().into_owned();
    let file = std::fs::File::open(path).ok()?;
    let (mut prompts, mut replies) = (String::new(), String::new());
    let (mut ai_title, mut custom_title, mut cwd, mut branch) = (None, None, None, None);
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        // Cheap checks before parsing: most lines are tool traffic.
        let kind = if line.contains(r#""type":"user""#) && !line.contains(r#""tool_result""#) {
            1
        } else if line.contains(r#""type":"assistant""#) && line.contains(r#""type":"text""#) {
            2
        } else if line.contains(r#"-title""#) {
            3
        } else {
            continue;
        };
        if kind != 3 && (line.contains(r#""isSidechain":true"#) || line.contains(r#""isMeta":true"#)) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        match kind {
            3 => {
                if let Some(t) = v.get("customTitle").and_then(|t| t.as_str()) {
                    custom_title = Some(t.to_string());
                } else if let Some(t) = v.get("aiTitle").and_then(|t| t.as_str()) {
                    ai_title = Some(t.to_string());
                }
            }
            _ => {
                if cwd.is_none() {
                    cwd = v.get("cwd").and_then(|c| c.as_str()).map(PathBuf::from);
                    branch = v.get("gitBranch").and_then(|b| b.as_str()).filter(|b| !b.is_empty()).map(String::from);
                }
                let text = message_text(&v);
                // Slash commands, hook output and the like arrive as tagged text.
                if text.trim().is_empty() || text.trim_start().starts_with('<') {
                    continue;
                }
                let (buf, cap) = if kind == 1 { (&mut prompts, PROMPT_CAP) } else { (&mut replies, REPLY_CAP) };
                if buf.len() < cap {
                    let room = cap - buf.len();
                    buf.push_str(truncate(&text, room));
                    buf.push('\n');
                }
            }
        }
    }
    if prompts.is_empty() && replies.is_empty() {
        return None;
    }
    let first = prompts.lines().next().unwrap_or_default().to_string();
    let title = custom_title.or(ai_title).unwrap_or(first);
    let text = format!("{prompts}{replies}");
    let lower = text.to_ascii_lowercase();
    Some(Doc { id, path: path.to_path_buf(), mtime, size, title: one_line(&title, 120), cwd, branch, text, lower })
}

/// Append `text` (one line per message) up to `cap`.
fn keep(buf: &mut String, text: &str, cap: usize) {
    if buf.len() < cap && !text.trim().is_empty() {
        buf.push_str(truncate(text, cap - buf.len()));
        buf.push('\n');
    }
}

/// `file`: its path, modified time and size; `said`: prompts, then replies.
fn doc(id: String, file: (&Path, SystemTime, u64), cwd: Option<PathBuf>, branch: Option<String>, said: (String, String)) -> Option<Doc> {
    let ((path, mtime, size), (prompts, replies)) = (file, said);
    if prompts.is_empty() && replies.is_empty() {
        return None;
    }
    let title = one_line(prompts.lines().next().unwrap_or_default(), 120);
    let text = format!("{prompts}{replies}");
    let lower = text.to_ascii_lowercase();
    Some(Doc { id, path: path.to_path_buf(), mtime, size, title, cwd, branch, text, lower })
}

/// Codex and Gemini CLI conversations that ran under one of `roots` (velmux's
/// repos, slots and sessions), reusing `old` docs whose file is unchanged.
pub fn scan_others(roots: &[PathBuf], old: &[Doc]) -> Vec<Doc> {
    let old: HashMap<&Path, &Doc> = old.iter().map(|d| (d.path.as_path(), d)).collect();
    let within = |cwd: &Path| roots.iter().any(|r| cwd.starts_with(r));
    let mut docs = Vec::new();
    let mut reuse_or = |path: &Path, index: &dyn Fn(SystemTime, u64) -> Option<Doc>| {
        let Ok(meta) = std::fs::metadata(path) else { return };
        let (mtime, size) = (meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len());
        match old.get(path) {
            Some(d) if d.mtime == mtime && d.size == size => docs.push((*d).clone()),
            _ => docs.extend(index(mtime, size)),
        }
    };
    for (_, path) in crate::agents::codex_rollouts(&crate::agents::codex_home(), None) {
        reuse_or(&path, &|mtime, size| index_codex(&path, mtime, size, &within));
    }
    for project in std::fs::read_dir(crate::agents::gemini_home().join("tmp")).into_iter().flatten().flatten() {
        let Some(root) = std::fs::read_to_string(project.path().join(".project_root")).ok().map(|r| PathBuf::from(r.trim())) else { continue };
        if !within(&root) {
            continue;
        }
        for f in std::fs::read_dir(project.path().join("chats")).into_iter().flatten().flatten() {
            let path = f.path();
            if path.extension().is_some_and(|x| x == "jsonl") {
                reuse_or(&path, &|mtime, size| index_gemini(&path, mtime, size, &root));
            }
        }
    }
    docs
}

/// A Codex rollout: `session_meta` first (id, cwd, git branch), then events;
/// your messages are `user_message`s and its replies `agent_message`s.
fn index_codex(path: &Path, mtime: SystemTime, size: u64, within: &dyn Fn(&Path) -> bool) -> Option<Doc> {
    let mut lines = BufReader::new(std::fs::File::open(path).ok()?).lines();
    let meta: serde_json::Value = serde_json::from_str(&lines.next()?.ok()?).ok()?;
    let p = &meta["payload"];
    let cwd = PathBuf::from(p["cwd"].as_str()?);
    if meta["type"] != "session_meta" || !within(&cwd) {
        return None;
    }
    let id = p["id"].as_str().or(p["session_id"].as_str())?;
    let branch = p["git"]["branch"].as_str().map(String::from);
    let (mut prompts, mut replies) = (String::new(), String::new());
    for line in lines.map_while(Result::ok) {
        if !line.contains(r#""type":"event_msg""#) || !(line.contains("user_message") || line.contains("agent_message")) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        let text = v["payload"]["message"].as_str().unwrap_or_default();
        match v["payload"]["type"].as_str() {
            Some("user_message") => keep(&mut prompts, text, PROMPT_CAP),
            Some("agent_message") => keep(&mut replies, text, REPLY_CAP),
            _ => {}
        }
    }
    doc(format!("codex:{id}"), (path, mtime, size), Some(cwd), branch, (prompts, replies))
}

/// A Gemini CLI chat: metadata first (`sessionId`), then `user` / `gemini`
/// messages; it ran in the project folder its directory belongs to.
fn index_gemini(path: &Path, mtime: SystemTime, size: u64, root: &Path) -> Option<Doc> {
    let mut lines = BufReader::new(std::fs::File::open(path).ok()?).lines();
    let meta: serde_json::Value = serde_json::from_str(&lines.next()?.ok()?).ok()?;
    if meta["kind"] == "subagent" {
        return None;
    }
    let id = meta["sessionId"].as_str()?.to_string();
    let (mut prompts, mut replies) = (String::new(), String::new());
    for line in lines.map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        let text = match &v["content"] {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
            _ => continue,
        };
        match v["type"].as_str() {
            Some("user") => keep(&mut prompts, &text, PROMPT_CAP),
            Some("gemini") => keep(&mut replies, &text, REPLY_CAP),
            _ => {}
        }
    }
    doc(format!("gemini:{id}"), (path, mtime, size), Some(root.to_path_buf()), None, (prompts, replies))
}

/// A message's text: a plain string, or its `text` parts.
fn message_text(v: &serde_json::Value) -> String {
    let content = &v["message"]["content"];
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&flat, max).to_string()
}

pub struct Hit<'a> {
    pub doc: &'a Doc,
    /// Text around the first match (empty for an empty query).
    pub snippet: String,
}

/// Conversations containing every word of `query` (case-insensitive), title
/// matches first, then newest; everything, newest first, for an empty query.
pub fn search<'a>(docs: &'a [Doc], query: &str, limit: usize) -> Vec<Hit<'a>> {
    let words: Vec<String> = query.split_whitespace().map(|w| w.to_ascii_lowercase()).collect();
    let mut hits: Vec<(u8, usize, Hit)> = Vec::new();
    for (i, doc) in docs.iter().enumerate() {
        if words.is_empty() {
            let first = doc.text.lines().next().unwrap_or_default();
            hits.push((0, i, Hit { doc, snippet: one_line(first, 200) }));
            continue;
        }
        let title = doc.title.to_ascii_lowercase();
        let mut first = None;
        let mut all = true;
        let mut in_title = 0;
        for w in &words {
            if title.contains(w.as_str()) {
                in_title += 1;
                continue;
            }
            match doc.lower.find(w.as_str()) {
                Some(at) => first = Some(first.map_or(at, |f: usize| f.min(at))),
                None => {
                    all = false;
                    break;
                }
            }
        }
        if !all {
            continue;
        }
        let snippet = match first.or_else(|| doc.lower.find(words[0].as_str())) {
            Some(at) => snippet(&doc.text, at, 90),
            None => String::new(),
        };
        // Lower rank sorts first: all words in the title, some, none.
        let rank = if in_title == words.len() { 0 } else if in_title > 0 { 1 } else { 2 };
        hits.push((rank, i, Hit { doc, snippet }));
    }
    hits.sort_by_key(|(rank, i, _)| (*rank, *i));
    hits.into_iter().take(limit).map(|(_, _, h)| h).collect()
}

/// About `width` characters of `text` on one line, starting a little before `at`.
fn snippet(text: &str, at: usize, width: usize) -> String {
    let mut start = at.saturating_sub(24);
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let tail: String = text[start..].chars().take(width).collect();
    let flat = one_line(&tail, width * 4);
    if start > 0 {
        format!("…{flat}")
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, id: &str, lines: &[serde_json::Value]) {
        let body: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        std::fs::write(dir.join(format!("{id}.jsonl")), body.join("\n")).unwrap();
    }

    fn user(text: &str) -> serde_json::Value {
        serde_json::json!({"type":"user","message":{"role":"user","content":text},"cwd":"/w/app","gitBranch":"feat/x"})
    }

    fn reply(text: &str) -> serde_json::Value {
        serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":text},{"type":"tool_use","name":"Bash","input":{"command":"secret-tool-input"}}]}})
    }

    #[test]
    fn indexes_codex_and_gemini_transcripts() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("ws/app");
        let within = |c: &Path| c.starts_with(&repo);
        let rollout = tmp.path().join("rollout.jsonl");
        let lines = [
            serde_json::json!({"type":"session_meta","payload":{"id":"019f-a","cwd":repo,"git":{"branch":"feat/x"}}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"add rate limits"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"secret-tool-input"}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"agent_message","message":"Added a token bucket."}}),
        ];
        std::fs::write(&rollout, lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n")).unwrap();
        let d = index_codex(&rollout, SystemTime::now(), 1, &within).unwrap();
        assert_eq!((d.id.as_str(), d.title.as_str(), d.branch.as_deref()), ("codex:019f-a", "add rate limits", Some("feat/x")));
        assert!(d.text.contains("token bucket") && !d.text.contains("secret-tool-input"));
        assert!(index_codex(&rollout, SystemTime::now(), 1, &|_: &Path| false).is_none(), "not velmux's");

        let chat = tmp.path().join("session.jsonl");
        let lines = [
            serde_json::json!({"sessionId":"g-1","kind":"main","startTime":"x"}),
            serde_json::json!({"id":"1","type":"user","content":[{"text":"why is search flaky"}]}),
            serde_json::json!({"id":"2","type":"gemini","content":"It re-mounts on each keystroke."}),
            serde_json::json!({"$set":{"lastUpdated":"y"}}),
        ];
        std::fs::write(&chat, lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n")).unwrap();
        let g = index_gemini(&chat, SystemTime::now(), 1, &repo).unwrap();
        assert_eq!((g.id.as_str(), g.title.as_str()), ("gemini:g-1", "why is search flaky"));
        assert!(g.text.contains("re-mounts"));
    }

    #[test]
    fn indexes_prompts_and_replies_and_finds_them() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        write(d, "a", &[user("fix the login redirect"), reply("The redirect loops because the cookie is missing."), serde_json::json!({"type":"ai-title","aiTitle":"Login redirect loop"})]);
        write(d, "b", &[user("<command-name>/clear</command-name>"), user("add dark mode"), reply("Added a theme toggle.")]);
        write(d, "c", &[serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","content":"redirect"}]}})]);
        let docs = scan(&[d.to_path_buf()], Vec::new());
        assert_eq!(docs.len(), 2, "c has no conversation");
        let a = docs.iter().find(|x| x.id == "a").unwrap();
        assert_eq!(a.title, "Login redirect loop");
        assert_eq!((a.cwd.as_deref(), a.branch.as_deref()), (Some(Path::new("/w/app")), Some("feat/x")));
        assert!(!a.text.contains("secret-tool-input"), "tool calls aren't indexed");
        let b = docs.iter().find(|x| x.id == "b").unwrap();
        assert_eq!(b.title, "add dark mode", "first real prompt");

        let hits = search(&docs, "COOKIE missing", 10);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("cookie is missing"), "{}", hits[0].snippet);
        assert!(search(&docs, "cookie toggle", 10).is_empty(), "every word must match");
        assert_eq!(search(&docs, "", 10).len(), 2);
        // Unchanged files are reused, changed ones re-read.
        write(d, "b", &[user("add dark mode"), reply("Added a theme toggle."), serde_json::json!({"type":"custom-title","customTitle":"Dark mode"})]);
        let again = scan(&[d.to_path_buf()], docs);
        assert_eq!(again.iter().find(|x| x.id == "b").unwrap().title, "Dark mode");
    }
}
