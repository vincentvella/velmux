use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Free,
    InUse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slot {
    pub id: String,
    pub path: PathBuf,
    pub status: Status,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub claimed_by: Option<String>,
    #[serde(default)]
    pub claimed_at: Option<u64>,
    #[serde(default)]
    pub released_at: Option<u64>,
    /// Hash of the lockfile at the last successful install; lets claim skip installs.
    #[serde(default)]
    pub deps_hash: Option<String>,
}

/// A branch handed back to the pool with work on it (stashed changes, or
/// commits not on the base), until it's claimed again or discarded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Parked {
    pub branch: String,
    /// The slot it was released from.
    pub slot: String,
    pub parked_at: u64,
    /// Claude sessions that were working on it, most relevant first.
    #[serde(default)]
    pub sessions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub base: PathBuf,
    pub slots: Vec<Slot>,
    #[serde(default)]
    pub parked: Vec<Parked>,
}

impl State {
    pub fn slot(&self, id: &str) -> Option<&Slot> {
        self.slots.iter().find(|s| s.id == id)
    }

    pub fn slot_mut(&mut self, id: &str) -> Option<&mut Slot> {
        self.slots.iter_mut().find(|s| s.id == id)
    }
}

/// `pool.json` guarded by an exclusive `flock` on `pool.lock`, so the CLI, the
/// TUI daemon and agents can all mutate the pool concurrently.
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Store {
        Store { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Load (or create) state under the lock, let `f` mutate it, then persist atomically.
    pub fn with<T>(&self, base: &Path, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let lock = self.lock()?;
        let mut state = self.read()?.unwrap_or_else(|| State {
            version: 1,
            base: base.to_path_buf(),
            slots: Vec::new(),
            parked: Vec::new(),
        });
        let out = f(&mut state)?;
        self.write(&state)?;
        drop(lock);
        Ok(out)
    }

    /// Read-only snapshot; still takes the lock so it never sees a half-written file.
    pub fn snapshot(&self) -> Result<Option<State>> {
        if !self.dir.exists() {
            return Ok(None);
        }
        let _lock = self.lock()?;
        self.read()
    }

    fn lock(&self) -> Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("pool.lock"))?;
        file.lock().context("locking pool")?;
        Ok(file)
    }

    fn read(&self) -> Result<Option<State>> {
        let path = self.dir.join("pool.json");
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn write(&self, state: &State) -> Result<()> {
        let tmp = self.dir.join("pool.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
        std::fs::rename(&tmp, self.dir.join("pool.json"))?;
        Ok(())
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
