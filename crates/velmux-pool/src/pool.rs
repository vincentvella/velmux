use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;

use crate::config::{Config, Env};
use crate::deps::{self, Outcome};
use crate::git::{self, git};
use crate::state::{now, Slot, Status, Store};
use crate::claude;

/// A repo's base checkout (the "space") and its pool of worktree slots.
pub struct Pool {
    env: Env,
    pub base: PathBuf,
    pub name: String,
    pub dir: PathBuf,
    pub cfg: Config,
    base_ref: String,
    store: Store,
}

#[derive(Debug, Default, Clone)]
pub struct ClaimOpts {
    pub branch: Option<String>,
    pub by: Option<String>,
    pub fetch: bool,
    pub install: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SlotView {
    pub id: String,
    pub path: PathBuf,
    pub status: Status,
    pub branch: Option<String>,
    pub claimed_by: Option<String>,
    pub claimed_at: Option<u64>,
    pub dirty: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Released {
    pub id: String,
    pub branch: Option<String>,
    /// Stash message, when uncommitted work was parked.
    pub stashed: Option<String>,
    /// Branch created to keep detached commits reachable.
    pub rescued: Option<String>,
}

fn stash_marker(branch: &str) -> String {
    format!("velmux[{branch}]")
}

impl Pool {
    /// Resolve the pool for any path inside a base checkout or one of its slots.
    pub fn discover(env: Env, cwd: &Path) -> Result<Pool> {
        let base = git::base_of(cwd)?;
        let cfg = Config::load(&env, &base)?;
        let name = base
            .file_name()
            .ok_or_else(|| anyhow!("cannot name pool for {}", base.display()))?
            .to_string_lossy()
            .into_owned();
        let root = match &cfg.pool_root {
            Some(root) => root.clone(),
            None => base.parent().unwrap_or(Path::new("/")).join(".pools"),
        };
        let dir = root.join(&name);
        let base_ref = cfg.base_ref.clone().unwrap_or_else(|| git::default_base_ref(&base));
        Ok(Pool {
            store: Store::new(dir.clone()),
            env,
            base,
            name,
            dir,
            cfg,
            base_ref,
        })
    }

    pub fn base_ref(&self) -> &str {
        &self.base_ref
    }

    /// Create missing slots up to the configured count. Idempotent; never shrinks.
    pub fn init(&self, report: &dyn Fn(&str)) -> Result<Vec<SlotView>> {
        std::fs::create_dir_all(&self.dir)?;
        let created = self.store.with(&self.base, |st| {
            let mut created = Vec::new();
            for n in 1..=self.cfg.slots {
                let id = format!("slot-{n}");
                let path = self.dir.join(&id);
                if !path.exists() {
                    report(&format!("{id}: creating worktree at {}", path.display()));
                    let p = path.to_string_lossy();
                    git(&self.base, &["worktree", "add", "--detach", &p, &self.base_ref])?;
                    created.push(id.clone());
                }
                let path = path.canonicalize()?;
                if st.slot(&id).is_none() {
                    st.slots.push(Slot {
                        id: id.clone(),
                        path,
                        status: Status::Free,
                        branch: None,
                        claimed_by: None,
                        claimed_at: None,
                        released_at: None,
                        deps_hash: None,
                    });
                }
            }
            Ok(created)
        })?;

        let slots = self.store.snapshot()?.map(|s| s.slots).unwrap_or_default();
        for slot in slots.iter().filter(|s| s.status == Status::Free) {
            self.link_into(&slot.path)?;
            if self.cfg.install {
                self.install(slot, report)?;
            }
        }
        if !created.is_empty() {
            report(&format!("created {} slot(s)", created.len()));
        }
        self.status()
    }

    /// Reserve the least-recently-used free slot and check out `opts.branch` there.
    pub fn claim(&self, opts: &ClaimOpts, report: &dyn Fn(&str)) -> Result<SlotView> {
        let slot = self.store.with(&self.base, |st| {
            if st.slots.is_empty() {
                bail!("pool for {} has no slots; run `velmux init`", self.name);
            }
            let slot = st
                .slots
                .iter_mut()
                .filter(|s| s.status == Status::Free)
                .min_by_key(|s| s.released_at.unwrap_or(0))
                .ok_or_else(|| anyhow!("all {} slots in {} are in use", self.cfg.slots, self.name))?;
            slot.status = Status::InUse;
            slot.branch = opts.branch.clone();
            slot.claimed_by = opts.by.clone();
            slot.claimed_at = Some(now());
            Ok(slot.clone())
        })?;

        // Slow git/install work runs outside the lock so claims proceed in parallel.
        if let Err(e) = self.prepare(&slot, opts, report) {
            let _ = git(&slot.path, &["switch", "--detach", &self.base_ref]);
            let _ = self.store.with(&self.base, |st| {
                if let Some(s) = st.slot_mut(&slot.id) {
                    s.status = Status::Free;
                    s.branch = None;
                    s.claimed_by = None;
                }
                Ok(())
            });
            return Err(e.context(format!("claiming {}", slot.id)));
        }
        self.view(&slot.id)
    }

    fn prepare(&self, slot: &Slot, opts: &ClaimOpts, report: &dyn Fn(&str)) -> Result<()> {
        let dir = &slot.path;
        if opts.fetch && git::has_remote(dir) {
            report("fetching origin");
            git(dir, &["fetch", "--quiet", "origin"])?;
        }
        match &opts.branch {
            Some(b) if git::local_branch_exists(dir, b) => {
                git(dir, &["switch", "--quiet", b])?;
                if let Some(stash) = git::find_stash(dir, &stash_marker(b)) {
                    report(&format!("restoring parked work from {stash}"));
                    if let Err(e) = git(dir, &["stash", "pop", "--quiet", &stash]) {
                        report(&format!("warning: could not restore {stash}, left in stash list: {e}"));
                    }
                }
            }
            Some(b) if git::remote_branch_exists(dir, b) => {
                let remote = format!("origin/{b}");
                git(dir, &["switch", "--quiet", "--track", "-c", b, &remote])?;
            }
            Some(b) => {
                git(dir, &["switch", "--quiet", "--no-track", "-c", b, &self.base_ref])?;
            }
            None => {
                git(dir, &["switch", "--quiet", "--detach", &self.base_ref])?;
            }
        }
        self.link_into(dir)?;
        if opts.install && self.cfg.install {
            self.install(slot, report)?;
        }
        for hook in &self.cfg.post_claim {
            report(&format!("hook: {hook}"));
            deps::run_shell(dir, hook)?;
        }
        Ok(())
    }

    /// Park the slot's work and return it to the pool clean, keeping deps warm.
    pub fn release(&self, id: &str, force: bool) -> Result<Released> {
        let snapshot = self.store.snapshot()?.ok_or_else(|| anyhow!("pool not initialised"))?;
        let slot = snapshot.slot(id).ok_or_else(|| anyhow!("no slot named {id}"))?.clone();
        if slot.status == Status::Free && !force {
            bail!("{id} is already free");
        }
        let dir = &slot.path;
        let branch = git::current_branch(dir);

        let mut stashed = None;
        if git::is_dirty(dir)? {
            let label = branch.clone().unwrap_or_else(|| "detached".into());
            let msg = format!("{} from {id}", stash_marker(&label));
            git(dir, &["stash", "push", "--quiet", "--include-untracked", "-m", &msg])?;
            stashed = Some(msg);
        }

        let mut rescued = None;
        if branch.is_none() && !git::git_ok(dir, &["merge-base", "--is-ancestor", "HEAD", &self.base_ref]) {
            let name = format!("velmux/rescue/{id}-{}", now());
            git(dir, &["branch", &name])?;
            rescued = Some(name);
        }

        git(dir, &["switch", "--quiet", "--detach", &self.base_ref])?;
        let mut clean = vec!["clean".to_string(), "-fdxq".to_string()];
        for keep in self.cfg.keep.iter().cloned().chain(self.links()) {
            clean.push("-e".into());
            clean.push(keep);
        }
        let clean: Vec<&str> = clean.iter().map(String::as_str).collect();
        git(dir, &clean)?;

        self.store.with(&self.base, |st| {
            let s = st.slot_mut(id).ok_or_else(|| anyhow!("{id} vanished"))?;
            s.status = Status::Free;
            s.branch = None;
            s.claimed_by = None;
            s.claimed_at = None;
            s.released_at = Some(now());
            Ok(())
        })?;
        Ok(Released { id: id.to_string(), branch, stashed, rescued })
    }

    pub fn status(&self) -> Result<Vec<SlotView>> {
        let Some(st) = self.store.snapshot()? else {
            return Ok(Vec::new());
        };
        Ok(st.slots.iter().map(|s| self.describe(s)).collect())
    }

    pub fn view(&self, id: &str) -> Result<SlotView> {
        let st = self.store.snapshot()?.ok_or_else(|| anyhow!("pool not initialised"))?;
        let slot = st.slot(id).ok_or_else(|| anyhow!("no slot named {id}"))?;
        Ok(self.describe(slot))
    }

    /// Which slot (if any) contains `path`.
    pub fn slot_of(&self, path: &Path) -> Result<Option<String>> {
        let path = path.canonicalize()?;
        Ok(self
            .status()?
            .into_iter()
            .find(|s| path.starts_with(&s.path))
            .map(|s| s.id))
    }

    /// Reconcile state with disk: prune dead worktrees and recreate missing slots.
    pub fn gc(&self, report: &dyn Fn(&str)) -> Result<()> {
        git(&self.base, &["worktree", "prune"])?;
        let missing: Vec<String> = self
            .status()?
            .into_iter()
            .filter(|s| !s.path.exists())
            .map(|s| s.id)
            .collect();
        if !missing.is_empty() {
            self.store.with(&self.base, |st| {
                st.slots.retain(|s| !missing.contains(&s.id));
                Ok(())
            })?;
            report(&format!("recreating missing: {}", missing.join(", ")));
        }
        self.init(report)?;
        Ok(())
    }

    fn describe(&self, s: &Slot) -> SlotView {
        let live = s.status == Status::InUse && s.path.exists();
        SlotView {
            id: s.id.clone(),
            path: s.path.clone(),
            status: s.status,
            branch: if live { git::current_branch(&s.path) } else { None }.or(s.branch.clone()),
            claimed_by: s.claimed_by.clone(),
            claimed_at: s.claimed_at,
            dirty: live && git::is_dirty(&s.path).unwrap_or(false),
        }
    }

    fn install(&self, slot: &Slot, report: &dyn Fn(&str)) -> Result<()> {
        let outcome = deps::ensure(
            &slot.path,
            &self.base,
            self.cfg.install_cmd.as_deref(),
            slot.deps_hash.as_deref(),
            &|m| report(&format!("{}: {m}", slot.id)),
        )?;
        if let Outcome::Installed { hash, .. } = outcome {
            self.store.with(&self.base, |st| {
                if let Some(s) = st.slot_mut(&slot.id) {
                    s.deps_hash = Some(hash);
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    /// Gitignored files shared from base into slots (configured, or base's `.env*`).
    fn links(&self) -> Vec<String> {
        if let Some(link) = &self.cfg.link {
            return link.clone();
        }
        let Ok(entries) = std::fs::read_dir(&self.base) else {
            return Vec::new();
        };
        entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".env") && self.base.join(n).is_file())
            .filter(|n| git::git_ok(&self.base, &["check-ignore", "-q", n]))
            .collect()
    }

    fn link_into(&self, slot: &Path) -> Result<()> {
        for name in self.links() {
            let dest = slot.join(&name);
            if std::fs::symlink_metadata(&dest).is_err() && self.base.join(&name).exists() {
                std::os::unix::fs::symlink(self.base.join(&name), &dest)
                    .with_context(|| format!("linking {}", dest.display()))?;
            }
        }
        if self.cfg.claude_history {
            claude::link(&self.env.claude_dir, &self.base, slot)?;
        }
        Ok(())
    }
}
