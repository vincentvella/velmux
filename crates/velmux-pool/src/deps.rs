//! Dependency provisioning per slot.
//!
//! Every slot owns a real `node_modules`; disk stays flat because each package
//! manager is driven through its own dedup path (bun/pnpm clone or hardlink from
//! a global store, yarn berry `hardlinks-global`). npm and yarn classic have no
//! store, so a fresh slot is seeded with an APFS copy-on-write clone of base's
//! tree before the incremental install.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Bun,
    Pnpm,
    YarnBerry,
    YarnClassic,
    Npm,
}

impl Manager {
    pub fn detect(dir: &Path) -> Option<(Manager, &'static str)> {
        let has = |f: &str| dir.join(f).is_file();
        if has("bun.lock") {
            Some((Manager::Bun, "bun.lock"))
        } else if has("bun.lockb") {
            Some((Manager::Bun, "bun.lockb"))
        } else if has("pnpm-lock.yaml") {
            Some((Manager::Pnpm, "pnpm-lock.yaml"))
        } else if has("yarn.lock") && has(".yarnrc.yml") {
            Some((Manager::YarnBerry, "yarn.lock"))
        } else if has("yarn.lock") {
            Some((Manager::YarnClassic, "yarn.lock"))
        } else if has("package-lock.json") {
            Some((Manager::Npm, "package-lock.json"))
        } else {
            None
        }
    }

    fn command(self) -> &'static str {
        match self {
            Manager::Bun => "bun install",
            Manager::Pnpm => "pnpm install",
            Manager::YarnBerry | Manager::YarnClassic => "yarn install",
            Manager::Npm => "npm install",
        }
    }

    fn needs_seed(self) -> bool {
        matches!(self, Manager::Npm | Manager::YarnClassic)
    }
}

pub enum Outcome {
    /// No lockfile and no custom command.
    None,
    /// Lockfile unchanged since the slot's last install.
    Skipped,
    Installed { hash: String, command: String },
}

/// Install deps in `slot` unless the lockfile hash still matches `prev_hash`.
pub fn ensure(
    slot: &Path,
    base: &Path,
    custom_cmd: Option<&str>,
    prev_hash: Option<&str>,
    report: &dyn Fn(&str),
) -> Result<Outcome> {
    let detected = Manager::detect(slot);
    let hash = match detected {
        Some((_, lockfile)) => hash_file(&slot.join(lockfile))?,
        None if custom_cmd.is_some() => "custom".to_string(),
        None => return Ok(Outcome::None),
    };
    let warm = custom_cmd.is_some() || slot.join("node_modules").exists();
    if warm && prev_hash == Some(hash.as_str()) {
        return Ok(Outcome::Skipped);
    }

    if let (None, Some((manager, _))) = (custom_cmd, detected) {
        if manager.needs_seed() && !slot.join("node_modules").exists() {
            seed_from_base(slot, base, report)?;
        }
    }

    let command = custom_cmd
        .map(str::to_string)
        .or_else(|| detected.map(|(m, _)| m.command().to_string()))
        .unwrap();
    report(&format!("installing: {command}"));
    run_shell(slot, &command)?;
    Ok(Outcome::Installed { hash, command })
}

fn seed_from_base(slot: &Path, base: &Path, report: &dyn Fn(&str)) -> Result<()> {
    let src = base.join("node_modules");
    if !src.is_dir() {
        return Ok(());
    }
    report("seeding node_modules from base (copy-on-write)");
    let status = if cfg!(target_os = "macos") {
        Command::new("cp").arg("-cR").arg(&src).arg(slot).status()
    } else {
        Command::new("cp").arg("-a").arg("--reflink=auto").arg(&src).arg(slot).status()
    }
    .context("running cp")?;
    if !status.success() {
        bail!("seeding node_modules from {} failed", src.display());
    }
    Ok(())
}

pub fn run_shell(dir: &Path, command: &str) -> Result<()> {
    run_shell_with(dir, command, &[])
}

