//! Update checks against velmux.com/api/latest, and applying an update the
//! way velmux was installed: Homebrew upgrades itself, a downloaded binary is
//! swapped in place (the running server then restarts into it), cargo and dev
//! builds are pointed at their own commands. Fetches go through `curl`, which
//! macOS and Linux ship, so velmux needs no HTTP stack.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use velmux_pool::sys::Canonical as _;

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// What `/api/latest` says about the newest release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Latest {
    pub version: String,
    /// Release page.
    pub url: String,
    /// The download for this machine's platform, when there is one.
    pub download: Option<String>,
}

fn endpoint() -> String {
    std::env::var("VELMUX_UPDATE_URL").unwrap_or_else(|_| "https://velmux.com/api/latest".into())
}

/// This build's release target (the platform's archive name).
pub fn target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

fn curl(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", "20", "-A", &format!("velmux/{CURRENT}")])
        .args(args)
        .output()
        .context("running curl")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

pub fn parse(json: &str) -> Result<Latest> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing the update check")?;
    let version = v["version"].as_str().context("no version in the update check")?.to_string();
    Ok(Latest {
        version,
        url: v["url"].as_str().unwrap_or_default().to_string(),
        download: target().and_then(|t| v["downloads"][t].as_str()).map(String::from),
    })
}

/// Ask velmux.com for the newest release.
pub fn fetch() -> Result<Latest> {
    parse(&String::from_utf8_lossy(&curl(&[&endpoint()])?))
}

/// `a` is a later version than `b` (`x.y.z`, extra parts compared too).
pub fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| v.trim_start_matches('v').split(['.', '-']).map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parts(a) > parts(b)
}

/// How this velmux got here, which decides how it updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    Homebrew,
    Cargo,
    /// A downloaded binary (the install script, a release archive) at this path.
    Binary(PathBuf),
    /// Built from source (`target/debug|release`): left alone.
    Dev,
}

impl Install {
    pub fn detect() -> Install {
        let exe = crate::client::launched_exe().and_then(|p| p.canonical().ok()).unwrap_or_default();
        Install::of(&exe)
    }

    pub fn of(exe: &Path) -> Install {
        let s = exe.to_string_lossy().replace('\\', "/");
        if s.contains("/Cellar/") || s.contains("/homebrew/") || s.contains("/linuxbrew/") {
            Install::Homebrew
        } else if s.contains("/.cargo/bin/") {
            Install::Cargo
        } else if s.contains("/target/debug/") || s.contains("/target/release/") || s.contains("/target/") && s.contains("/release/") {
            Install::Dev
        } else {
            Install::Binary(exe.to_path_buf())
        }
    }

    /// A command that updates this install, to run in a pane (Homebrew,
    /// cargo); `None` when velmux updates the binary itself or can't.
    pub fn command(&self) -> Option<&'static str> {
        match self {
            Install::Homebrew => Some("brew update --quiet && brew upgrade vincentvella/tap/velmux"),
            Install::Cargo => Some("cargo install --git https://github.com/vincentvella/velmux velmux-cli"),
            _ => None,
        }
    }
}

