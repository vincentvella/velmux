use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;

use crate::config::{Config, Env, SyncMode};
use crate::deps::{self, Outcome};
use crate::git::{self, git};
use crate::state::{now, Parked, Slot, Status, Store};
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
    /// Commits on the base ref the slot doesn't have yet, and the reverse.
    pub behind: usize,
    pub ahead: usize,
    /// `rebase` / `merge` stopped part-way (conflicts to resolve).
    pub conflict: Option<String>,
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

/// What [`Pool::update_base`] did to the checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseUpdate {
    UpToDate,
    /// Moved forward this many commits.
    FastForwarded(usize),
    /// Behind, but local changes would be overwritten: left as it was.
    Blocked(usize),
    /// It has commits of its own as well: left alone.
    Diverged { behind: usize, ahead: usize },
    Skipped(String),
}

/// Parked work, as the sidebar and `velmux parked` show it.
#[derive(Debug, Clone, Serialize)]
pub struct ParkedView {
    pub branch: String,
    pub slot: String,
    pub parked_at: u64,
    pub sessions: Vec<String>,
    /// Files in its stash (0 when only commits were left).
    pub files: usize,
    /// Commits not on the base.
    pub ahead: usize,
}

impl ParkedView {
    /// `3 files · 1 commit`, what's waiting on the branch.
    pub fn what(&self) -> String {
        let plural = |n: usize, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
        let mut parts = Vec::new();
        if self.files > 0 {
            parts.push(plural(self.files, "file"));
        }
        if self.ahead > 0 {
            parts.push(plural(self.ahead, "commit"));
        }
        parts.join(" · ")
    }

    /// How long ago it was parked: `now`, `5m`, `3h`, `2d`.
    pub fn age(&self, now: u64) -> String {
        let s = now.saturating_sub(self.parked_at);
        match s {
            0..60 => "now".into(),
            60..3600 => format!("{}m", s / 60),
            3600..86400 => format!("{}h", s / 3600),
            _ => format!("{}d", s / 86400),
        }
    }
}

