//! Group Claude Code transcripts from every slot under the base checkout.
//!
//! Claude Code already keys *memory* to the git common dir, but keys
//! *transcripts* by cwd (`<claude_dir>/projects/<sanitized cwd>`). Symlinking each
//! slot's project dir to base's makes `claude -c` / `--resume` see one history.
//! Resuming adopts the current cwd, so callers must resume inside the slot the
//! session ran in.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

/// Mirrors Claude Code's project-dir naming: every non-alphanumeric char → `-`.
pub fn sanitize(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

pub fn project_dir(claude_dir: &Path, path: &Path) -> PathBuf {
    claude_dir.join("projects").join(sanitize(path))
}

/// Point `slot`'s project dir at `base`'s, folding in any transcripts that a slot
/// accumulated before it was linked.
pub fn link(claude_dir: &Path, base: &Path, slot: &Path) -> Result<()> {
    let target = project_dir(claude_dir, base);
    let link = project_dir(claude_dir, slot);
    std::fs::create_dir_all(&target).with_context(|| format!("creating {}", target.display()))?;

    match std::fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if std::fs::read_link(&link)? == target {
                return Ok(());
            }
            std::fs::remove_file(&link)?;
        }
        Ok(meta) if meta.is_dir() => {
            for entry in std::fs::read_dir(&link)? {
                let entry = entry?;
                let dest = target.join(entry.file_name());
                if !dest.exists() {
                    std::fs::rename(entry.path(), dest)?;
                }
            }
            std::fs::remove_dir_all(&link)?;
        }
        Ok(_) => std::fs::remove_file(&link)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    std::os::unix::fs::symlink(&target, &link)
        .with_context(|| format!("linking {}", link.display()))?;
    Ok(())
}

/// The session a running Claude process is on, from the record Claude keeps
/// for every live instance (`<claude_dir>/sessions/<pid>.json`). Exact, works
/// for Claudes started by hand, and needs no guessing from transcripts.
pub fn running_session(claude_dir: &Path, pid: i32) -> Option<String> {
    let text = std::fs::read_to_string(claude_dir.join("sessions").join(format!("{pid}.json"))).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    (v.get("pid")?.as_i64()? == pid as i64).then(|| v.get("sessionId")?.as_str().map(str::to_string))?
}

/// Whether a transcript for session `id` exists in any project dir (Claude
/// resumes by id from anywhere).
pub fn session_exists(claude_dir: &Path, id: &str) -> bool {
    let Ok(dirs) = std::fs::read_dir(claude_dir.join("projects")) else { return false };
    let file = format!("{id}.jsonl");
    dirs.flatten().any(|d| d.path().join(&file).is_file())
}

/// Session `id`'s transcript, in whichever project dir it's in.
pub fn transcript(claude_dir: &Path, id: &str) -> Option<PathBuf> {
    let file = format!("{id}.jsonl");
    std::fs::read_dir(claude_dir.join("projects")).ok()?.flatten().map(|d| d.path().join(&file)).find(|p| p.is_file())
}

/// The name a conversation was given (`/rename`, `--name`): its transcript's
/// last `custom-title`.
pub fn custom_title(transcript: &Path) -> Option<String> {
    let text = std::fs::read_to_string(transcript).ok()?;
    text.lines().rev().filter(|l| l.contains("\"custom-title\"")).find_map(|l| {
        let v: serde_json::Value = serde_json::from_str(l).ok()?;
        (v["type"] == "custom-title").then(|| v["customTitle"].as_str().map(String::from))?
    })
}

/// The title Claude gave a conversation itself (its transcript's last
/// `ai-title`), from the end of the transcript (Claude repeats it as it goes).
pub fn ai_title(transcript: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(transcript).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(256 * 1024))).ok()?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).ok()?;
    let tail = String::from_utf8_lossy(&tail);
    tail.lines().rev().filter(|l| l.contains("\"ai-title\"")).find_map(|l| {
        let v: serde_json::Value = serde_json::from_str(l).ok()?;
        (v["type"] == "ai-title").then(|| v["aiTitle"].as_str().filter(|t| !t.trim().is_empty()).map(|t| t.trim().to_string()))?
    })
}

