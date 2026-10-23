//! Workstreams: slots from one or more repos, grouped as one piece of work.
//! A workstream is a place of its own (a folder in the unclassified group,
//! like a scratch session) whose folder links to each of its slots, named
//! after its repo, so an agent started there reaches every one. Its slots
//! leave their repo's list in the sidebar and show under it instead.

use super::*;

/// What an agent in a workstream is told.
pub(super) const PROMPT: &str = "You are running inside velmux, in the workstream \"{name}\": work that spans \
     more than one repo. This folder holds a link to each of its slots, named after the repo (a git worktree on \
     its own branch, dependencies installed); make your changes inside those subfolders, committing in each \
     repo's own git. To bring another repo in, run `velmux claim <short-branch-name> --repo <repo name>`: it \
     claims a slot there and links it here. Don't release the slots yourself: the user releases the workstream \
     when it's done.";

impl App {
    /// `name` as a `&'static str` for a menu row; each distinct one is kept once.
    pub(super) fn intern(&mut self, name: &str) -> &'static str {
        if let Some(s) = self.interned.get(name) {
            return s;
        }
        let s: &'static str = Box::leak(name.to_string().into_boxed_str());
        self.interned.insert(name.to_string(), s);
        s
    }

    /// The folder a place is (scratch session or workstream), if it's one.
    pub fn folder(&self, loc: &Loc) -> Option<&Folder> {
        let id = loc.slot.as_ref()?;
        self.projects.get(loc.project).filter(|p| p.is_unclassified())?.folders.iter().find(|f| &f.id == id)
    }

    pub(super) fn folder_mut(&mut self, loc: &Loc) -> Option<&mut Folder> {
        let id = loc.slot.clone()?;
        self.projects.get_mut(loc.project).filter(|p| p.is_unclassified())?.folders.iter_mut().find(|f| f.id == id)
    }

    pub fn is_workstream(&self, loc: &Loc) -> bool {
        self.folder(loc).is_some_and(|f| f.members.is_some())
    }

    /// Every workstream, in sidebar order.
    pub fn workstreams(&self) -> Vec<Loc> {
        self.projects
            .iter()
            .enumerate()
            .filter(|(_, p)| p.is_unclassified() && !p.removed)
            .flat_map(|(i, p)| p.folders.iter().filter(|f| f.members.is_some()).map(move |f| Loc { project: i, slot: Some(f.id.clone()) }))
            .collect()
    }

    /// A workstream's slots, as places (ones whose repo isn't open are left out).
    pub fn members(&self, ws: &Loc) -> Vec<Loc> {
        let Some(members) = self.folder(ws).and_then(|f| f.members.as_ref()) else { return Vec::new() };
        members
            .iter()
            .filter_map(|m| {
                let project = self.projects.iter().position(|p| !p.removed && p.base() == Some(m.base.as_path()))?;
                Some(Loc { project, slot: Some(m.slot.clone()) })
            })
            .collect()
    }

    /// The pull requests of a workstream's slots (the ones that have one).
    pub fn workstream_prs(&self, ws: &Loc) -> Vec<&velmux_pool::gh::Pr> {
        self.members(ws)
            .iter()
            .filter_map(|m| {
                let project = &self.projects[m.project];
                let branch = project.slots.iter().find(|s| Some(&s.id) == m.slot.as_ref())?.branch.as_ref()?;
                project.prs.get(branch)
            })
            .collect()
    }

    /// Every slot in the workstream has a PR, and all of them have merged:
    /// it's done, and ready to release.
    pub fn workstream_ready(&self, ws: &Loc) -> bool {
        let members = self.members(ws);
        let prs = self.workstream_prs(ws);
        !members.is_empty() && prs.len() == members.len() && prs.iter().all(|p| p.state == velmux_pool::gh::PrState::Merged)
    }

    /// The workstream a repo's slot is in, if any.
    pub fn workstream_of(&self, slot: &Loc) -> Option<Loc> {
        let base = self.projects.get(slot.project)?.base()?;
        let id = slot.slot.as_ref()?;
        self.workstreams().into_iter().find(|ws| {
            self.folder(ws).and_then(|f| f.members.as_ref()).is_some_and(|m| m.iter().any(|m| m.base == base && &m.slot == id))
        })
    }

    /// Where workstream folders go: beside the repos' pools (`.pools/.workstreams`).
    fn workstreams_root(&self) -> PathBuf {
        self.projects
            .iter()
            .find_map(|p| p.pool.as_ref().and_then(|pool| pool.dir.parent().map(|d| d.join(".workstreams"))))
            .or_else(|| paths::state_dir().ok().map(|d| d.join("workstreams")))
            .unwrap_or_else(|| PathBuf::from("/tmp/velmux-workstreams"))
    }

    /// A new, empty workstream called `name`.
    pub(super) fn new_workstream(&mut self, name: &str) -> Option<Loc> {
        let name: String = name.trim().replace('/', "-");
        let name = name.trim_start_matches('.').to_string();
        if name.is_empty() {
            return None;
        }
        let project = self.unclassified();
        self.projects[project].removed = false;
        let mut id = name.clone();
        let mut n = 2;
        while self.projects[project].folders.iter().any(|f| f.id == id) {
            id = format!("{name}-{n}");
            n += 1;
        }
        let dir = self.workstreams_root().join(&id);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.say(format!("can't make {}: {e}", dir.display()));
            return None;
        }
        self.projects[project].folders.push(Folder { id: id.clone(), path: dir, members: Some(Vec::new()) });
        self.save();
        Some(Loc { project, slot: Some(id) })
    }

    /// Put a repo's slot into workstream `ws` (out of any other it was in).
    pub(super) fn add_to_workstream(&mut self, ws: &Loc, slot: &Loc) {
        let (Some(base), Some(id)) = (self.projects.get(slot.project).and_then(|p| p.base()).map(Path::to_path_buf), slot.slot.clone()) else {
            return;
        };
        if let Some(old) = self.workstream_of(slot) {
            if &old == ws {
                return;
            }
            self.remove_from_workstream(slot);
        }
        if let Some(members) = self.folder_mut(ws).and_then(|f| f.members.as_mut()) {
            members.push(session::Member { base, slot: id });
        }
        self.link_workstream(ws);
        self.full_redraw = true;
        self.save();
    }

    /// Take a slot out of its workstream (back into its repo's list).
    pub(super) fn remove_from_workstream(&mut self, slot: &Loc) {
        let Some(ws) = self.workstream_of(slot) else { return };
        let base = self.projects[slot.project].base().map(Path::to_path_buf);
        if let Some(members) = self.folder_mut(&ws).and_then(|f| f.members.as_mut()) {
            members.retain(|m| Some(&m.base) != base.as_ref() || Some(&m.slot) != slot.slot.as_ref());
        }
        self.link_workstream(&ws);
        self.full_redraw = true;
        self.save();
    }

    /// What a workstream's folder should link to: (link name, slot path). A
    /// repo's name, or `<repo>-<n>` when it has more than one slot in it.
    fn workstream_links(&self, ws: &Loc) -> Vec<(String, PathBuf)> {
        let members = self.members(ws);
        members
            .iter()
            .filter_map(|m| {
                let project = &self.projects[m.project];
                let id = m.slot.as_ref()?;
                let path = project.slots.iter().find(|s| &s.id == id).map(|s| s.path.clone())?;
                let shared = members.iter().filter(|o| o.project == m.project).count() > 1;
                let name = if shared { format!("{}-{}", project.name(), id.trim_start_matches("slot-")) } else { project.name().to_string() };
                Some((name, path))
            })
            .collect()
    }

    /// Make a workstream's folder link to exactly its slots (other links go;
    /// anything else in it is left alone).
    pub(super) fn link_workstream(&self, ws: &Loc) {
        let Some(dir) = self.folder(ws).map(|f| f.path.clone()) else { return };
        let want = self.workstream_links(ws);
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let is_link = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
            let name = entry.file_name().to_string_lossy().into_owned();
            let keep = want.iter().any(|(n, target)| *n == name && std::fs::read_link(&path).is_ok_and(|t| &t == target));
            if is_link && !keep {
                let _ = std::fs::remove_file(&path);
            }
        }
        for (name, target) in want {
            let link = dir.join(&name);
            if std::fs::symlink_metadata(&link).is_err() {
                let _ = std::os::unix::fs::symlink(&target, &link);
            }
        }
    }

    /// A pane's program is working in `dir` (or under it).
    fn in_use(&self, dir: &Path) -> bool {
        self.panes.iter().any(|p| p.fg.cwd.as_ref().is_some_and(|c| c.starts_with(dir)))
    }

    /// Empty folders left in the workstreams' folder by removed workstreams,
    /// once nothing runs in them.
    fn sweep_workstream_dirs(&self) {
        let root = self.workstreams_root();
        let ours: Vec<PathBuf> = self.workstreams().iter().filter_map(|w| self.folder(w).map(|f| f.path.clone())).collect();
        for entry in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            let dir = entry.path();
            if dir.is_dir() && !ours.contains(&dir) && !self.in_use(&dir) {
                let _ = std::fs::remove_dir(&dir); // only if it's empty
            }
        }
    }

    /// After a refresh: slots that were released (or are gone) leave their
    /// workstream. Repos whose slots aren't known yet are left alone.
    pub(super) fn prune_workstreams(&mut self) {
        self.sweep_workstream_dirs();
        self.prune_parked_workstreams();
        for ws in self.workstreams() {
            let before = self.folder(&ws).and_then(|f| f.members.clone()).unwrap_or_default();
            let keep: Vec<session::Member> = before
                .iter()
                .filter(|m| {
                    let Some(project) = self.projects.iter().find(|p| !p.removed && p.base() == Some(m.base.as_path())) else { return true };
                    if project.slots.is_empty() {
                        return true;
                    }
                    project.slots.iter().any(|s| s.id == m.slot && s.status == Status::InUse)
                })
                .cloned()
                .collect();
            if keep.len() != before.len() {
                if let Some(f) = self.folder_mut(&ws) {
                    f.members = Some(keep);
                }
                self.link_workstream(&ws);
                self.save();
            } else {
                // Slot paths may have changed (or the folder been cleared): keep links right.
                self.link_workstream(&ws);
            }
        }
    }

    /// Hand back every slot in a workstream (parking unfinished work as a
    /// release does), then remove it. Panes open in its slots move back to
    /// their repo's checkout, still running, as `velmux release` does for one;
    /// their conversations are also kept with the parked branch.
    pub(super) fn release_workstream(&mut self, ws: Loc) {
        let members = self.members(&ws);
        let n = members.len();
        // Kept to bring it back as a whole: its branches (shown while any is
        // still parked) and its own conversations.
        let branches: Vec<(PathBuf, String)> = members
            .iter()
            .filter_map(|m| {
                let project = &self.projects[m.project];
                let branch = project.slots.iter().find(|s| Some(&s.id) == m.slot.as_ref())?.branch.clone()?;
                Some((project.base()?.to_path_buf(), branch))
            })
            .collect();
        let sessions: Vec<String> = self.panes.iter().filter(|p| p.loc == ws).filter_map(|p| p.session.clone()).collect();
        if !branches.is_empty() {
            let name = ws.slot.clone().unwrap_or_default();
            self.parked_workstreams.retain(|p| p.name != name);
            self.parked_workstreams.push(session::ParkedWorkstream { name, branches, sessions, parked_at: velmux_pool::state::now() });
        }
        // The workstream's own panes (the conversation that worked across the
        // repos) go to its first repo's checkout, still running.
        let to = members.first().map(|m| Loc { project: m.project, slot: None });
        for slot in members {
            self.release_moving_panes(slot);
        }
        if let Some(to) = to {
            let own: Vec<u64> = self.panes.iter().filter(|p| p.loc == ws).map(|p| p.id).collect();
            for id in &own {
                self.move_pane(*id, to.clone());
            }
            if !own.is_empty() && self.current.as_ref() == Some(&ws) {
                self.go_to(to);
            }
        }
        self.remove_workstream(ws.clone());
        self.say(format!("released {n} slot(s) and removed {}", ws.slot.unwrap_or_default()));
    }

    /// Remove a workstream: its panes close, its slots go back to their repos,
    /// its folder's links go (and the folder, if that leaves it empty).
    pub(super) fn remove_workstream(&mut self, ws: Loc) {
        let dir = self.folder(&ws).map(|f| f.path.clone());
        if let Some(f) = self.folder_mut(&ws) {
            f.members = Some(Vec::new());
        }
        self.link_workstream(&ws);
        self.close_panes_where(|p| p.loc == ws);
        // Its folder goes once nothing runs in it (a pane moved out may still);
        // `prune_workstreams` takes it then.
        if let Some(dir) = dir.filter(|d| !self.in_use(d)) {
            let _ = std::fs::remove_dir(dir);
        }
        if let (Some(id), Some(project)) = (&ws.slot, self.projects.get_mut(ws.project)) {
            project.folders.retain(|f| &f.id != id);
        }
        self.views.remove(&ws);
        self.leave(&ws);
        self.full_redraw = true;
        self.save();
    }

    /// `velmux claim` from a pane ([`ClientMsg::ClaimInto`]): which repo (by
    /// `repo`, else the pane's own), and which workstream (`workstream`,
    /// made if it doesn't exist, else the one the pane is in). For a
    /// workstream the slot joins it and the pane stays; otherwise the pane
    /// moves into the slot, as a plain claim does.
    pub(super) fn claim_into_for(&mut self, pane: u64, branch: Option<String>, repo: Option<String>, workstream: Option<String>, mut stream: UnixStream) {
        let mut fail = |msg: String| {
            let _ = ServerMsg::Reply(format!("error: {msg}")).write(&mut stream);
        };
        let Some(here) = self.pane(pane).map(|p| p.loc.clone()) else { return fail("that pane isn't in velmux".into()) };
        let ws = match &workstream {
            Some(name) => match self.workstreams().into_iter().find(|w| w.slot.as_deref() == Some(name.as_str())) {
                Some(ws) => Some(ws),
                None => match self.new_workstream(name) {
                    Some(ws) => Some(ws),
                    None => return fail(format!("can't make a workstream called {name:?}")),
                },
            },
            None => self.is_workstream(&here).then(|| here.clone()),
        };
        let project = match &repo {
            Some(r) => match self.repo_named(r) {
                Some(p) => p,
                None => {
                    let open: Vec<&str> = self.projects.iter().filter(|p| !p.removed && !p.is_unclassified()).map(|p| p.name()).collect();
                    return fail(format!("no repo {r:?} in velmux (open: {}); give its path, or add it as a space", open.join(", ")));
                }
            },
            None if !self.projects[here.project].is_unclassified() => here.project,
            None => return fail("say which repo the slot is in: velmux claim <branch> --repo <repo>".into()),
        };
        let for_pane = Some((pane, stream));
        match ws {
            Some(ws) => {
                if let Some(job) = self.claim_in(project, branch, for_pane) {
                    self.claim_into.insert(job, ws);
                }
            }
            None => {
                self.claim_in(project, branch, for_pane);
            }
        }
    }

    /// An open repo by name, or the repo a path is in (its checkout, one of
    /// its slots, a workstream's link to one); a repo that isn't open yet is
    /// added as a space.
    fn repo_named(&mut self, r: &str) -> Option<usize> {
        let open = |p: &Project| !p.removed && !p.is_unclassified();
        if let Some(i) = self.projects.iter().position(|p| open(p) && p.name() == r) {
            return Some(i);
        }
        let path = Path::new(r);
        if !path.is_absolute() {
            return None;
        }
        let pool = Pool::discover(Env::from_process().ok()?, path).ok()?;
        if let Some(i) = self.projects.iter().position(|p| p.base() == Some(pool.base.as_path())) {
            self.projects[i].removed = false;
            return Some(i);
        }
        self.projects.push(Project::repo(pool));
        self.refresh_slots();
        Some(self.projects.len() - 1)
    }

    /// A parked workstream's branches that are still parked: (repo, branch).
    pub fn parked_branches(&self, i: usize) -> Vec<(usize, String)> {
        let Some(pw) = self.parked_workstreams.get(i) else { return Vec::new() };
        pw.branches
            .iter()
            .filter_map(|(base, branch)| {
                let project = self.projects.iter().position(|p| !p.removed && p.base() == Some(base.as_path()))?;
                self.projects[project].parked.iter().any(|p| &p.branch == branch).then(|| (project, branch.clone()))
            })
            .collect()
    }

    /// A repo's parked branch belongs to a parked workstream (and is listed there).
    pub fn in_parked_workstream(&self, project: usize, branch: &str) -> bool {
        let Some(base) = self.projects.get(project).and_then(|p| p.base()) else { return false };
        self.parked_workstreams.iter().any(|pw| pw.branches.iter().any(|(b, br)| b == base && br == branch))
    }

    /// Bring a parked workstream back: a workstream of the same name, its
    /// parked branches claimed into it (each with its conversations), and its
    /// own conversations, moved in if they're still open, else resumed.
    pub(super) fn resume_parked_workstream(&mut self, i: usize) {
        let branches = self.parked_branches(i);
        if i >= self.parked_workstreams.len() {
            return;
        }
        let pw = self.parked_workstreams.remove(i);
        let Some(ws) = self.new_workstream(&pw.name) else { return };
        for (project, branch) in &branches {
            if let Some(job) = self.claim_in(*project, Some(branch.clone()), None) {
                self.claim_into.insert(job, ws.clone());
            }
        }
        let open: Vec<String> = pw.sessions.iter().filter(|s| crate::agents::exists(s, &self.claude_dir) || self.live_session_pane(s, None).is_some()).cloned().collect();
        self.go_to(ws.clone());
        if !open.is_empty() {
            self.resume_sessions(ws, &open);
        }
        self.save();
        self.say(format!("resuming {}: {} branch(es)", pw.name, branches.len()));
    }

    /// Parked workstreams with nothing left parked (resumed or discarded one
    /// by one, or merged) go. Repos whose parked work isn't known yet are
    /// left alone.
    pub(super) fn prune_parked_workstreams(&mut self) {
        let before = self.parked_workstreams.len();
        let loaded = |base: &Path| self.projects.iter().any(|p| !p.removed && p.base() == Some(base) && !p.slots.is_empty());
        let keep: Vec<bool> = (0..before)
            .map(|i| {
                let pw = &self.parked_workstreams[i];
                // Just released: its branches are still being parked.
                let fresh = velmux_pool::state::now().saturating_sub(pw.parked_at) < 60;
                fresh || !self.parked_branches(i).is_empty() || pw.branches.iter().any(|(b, _)| !loaded(b))
            })
            .collect();
        let mut k = keep.into_iter();
        self.parked_workstreams.retain(|_| k.next().unwrap_or(true));
        if self.parked_workstreams.len() != before {
            self.save();
        }
    }

    /// Sync every slot in the workstream that's behind its base (rebase or
    /// merge, as each repo's config says), one job for all.
    pub(super) fn sync_workstream(&mut self, ws: &Loc) {
        let work: Vec<(Arc<Pool>, String)> = self
            .members(ws)
            .into_iter()
            .filter_map(|m| {
                let project = &self.projects[m.project];
                let slot = project.slots.iter().find(|s| Some(&s.id) == m.slot.as_ref())?;
                (slot.behind > 0).then(|| Some((project.pool.clone()?, slot.id.clone())))?
            })
            .collect();
        if work.is_empty() {
            return self.toast("every slot in it is up to date");
        }
        self.job_seq += 1;
        let job = format!("sync {}", self.job_seq);
        self.jobs.push((job.clone(), format!("syncing {} slots", work.len())));
        let tx = self.events.clone();
        std::thread::spawn(move || {
            let results: Vec<String> = work.iter().map(|(pool, id)| pool.sync(id, &|_| {}).unwrap_or_else(|e| format!("{}/{id}: {e:#}", pool.name))).collect();
            let _ = tx.send(Event::Synced { job, result: Ok(results.join(" · ")) });
        });
    }

    /// Push every slot in the workstream that has no PR yet and open one for
    /// each (in the browser, to finish), one job for all.
    pub(super) fn push_workstream(&mut self, ws: &Loc) {
        if !velmux_pool::gh::available() {
            return self.say("install the GitHub CLI (gh) for PRs");
        }
        let work: Vec<(PathBuf, String)> = self
            .members(ws)
            .into_iter()
            .filter_map(|m| {
                let project = &self.projects[m.project];
                let slot = project.slots.iter().find(|s| Some(&s.id) == m.slot.as_ref())?;
                let branch = slot.branch.clone()?;
                (!project.prs.contains_key(&branch)).then(|| (slot.path.clone(), branch))
            })
            .collect();
        if work.is_empty() {
            return self.toast("every slot in it has a PR");
        }
        self.job_seq += 1;
        let job = format!("pr {}", self.job_seq);
        self.jobs.push((job.clone(), format!("pushing {} branches", work.len())));
        let tx = self.events.clone();
        std::thread::spawn(move || {
            let results: Vec<String> = work
                .iter()
                .map(|(dir, branch)| match velmux_pool::gh::push_and_open(dir, branch) {
                    Ok(_) => format!("pushed {branch}"),
                    Err(e) => format!("{branch}: {e:#}"),
                })
                .collect();
            let _ = tx.send(Event::PrOpened { job, result: Ok(format!("{}; finish the PRs in your browser", results.join(" · "))) });
        });
    }
}
