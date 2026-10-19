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

/// The session a pane in `cwd` should resume: the newest transcript recorded in
/// `cwd` that is either `known` or was written since `since`, skipping sessions
/// that belong to other panes (`exclude`). Catches `/clear`, which starts a new
/// session without telling us.
pub fn latest_session(
    claude_dir: &Path,
    cwd: &Path,
    known: Option<&str>,
    since: SystemTime,
    exclude: &[String],
) -> Option<String> {
    let dir = project_dir(claude_dir, cwd);
    let mut best: Option<(SystemTime, String)> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
        if exclude.contains(&id) {
            continue;
        }
        let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else { continue };
        let is_known = known == Some(id.as_str());
        if !is_known && (mtime < since || transcript_cwd(&path).as_deref() != Some(cwd)) {
            continue;
        }
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, id));
        }
    }
    best.map(|(_, id)| id)
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
    use std::time::Duration;

    fn transcript(dir: &Path, id: &str, cwd: &str) {
        let body = format!(
            "{{\"type\":\"mode\",\"sessionId\":\"{id}\"}}\n{{\"type\":\"user\",\"cwd\":\"{cwd}\"}}\n"
        );
        std::fs::write(dir.join(format!("{id}.jsonl")), body).unwrap();
    }

    #[test]
    fn latest_session_prefers_newest_in_same_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path();
        let slot = Path::new("/w/.pools/app/slot-1");
        let dir = project_dir(claude, slot);
        std::fs::create_dir_all(&dir).unwrap();
        let start = SystemTime::now() - Duration::from_secs(5);

        transcript(&dir, "aaa", "/w/.pools/app/slot-1");
        assert_eq!(latest_session(claude, slot, Some("aaa"), start, &[]).as_deref(), Some("aaa"));

        // After /clear: a newer transcript for the same slot wins over the known id…
        std::thread::sleep(Duration::from_millis(20));
        transcript(&dir, "bbb", "/w/.pools/app/slot-1");
        assert_eq!(latest_session(claude, slot, Some("aaa"), start, &[]).as_deref(), Some("bbb"));
        // …unless another pane owns it, or it ran in a different slot.
        let others = vec!["bbb".to_string()];
        assert_eq!(latest_session(claude, slot, Some("aaa"), start, &others).as_deref(), Some("aaa"));
        std::thread::sleep(Duration::from_millis(20));
        transcript(&dir, "ccc", "/w/.pools/app/slot-2");
        assert_eq!(latest_session(claude, slot, Some("aaa"), start, &others).as_deref(), Some("aaa"));

        // Unknown and nothing recent: start fresh.
        let later = SystemTime::now() + Duration::from_secs(60);
        assert_eq!(latest_session(claude, slot, None, later, &[]), None);
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
