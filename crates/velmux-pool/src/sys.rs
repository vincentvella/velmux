//! What differs between Unix and Windows, in one place: the home folder,
//! links, quoting for the shell panes run in, and starting a process with no
//! terminal of its own.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Your home folder: `$HOME` when set (as on Unix, and as tests set it),
/// else the platform's own (`%USERPROFILE%` on Windows).
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from).or_else(std::env::home_dir)
}

/// Quote `s` as one word for the shell panes run their commands in: POSIX
/// `'…'` on Unix, PowerShell's `'…'` (a `'` doubled) on Windows.
pub fn quote(s: &str) -> String {
    if cfg!(windows) {
        format!("'{}'", s.replace('\'', "''"))
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// [`Path::canonicalize`] without Windows' `\\?\` prefix, which git and
/// most programs given the path don't understand.
pub trait Canonical {
    fn canonical(&self) -> std::io::Result<PathBuf>;
}

impl Canonical for Path {
    fn canonical(&self) -> std::io::Result<PathBuf> {
        let path = self.canonicalize()?;
        #[cfg(windows)]
        if let Some(plain) = path.to_str().and_then(|s| s.strip_prefix(r"\\?\")).filter(|s| !s.starts_with(r"UNC\")) {
            return Ok(PathBuf::from(plain));
        }
        Ok(path)
    }
}

/// Whether `path` is a program you can run: executable bits on Unix, a file
/// on Windows (where `PATHEXT` decides; see [`find_program`]).
pub fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// `prog` in `dir` as the OS would run it: as is on Unix; on Windows also with
/// each `PATHEXT` extension (`claude` → `claude.exe`, `claude.cmd`…).
pub fn find_program(dir: &Path, prog: &str) -> Option<PathBuf> {
    let exact = dir.join(prog);
    if cfg!(not(windows)) || Path::new(prog).extension().is_some() {
        return is_executable(&exact).then_some(exact);
    }
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    exts.split(';').filter(|e| !e.is_empty()).map(|e| dir.join(format!("{prog}{}", e.to_lowercase()))).find(|p| is_executable(p))
}

/// PowerShell, the shell velmux runs commands in on Windows: PowerShell 7
/// (`pwsh`) when it's installed, else the Windows PowerShell every Windows has.
pub fn powershell() -> String {
    if on_path("pwsh").is_some() { "pwsh".into() } else { "powershell".into() }
}

/// `prog` as found on `PATH`.
pub fn on_path(prog: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|d| find_program(&d, prog))
}

/// A POSIX `sh` for repo hooks (`post_claim`, `install_cmd`…), which are
/// shell commands shared by everyone working on the repo. On Windows that's
/// the one Git for Windows ships (velmux needs git anyway): on `PATH`, or
/// beside `git.exe` (`<Git>\cmd\git.exe` → `<Git>\bin\sh.exe`).
pub fn posix_shell() -> Option<PathBuf> {
    if cfg!(not(windows)) {
        return Some(PathBuf::from("sh"));
    }
    on_path("sh").or_else(|| {
        let git = on_path("git")?;
        let root = git.parent()?.parent()?;
        [root.join("bin").join("sh.exe"), root.join("usr").join("bin").join("sh.exe")].into_iter().find(|p| p.is_file())
    })
}

/// Link `link` to `target`. On Windows, where symlinks need Developer Mode
/// or admin rights, a folder falls back to a junction and a file to a hard
/// link (the same file under both names).
pub fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        if target.is_dir() {
            std::os::windows::fs::symlink_dir(target, link).or_else(|_| junction::create(target, link))
        } else {
            std::os::windows::fs::symlink_file(target, link).or_else(|_| std::fs::hard_link(target, link))
        }
    }
}

/// Where the link at `link` points, comparable with the path it was made
/// from (Windows reports junction targets with a `\\?\` prefix).
pub fn read_link(link: &Path) -> std::io::Result<PathBuf> {
    let target = std::fs::read_link(link)?;
    #[cfg(windows)]
    if let Some(plain) = target.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        return Ok(PathBuf::from(plain));
    }
    Ok(target)
}

/// Remove the link at `path` (on Windows a link to a folder is removed as one).
pub fn remove_link(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path).or_else(|e| if cfg!(windows) { std::fs::remove_dir(path) } else { Err(e) })
}

/// Start `cmd` with no terminal: a session of its own on Unix (nothing it runs
/// can take the terminal you're in for job control); on Windows, a hidden
/// console of its own and its own process group, so ctrl+c in your terminal
/// and closing it leave it alone.
pub fn detach(cmd: &mut Command) {
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_for_the_pane_shell() {
        if cfg!(windows) {
            assert_eq!(quote("don't"), "'don''t'");
        } else {
            assert_eq!(quote("don't"), r"'don'\''t'");
        }
    }

    #[test]
    fn links_files_and_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, file) = (tmp.path().join("dir"), tmp.path().join("file"));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(&file, "x").unwrap();
        symlink(&dir, &tmp.path().join("dir-link")).unwrap();
        symlink(&file, &tmp.path().join("file-link")).unwrap();
        assert!(tmp.path().join("dir-link").is_dir());
        assert_eq!(std::fs::read_to_string(tmp.path().join("file-link")).unwrap(), "x");
        remove_link(&tmp.path().join("dir-link")).unwrap();
        assert!(dir.is_dir(), "removing the link keeps its target");
    }
}
