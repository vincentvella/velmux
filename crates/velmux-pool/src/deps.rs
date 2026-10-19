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
    let out = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .output()
        .with_context(|| format!("running `{command}`"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!("`{command}` failed in {}:\n{}", dir.display(), tail.join("\n"));
    }
    Ok(())
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