/// [`run_shell`] with extra environment variables.
///
/// It runs in your own shell, as an interactive login shell started in `dir`
/// (`$SHELL -l -i -c`), so the node (or ruby, python…) it uses is the one a
/// terminal there would get: nvm / fnm / mise hooks in your shell config see
/// the slot's `.nvmrc` or `.tool-versions`. With a `.nvmrc` and nvm installed,
/// nvm is also switched explicitly, for configs that don't do it on `cd`.
pub fn run_shell_with(dir: &Path, command: &str, env: &[(String, String)]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let (program, args) = user_shell(dir, command);
    let mut cmd = Command::new(&program);
    cmd.args(&args).envs(env.iter().map(|(k, v)| (k, v))).current_dir(dir).stdin(std::process::Stdio::null());
    // A session of its own, with no terminal: an interactive shell takes the
    // terminal it's started from for job control, which would leave a
    // `velmux claim` run in your terminal stopped in the background (SIGTTIN).
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let out = cmd.output().with_context(|| format!("running `{command}`"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!("`{command}` failed in {}:\n{}", dir.display(), tail.join("\n"));
    }
    Ok(())
}

/// The shell and arguments for [`run_shell_with`]: `$SHELL` when it's one
/// we know how to start like that (zsh, bash, fish), else plain `sh -c`.
fn user_shell(dir: &Path, command: &str) -> (String, Vec<String>) {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let name = Path::new(&shell).file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
    let nvm = std::env::var_os("NVM_DIR").map(std::path::PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".nvm")));
    let nvm_sh = nvm.map(|d| d.join("nvm.sh")).filter(|f| f.is_file());
    // nvm is a POSIX shell script: sourced in sh, bash and zsh (not fish).
    let command = match nvm_sh {
        Some(f) if dir.join(".nvmrc").is_file() && name != "fish" => {
            format!(". {} >/dev/null 2>&1 && nvm use >/dev/null 2>&1; {command}", shell_quote(&f.display().to_string()))
        }
        _ => command.to_string(),
    };
    match name.as_str() {
        "zsh" | "bash" | "fish" => (shell, vec!["-l".into(), "-i".into(), "-c".into(), command]),
        _ => ("sh".into(), vec!["-c".into(), command]),
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// FNV-1a 64: stable across Rust versions, unlike `DefaultHasher`.
fn hash_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    Ok(format!("{h:016x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_in_your_shell_with_nvm_switched() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, args) = user_shell(tmp.path(), "yarn install");
        assert_eq!(args.last().unwrap(), "yarn install", "no .nvmrc: as is");
        std::fs::write(tmp.path().join(".nvmrc"), "24\n").unwrap();
        let (_, args) = user_shell(tmp.path(), "yarn install");
        let nvm = std::env::var_os("NVM_DIR").map(std::path::PathBuf::from).unwrap_or_else(|| Path::new(&std::env::var("HOME").unwrap()).join(".nvm"));
        if nvm.join("nvm.sh").is_file() {
            assert!(args.last().unwrap().contains("nvm use") && args.last().unwrap().ends_with("; yarn install"), "{args:?}");
        }
    }

    #[test]
    fn detects_managers_by_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert_eq!(Manager::detect(d), None);
        std::fs::write(d.join("package-lock.json"), "{}").unwrap();
        assert_eq!(Manager::detect(d).unwrap().0, Manager::Npm);
        std::fs::write(d.join("yarn.lock"), "").unwrap();
        assert_eq!(Manager::detect(d).unwrap().0, Manager::YarnClassic);
        std::fs::write(d.join(".yarnrc.yml"), "").unwrap();
        assert_eq!(Manager::detect(d).unwrap().0, Manager::YarnBerry);
        std::fs::write(d.join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(Manager::detect(d).unwrap().0, Manager::Pnpm);
        std::fs::write(d.join("bun.lock"), "").unwrap();
        assert_eq!(Manager::detect(d).unwrap().0, Manager::Bun);
    }
}