/// `slot-3` → 3 (0 for anything else).
pub fn slot_number(id: &str) -> u16 {
    id.strip_prefix("slot-").and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// `shop-pool-3` (or `shop-slot-3`, `shop-pool3`) → 3, for repo `shop`.
fn sibling_number(repo: &str, folder: &str) -> Option<u16> {
    let rest = folder.strip_prefix(repo)?.strip_prefix('-')?;
    let rest = rest.strip_prefix("pool").or_else(|| rest.strip_prefix("slot"))?;
    let n = rest.strip_prefix('-').unwrap_or(rest);
    n.parse().ok().filter(|&n| n > 0)
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

    /// This repo's worktrees laid out as a pool beside it, the way some other
    /// tools do it (`<parent>/<repo>-pool-N`, or `-slot-N`), by number.
    pub fn siblings(&self) -> Vec<(u16, PathBuf)> {
        let parent = self.base.parent().unwrap_or(Path::new("/"));
        let mut found: Vec<(u16, PathBuf)> = git::worktrees(&self.base)
            .into_iter()
            .filter(|p| p.parent() == Some(parent))
            .filter_map(|p| Some((sibling_number(&self.name, &p.file_name()?.to_string_lossy())?, p)))
            .collect();
        found.sort();
        found
    }

    /// Take this repo's [`siblings`](Self::siblings) in as slots, keeping
    /// their paths: one that's detached and clean is free, any other is in use
    /// (its branch and changes are left as they are). Returns how many.
    pub fn adopt(&self, report: &dyn Fn(&str)) -> Result<usize> {
        let siblings = self.siblings();
        if siblings.is_empty() {
            return Ok(0);
        }
        self.store.with(&self.base, |st| {
            let mut adopted = 0;
            for (n, path) in siblings {
                let Ok(path) = path.canonicalize() else { continue };
                if st.slots.iter().any(|s| s.path == path) {
                    continue;
                }
                // Its own number when that's free, else the next one that is.
                let id = (n..).chain(1..n).map(|n| format!("slot-{n}")).find(|id| st.slot(id).is_none()).unwrap();
                let branch = git::current_branch(&path);
                let free = branch.is_none() && !git::is_dirty(&path).unwrap_or(true);
                report(&format!("{id}: using the existing worktree {}", path.display()));
                st.slots.push(Slot {
                    id,
                    status: if free { Status::Free } else { Status::InUse },
                    claimed_at: (!free).then(now),
                    path,
                    branch,
                    claimed_by: None,
                    released_at: None,
                    deps_hash: None,
                });
                adopted += 1;
            }
            st.slots.sort_by_key(|s| slot_number(&s.id));
            Ok(adopted)
        })
    }

    /// Where a new slot `n` goes: beside the repo when its pool already lives
    /// there, else in the pool folder.
    fn new_slot_path(&self, slots: &[Slot], n: usize) -> PathBuf {
        let parent = self.base.parent().unwrap_or(Path::new("/"));
        if slots.iter().any(|s| s.path.parent() == Some(parent)) {
            parent.join(format!("{}-pool-{n}", self.name))
        } else {
            self.dir.join(format!("slot-{n}"))
        }
    }

    /// Every folder holding a slot: the pool folder, and each slot that lives
    /// elsewhere (beside the repo), for giving an agent access to them.
    pub fn slot_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.dir.clone()];
        if let Ok(Some(st)) = self.store.snapshot() {
            dirs.extend(st.slots.into_iter().map(|s| s.path).filter(|p| !p.starts_with(&self.dir)));
        }
        dirs
    }

    /// Create missing slots up to the configured count. Idempotent; never shrinks.
    pub fn init(&self, report: &dyn Fn(&str)) -> Result<Vec<SlotView>> {
        std::fs::create_dir_all(&self.dir)?;
        self.adopt(report)?;
        let created = self.store.with(&self.base, |st| {
            let mut created = Vec::new();
            for n in 1..=self.cfg.slots {
                let id = format!("slot-{n}");
                let path = st.slot(&id).map(|s| s.path.clone()).unwrap_or_else(|| self.new_slot_path(&st.slots, n));
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
        if let Some(branch) = &opts.branch {
            self.store.with(&self.base, |st| {
                st.parked.retain(|p| &p.branch != branch);
                Ok(())
            })?;
        }
        self.view(&slot.id)
    }

    fn prepare(&self, slot: &Slot, opts: &ClaimOpts, report: &dyn Fn(&str)) -> Result<()> {
        let dir = &slot.path;
        // Progress lines start with the slot id, so callers know which slot is busy.
        report(&format!("{}: checking out {}", slot.id, opts.branch.as_deref().unwrap_or(&self.base_ref)));
        if opts.fetch && git::has_remote(dir) {
            report(&format!("{}: fetching origin", slot.id));
            git(dir, &["fetch", "--quiet", "origin"])?;
        }
        match &opts.branch {
            Some(b) if git::local_branch_exists(dir, b) => {
                git(dir, &["switch", "--quiet", b])?;
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
        // Work parked on this branch (released, or moved out of the checkout).
        if let Some(b) = &opts.branch {
            if let Some(stash) = git::find_stash(dir, &stash_marker(b)) {
                report(&format!("{}: restoring parked work from {stash}", slot.id));
                if let Err(e) = git(dir, &["stash", "pop", "--quiet", &stash]) {
                    report(&format!("warning: could not restore {stash}, left in stash list: {e}"));
                }
            }
        }
        self.link_into(dir)?;
        if opts.install && self.cfg.install {
            self.install(slot, report)?;
        }
        for hook in &self.cfg.post_claim {
            report(&format!("hook: {hook}"));
            deps::run_shell_with(dir, hook, &self.slot_env(&slot.id))?;
        }
        Ok(())
    }

    /// Park the slot's work and return it to the pool clean, keeping deps warm.
    pub fn release(&self, id: &str, force: bool) -> Result<Released> {
        self.release_with(id, force, &[])
    }

    /// [`release`](Self::release), noting the Claude `sessions` that worked on
    /// the branch so resuming it can bring them back.
    pub fn release_with(&self, id: &str, force: bool, sessions: &[String]) -> Result<Released> {
        let snapshot = self.store.snapshot()?.ok_or_else(|| anyhow!("pool not initialised"))?;
        let slot = snapshot.slot(id).ok_or_else(|| anyhow!("no slot named {id}"))?.clone();
        if slot.status == Status::Free && !force {
            bail!("{id} is already free");
        }
        let dir = &slot.path;
        let branch = git::current_branch(dir);

        // Detached commits get a branch first, so any stash is parked under it.
        let mut rescued = None;
        if branch.is_none() && !git::git_ok(dir, &["merge-base", "--is-ancestor", "HEAD", &self.base_ref]) {
            let name = format!("velmux/rescue/{id}-{}", now());
            git(dir, &["branch", &name])?;
            rescued = Some(name);
        }
        let parked = branch.clone().or(rescued.clone());

        let mut stashed = None;
        if git::is_dirty(dir)? {
            let label = parked.clone().unwrap_or_else(|| "detached".into());
            let msg = format!("{} from {id}", stash_marker(&label));
            git(dir, &["stash", "push", "--quiet", "--include-untracked", "-m", &msg])?;
            stashed = Some(msg);
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
            if let Some(b) = &parked {
                st.parked.retain(|p| &p.branch != b);
                st.parked.push(Parked { branch: b.clone(), slot: id.to_string(), parked_at: now(), sessions: sessions.to_vec() });
            }
            Ok(())
        })?;
        Ok(Released { id: id.to_string(), branch, stashed, rescued })
    }

    /// Bring a slot's branch up to date with the base ref (fetching first):
    /// rebase (or merge, per `sync`) with uncommitted work set aside and put
    /// back. On conflicts it backs out, leaving the slot as it was.
    pub fn sync(&self, id: &str, report: &dyn Fn(&str)) -> Result<String> {
        let slot = self.view(id)?;
        if slot.status != Status::InUse {
            bail!("{id} is free; nothing to sync");
        }
        let dir = &slot.path;
        if let Some(op) = git::conflict(dir) {
            bail!("{id} has a {op} in progress; finish or abort it first");
        }
        if git::has_remote(dir) {
            report(&format!("{id}: fetching origin"));
            git(dir, &["fetch", "--quiet", "origin"])?;
        }
        let (behind, _) = git::behind_ahead(dir, &self.base_ref);
        if behind == 0 {
            return Ok(format!("{id} is up to date with {}", self.base_ref));
        }
        let (verb, args, abort): (&str, Vec<&str>, &[&str]) = match self.cfg.sync {
            SyncMode::Rebase => ("rebased onto", vec!["rebase", "--autostash", "--quiet", &self.base_ref], &["rebase", "--abort"]),
            SyncMode::Merge => ("merged", vec!["merge", "--autostash", "--no-edit", "--quiet", &self.base_ref], &["merge", "--abort"]),
        };
        report(&format!("{id}: {} {behind} commit(s) from {}", if self.cfg.sync == SyncMode::Rebase { "rebasing onto" } else { "merging" }, self.base_ref));
        if let Err(e) = git(dir, &args) {
            let files = git(dir, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
            let n = files.lines().count();
            let _ = git(dir, abort);
            if n > 0 {
                bail!("conflicts in {n} file(s) ({}); backed out, {id} is unchanged", files.lines().take(3).collect::<Vec<_>>().join(", "));
            }
            return Err(e.context(format!("syncing {id}; backed out")));
        }
        Ok(format!("{id} {verb} {} (+{behind})", self.base_ref))
    }

    /// A slot's environment: `VELMUX_BASE`, `VELMUX_SLOT` and its ports.
    pub fn slot_env(&self, id: &str) -> Vec<(String, String)> {
        let mut env = vec![
            ("VELMUX_BASE".to_string(), self.base.display().to_string()),
            ("VELMUX_POOL_DIR".to_string(), self.dir.display().to_string()),
            ("VELMUX_SLOT".to_string(), id.to_string()),
        ];
        env.extend(self.cfg.ports.for_slot(slot_number(id)).into_iter().map(|(k, v)| (k, v.to_string())));
        env
    }

    /// The slot's main port (`PORT`, else the first configured), if ports are on.
    pub fn slot_port(&self, id: &str) -> Option<u16> {
        self.cfg.ports.primary(slot_number(id))
    }

    /// The checkout's uncommitted changes, as `git status` lines (empty: clean).
    /// Work there is what slots are for, so velmux flags it.
    pub fn base_changes(&self) -> Vec<String> {
        git(&self.base, &["status", "--porcelain"]).map(|s| s.lines().map(String::from).collect()).unwrap_or_default()
    }

    /// Move the checkout's uncommitted changes out of the way, parked for
    /// `branch`: claiming that branch puts them back in its slot.
    pub fn stash_base_for(&self, branch: &str) -> Result<()> {
        if self.base_changes().is_empty() {
            bail!("{}'s checkout has no changes to move", self.name);
        }
        git(&self.base, &["stash", "push", "--quiet", "--include-untracked", "-m", &format!("{} from the checkout", stash_marker(branch))])?;
        Ok(())
    }

    /// Fast-forward the checkout to its upstream when that's the base ref and
    /// it can: on that branch, nothing of its own committed, and git refusing
    /// (leaving it as it was) if local changes are in the way.
    pub fn update_base(&self) -> BaseUpdate {
        let dir = &self.base;
        let Some(upstream) = git(dir, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]).ok() else {
            return BaseUpdate::Skipped("its branch has no upstream".into());
        };
        let same = |a: &str, b: &str| git(dir, &["rev-parse", a]).ok().zip(git(dir, &["rev-parse", b]).ok()).is_some_and(|(x, y)| x == y);
        if upstream != self.base_ref && !same(&upstream, &self.base_ref) {
            return BaseUpdate::Skipped(format!("it tracks {upstream}, not {}", self.base_ref));
        }
        let (behind, ahead) = git::behind_ahead(dir, "@{u}");
        match (behind, ahead) {
            (0, _) => BaseUpdate::UpToDate,
            (_, a) if a > 0 => BaseUpdate::Diverged { behind, ahead: a },
            (n, _) => match git(dir, &["merge", "--ff-only", "--quiet", "@{u}"]) {
                Ok(_) => BaseUpdate::FastForwarded(n),
                Err(_) => BaseUpdate::Blocked(n),
            },
        }
    }

    /// Fetch the repo's remote (no-op without one); slots see the new base ref.
    pub fn fetch(&self) -> Result<()> {
        if git::has_remote(&self.base) {
            git(&self.base, &["fetch", "--quiet", "origin"])?;
        }
        Ok(())
    }

    /// Branches with work waiting to be resumed, newest first. Ones with nothing
    /// left (no stash, no commits past the base, or deleted) are dropped.
    pub fn parked(&self) -> Result<Vec<ParkedView>> {
        let Some(st) = self.store.snapshot()? else { return Ok(Vec::new()) };
        if st.parked.is_empty() {
            return Ok(Vec::new());
        }
        let in_use: Vec<String> = self.status()?.into_iter().filter(|s| s.status == Status::InUse).filter_map(|s| s.branch).collect();
        let mut views = Vec::new();
        let mut gone = Vec::new();
        for p in &st.parked {
            if in_use.contains(&p.branch) {
                continue;
            }
            let stash = git::stashes_with(&self.base, &stash_marker(&p.branch)).into_iter().next();
            let exists = git::local_branch_exists(&self.base, &p.branch);
            let ahead = if exists { git::ahead(&self.base, &self.base_ref, &p.branch) } else { 0 };
            if stash.is_none() && ahead == 0 {
                gone.push(p.branch.clone());
                continue;
            }
            let files = stash.as_ref().map_or(0, |(r, _)| git::stash_files(&self.base, r));
            views.push(ParkedView { branch: p.branch.clone(), slot: p.slot.clone(), parked_at: p.parked_at, sessions: p.sessions.clone(), files, ahead });
        }
        if !gone.is_empty() {
            self.store.with(&self.base, |st| {
                st.parked.retain(|p| !gone.contains(&p.branch));
                Ok(())
            })?;
        }
        views.sort_by_key(|v| std::cmp::Reverse(v.parked_at));
        Ok(views)
    }

    /// Throw parked work away: drop its stashes and forget it. The branch (and
    /// any commits on it) is kept; delete it with git if you mean to.
    pub fn discard(&self, branch: &str) -> Result<usize> {
        let stashes = git::stashes_with(&self.base, &stash_marker(branch));
        // Highest index first, so dropping doesn't renumber the rest.
        for (refname, _) in stashes.iter().rev() {
            git(&self.base, &["stash", "drop", "--quiet", refname])?;
        }
        self.store.with(&self.base, |st| {
            st.parked.retain(|p| p.branch != branch);
            Ok(())
        })?;
        Ok(stashes.len())
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
        let (behind, ahead) = if live { git::behind_ahead(&s.path, &self.base_ref) } else { (0, 0) };
        SlotView {
            behind,
            ahead,
            conflict: if live { git::conflict(&s.path).map(String::from) } else { None },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_pools_beside_the_repo() {
        assert_eq!(sibling_number("shopping-x", "shopping-x-pool-1"), Some(1));
        assert_eq!(sibling_number("shopping-x", "shopping-x-slot-12"), Some(12));
        assert_eq!(sibling_number("shop", "shop-pool3"), Some(3));
        assert_eq!(sibling_number("shop", "shopping-x-pool-1"), None);
        assert_eq!(sibling_number("shop", "shop-pool-0"), None);
        assert_eq!(sibling_number("shop", "shop-feature"), None);
    }
}