/// Name a conversation, the way `/rename` does: a `custom-title` (the
/// `/resume` picker) and an `agent-name` (the prompt box) at the end of its
/// transcript. A running Claude shows it once it's resumed.
pub fn set_title(transcript: &Path, id: &str, title: &str) -> Result<()> {
    use std::io::Write;
    let lines = format!(
        "{}\n{}\n",
        serde_json::json!({ "type": "custom-title", "customTitle": title, "sessionId": id }),
        serde_json::json!({ "type": "agent-name", "agentName": title, "sessionId": id })
    );
    let mut f = std::fs::OpenOptions::new().append(true).open(transcript)?;
    // One write, so it can't interleave with Claude's own appends.
    f.write_all(lines.as_bytes())?;
    Ok(())
}

/// A conversation found on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub id: String,
    /// Where it ran (the transcript's recorded cwd).
    pub cwd: PathBuf,
    pub modified: SystemTime,
    /// First thing the user typed, as a title.
    pub title: Option<String>,
}

/// Where [`newest_under`] looks.
pub struct Search<'a> {
    pub root: &'a Path,
    /// Also folders under `root` (repos), or exactly `root` (plain folders like `~`,
    /// where "under" would mean every conversation you've had).
    pub subfolders: bool,
    /// Only transcripts last written before this, so conversations that are open
    /// right now are never resumed a second time.
    pub before: SystemTime,
    pub exclude: &'a [String],
}

/// The newest conversation that ran in `root` (or under it). Project dirs are
/// matched by sanitized-name prefix, then each transcript's recorded cwd must
/// really be inside `root` (so `/w/app-old` never counts for `/w/app`).
/// Sidechain and empty transcripts are ignored, as are sessions in `exclude`.
pub fn newest_under(claude_dir: &Path, q: &Search) -> Option<Found> {
    let root = q.root;
    let name = sanitize(root);
    let sub_prefix = format!("{name}-");
    let mut best: Option<Found> = None;
    for pdir in std::fs::read_dir(claude_dir.join("projects")).ok()?.flatten() {
        let dir_name = pdir.file_name().to_string_lossy().into_owned();
        if dir_name != name && !(q.subfolders && dir_name.starts_with(&sub_prefix)) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(pdir.path()) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(id) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
            if path.extension().is_none_or(|e| e != "jsonl") || id.starts_with("agent-") || q.exclude.contains(&id) {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else { continue };
            if modified >= q.before || best.as_ref().is_some_and(|b| b.modified >= modified) {
                continue;
            }
            let inside = |c: &PathBuf| if q.subfolders { c.starts_with(root) } else { c == root };
            let Some(cwd) = transcript_cwd(&path).filter(inside) else { continue };
            let Some(title) = transcript_title(&path) else { continue };
            best = Some(Found { id, cwd, modified, title: Some(title) });
        }
    }
    best
}

/// The first prompt the user typed (skipping command/system wrappers).
fn transcript_title(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    BufReader::new(file).lines().take(400).map_while(Result::ok).find_map(|line| {
        let v: serde_json::Value = serde_json::from_str(&line).ok()?;
        if v.get("type")?.as_str()? != "user" {
            return None;
        }
        let content = v.get("message")?.get("content")?;
        let text = match content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(parts) => parts
                .iter()
                .find(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))?
                .get("text")?
                .as_str()?
                .to_string(),
            _ => return None,
        };
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        (!text.is_empty() && !text.starts_with('<')).then_some(text)
    })
}

