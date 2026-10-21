//! Searching Claude conversations: an index of each transcript's title, your
//! prompts and Claude's replies (tool calls and results left out), rebuilt in
//! the background for files that changed.

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
