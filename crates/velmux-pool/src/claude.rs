//! Group Claude Code transcripts from every slot under the base checkout.
//!
//! Claude Code already keys *memory* to the git common dir, but keys
//! *transcripts* by cwd (`<claude_dir>/projects/<sanitized cwd>`). Symlinking each
//! slot's project dir to base's makes `claude -c` / `--resume` see one history.
//! Resuming adopts the current cwd, so callers must resume inside the slot the
//! session ran in.

use std::path::{Path, PathBuf};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_like_claude_code() {
        assert_eq!(
            sanitize(Path::new("/Users/vince/.config/nvim")),
            "-Users-vince--config-nvim"
        );
        assert_eq!(
            sanitize(Path::new("/Users/vince/Workspace/.pools/app/slot-1")),
            "-Users-vince-Workspace--pools-app-slot-1"
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