/// The `cwd` a transcript was recorded in (from its first entries that carry one).
fn transcript_cwd(path: &Path) -> Option<PathBuf> {
    let file = std::fs::File::open(path).ok()?;
    BufReader::new(file).lines().take(200).map_while(Result::ok).find_map(|line| {
        let v: serde_json::Value = serde_json::from_str(&line).ok()?;
        v.get("cwd")?.as_str().map(PathBuf::from)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_a_conversation_like_rename_does() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path().join("s1.jsonl");
        std::fs::write(&t, "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n").unwrap();
        assert_eq!(custom_title(&t), None);
        set_title(&t, "s1", "feat/checkout").unwrap();
        assert_eq!(custom_title(&t).as_deref(), Some("feat/checkout"));
        assert!(std::fs::read_to_string(&t).unwrap().contains("\"agentName\":\"feat/checkout\""));
        set_title(&t, "s1", "renamed").unwrap();
        assert_eq!(custom_title(&t).as_deref(), Some("renamed"), "the last one counts");
        // Claude's own title, separately.
        assert_eq!(ai_title(&t), None);
        let mut f = std::fs::OpenOptions::new().append(true).open(&t).unwrap();
        std::io::Write::write_all(&mut f, b"{\"type\":\"ai-title\",\"aiTitle\":\"Fix the login flow\",\"sessionId\":\"s1\"}\n").unwrap();
        assert_eq!(ai_title(&t).as_deref(), Some("Fix the login flow"));
    }

    #[test]
    fn newest_under_finds_conversations_in_subfolders() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path();
        let root = Path::new("/w/app");
        let sub = Path::new("/w/app/packages/web");
        let write = |cwd: &Path, id: &str, body: &str| {
            let dir = project_dir(claude, cwd);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{id}.jsonl")), body).unwrap();
        };
        let convo = |cwd: &str, prompt: &str| {
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{cwd}\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{prompt}\"}}]}}}}\n"
            )
        };
        write(root, "old", &convo("/w/app", "first   question\\nwith newline"));
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(sub, "new", &convo("/w/app/packages/web", "fix the web build"));
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Newer, but ignored: a sidechain, an empty session, and a wrapper-only prompt.
        write(root, "agent-x", &convo("/w/app", "sidechain"));
        write(root, "empty", "{\"type\":\"mode\"}\n");
        write(root, "cmd", &convo("/w/app", "<command-name>/clear</command-name>"));

        // A sibling repo whose name extends this one's must not match.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(Path::new("/w/app-old"), "sibling", &convo("/w/app-old", "not this repo"));

        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        let q = |root, subfolders, exclude| Search { root, subfolders, before: later, exclude };
        let found = newest_under(claude, &q(root, true, &[])).unwrap();
        assert_eq!((found.id.as_str(), found.cwd.as_path()), ("new", sub));
        assert_eq!(found.title.as_deref(), Some("fix the web build"));
        let skip_new = vec!["new".to_string()];
        let found = newest_under(claude, &q(root, true, &skip_new)).unwrap();
        assert_eq!((found.id.as_str(), found.title.as_deref()), ("old", Some("first question with newline")));
        assert_eq!(newest_under(claude, &q(Path::new("/w/other"), true, &[])), None);
        // Exact-folder search ignores the subfolder conversation.
        assert_eq!(newest_under(claude, &q(root, false, &[])).unwrap().id, "old");
        // Conversations written after `before` (still open) are skipped.
        let earlier = Search { root, subfolders: true, before: SystemTime::UNIX_EPOCH, exclude: &[] };
        assert_eq!(newest_under(claude, &earlier), None);
    }

    #[test]
    fn running_session_reads_claudes_pid_record() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("4242.json"), r#"{"pid":4242,"sessionId":"abc-1","cwd":"/w"}"#).unwrap();
        std::fs::write(dir.join("7.json"), r#"{"pid":8,"sessionId":"stale"}"#).unwrap();
        assert_eq!(running_session(tmp.path(), 4242).as_deref(), Some("abc-1"));
        assert_eq!(running_session(tmp.path(), 7), None, "pid must match the file");
        assert_eq!(running_session(tmp.path(), 99), None);
    }

    #[test]
    fn sanitizes_like_claude_code() {
        assert_eq!(
            sanitize(Path::new("/Users/me/.config/nvim")),
            "-Users-me--config-nvim"
        );
        assert_eq!(
            sanitize(Path::new("/Users/me/Workspace/.pools/app/slot-1")),
            "-Users-me-Workspace--pools-app-slot-1"
        );
    }

    #[test]
    fn link_folds_existing_transcripts_into_base() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path();
        let (base, slot) = (Path::new("/w/app"), Path::new("/w/.pools/app/slot-1"));
        let pre = project_dir(claude, slot);
        std::fs::create_dir_all(&pre).unwrap();
        std::fs::write(pre.join("old.jsonl"), "x").unwrap();

        link(claude, base, slot).unwrap();
        link(claude, base, slot).unwrap(); // idempotent

        let target = project_dir(claude, base);
        assert!(target.join("old.jsonl").is_file());
        assert_eq!(std::fs::read_link(&pre).unwrap(), target);
    }
}