/// Download `latest` for this platform, check it against its published
/// checksum, and swap it in for the binary at `path` (atomically, beside it).
pub fn apply(latest: &Latest, path: &Path) -> Result<()> {
    let url = latest.download.as_deref().context("no download for this platform")?;
    let dir = path.parent().context("the binary has no folder")?;
    let tmp = tempdir(dir)?;
    let archive = tmp.join("velmux.tar.gz");
    std::fs::write(&archive, curl(&[url])?)?;
    let sum = String::from_utf8_lossy(&curl(&[&format!("{url}.sha256")])?).split_whitespace().next().unwrap_or_default().to_string();
    if sum.is_empty() || sha256(&archive)? != sum {
        let _ = std::fs::remove_dir_all(&tmp);
        bail!("the download doesn't match its checksum; nothing was changed");
    }
    let ok = Command::new("tar").arg("-xzf").arg(&archive).arg("-C").arg(&tmp).status().is_ok_and(|s| s.success());
    let new = std::fs::read_dir(&tmp)?.flatten().map(|e| e.path().join(if cfg!(windows) { "velmux.exe" } else { "velmux" })).find(|p| p.is_file());
    let result = match (ok, new) {
        (true, Some(new)) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
            }
            // Windows won't replace a running program's file, but will rename
            // it: move it aside first (removed by the next update).
            if cfg!(windows) {
                let aside = path.with_extension("old.exe");
                let _ = std::fs::remove_file(&aside);
                std::fs::rename(path, &aside).with_context(|| format!("moving {} aside", path.display()))?;
            }
            // Same folder, so this is an atomic replace; a running velmux keeps
            // its old file open until it restarts into the new one.
            std::fs::rename(&new, path).with_context(|| format!("replacing {}", path.display())).inspect_err(|_| {
                if cfg!(windows) {
                    let _ = std::fs::rename(path.with_extension("old.exe"), path);
                }
            })
        }
        _ => Err(anyhow::anyhow!("couldn't unpack the download")),
    };
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

fn tempdir(near: &Path) -> Result<PathBuf> {
    let dir = near.join(format!(".velmux-update-{}", std::process::id()));
    std::fs::create_dir_all(&dir).with_context(|| format!("can't write in {} (is it yours?)", near.display()))?;
    Ok(dir)
}

fn sha256(file: &Path) -> Result<String> {
    if cfg!(windows) {
        // Its second line is the hash.
        let out = Command::new("certutil").arg("-hashfile").arg(file).arg("SHA256").output()?;
        return Ok(String::from_utf8_lossy(&out.stdout).lines().nth(1).unwrap_or_default().replace(' ', "").to_lowercase());
    }
    let out = Command::new("shasum").args(["-a", "256"]).arg(file).output().or_else(|_| Command::new("sha256sum").arg(file).output())?;
    Ok(String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap_or_default().to_string())
}

/// The newest version you've already been told about, so each new release
/// is announced once.
pub fn announced() -> Option<String> {
    std::fs::read_to_string(crate::paths::state_dir().ok()?.join("update-announced")).ok().map(|s| s.trim().to_string())
}

pub fn mark_announced(version: &str) {
    if let Ok(dir) = crate::paths::state_dir() {
        let _ = std::fs::write(dir.join("update-announced"), version);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("0.3.0", "0.2.0"));
        assert!(newer("0.10.0", "0.9.9"), "numerically, not as text");
        assert!(newer("v1.0.0", "0.99.0"));
        assert!(!newer("0.2.0", "0.2.0"));
        assert!(!newer("0.1.9", "0.2.0"));
    }

    #[test]
    fn reads_the_api_and_picks_this_platform() {
        let latest = parse(r#"{"version":"0.3.0","tag":"v0.3.0","url":"https://x/r","downloads":{"aarch64-apple-darwin":"a","x86_64-apple-darwin":"b","x86_64-unknown-linux-musl":"c","aarch64-unknown-linux-musl":"d","x86_64-pc-windows-msvc":"e"}}"#).unwrap();
        assert_eq!(latest.version, "0.3.0");
        assert_eq!(latest.download.is_some(), target().is_some());
    }

    #[test]
    fn tells_installs_apart() {
        assert_eq!(Install::of(Path::new("/opt/homebrew/Cellar/velmux/0.2.0/bin/velmux")), Install::Homebrew);
        assert_eq!(Install::of(Path::new("/home/me/.linuxbrew/Cellar/velmux/0.2.0/bin/velmux")), Install::Homebrew);
        assert_eq!(Install::of(Path::new("/Users/me/.cargo/bin/velmux")), Install::Cargo);
        assert_eq!(Install::of(Path::new("/Users/me/src/velmux/target/release/velmux")), Install::Dev);
        assert_eq!(Install::of(Path::new("/Users/me/.local/bin/velmux")), Install::Binary("/Users/me/.local/bin/velmux".into()));
    }
}
