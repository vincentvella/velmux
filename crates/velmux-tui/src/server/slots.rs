//! Claiming and releasing slots, moving panes between places, and keeping
//! slots in step with their base.

use super::*;

impl App {
    /// `velmux release` inside a pane: move it back to its repo's base, then
    /// release the slot it was in (other panes there close, work is stashed).
    pub(super) fn release_for(&mut self, pane: u64) -> String {
        let Some(loc) = self.pane(pane).map(|p| p.loc.clone()) else { return "error: that pane isn't in velmux".into() };
        let Some(slot) = loc.slot.clone().filter(|_| !self.projects[loc.project].is_unclassified()) else {
            return "error: this pane isn't in a slot (nothing to release)".into();
        };
        let session = self.pane(pane).and_then(|p| p.session.clone());
        self.move_pane(pane, Loc { project: loc.project, slot: None });
        self.release_location(loc, session);
        format!("releasing {slot}; this conversation is back in the base checkout")
    }

    /// Move a running pane to another location (its process is untouched).
    pub(super) fn move_pane(&mut self, id: u64, to: Loc) {
        let Some(from) = self.pane(id).map(|p| p.loc.clone()) else { return };
        if from == to {
            return;
        }
        if let Some(mut view) = self.views.remove(&from) {
            if let Some(t) = view.tab_of(id) {
                let tab = view.tabs.remove(t);
                if let Some(layout) = tab.layout.remove(id) {
                    let last_focus = tab.last_focus.filter(|f| *f != id);
                    view.tabs.insert(t, Tab { layout, zoomed: tab.zoomed.filter(|z| *z != id), last_focus, name: tab.name });
                } else {
                    view.forget_tab(t);
                }
            }
            if !view.tabs.is_empty() {
                view.active = view.active.min(view.tabs.len() - 1);
                self.views.insert(from.clone(), view);
            }
        }
        if let Some(pane) = self.panes.iter_mut().find(|p| p.id == id) {
            pane.loc = to.clone();
        }
        self.insert_into_view(&to, id, None);
        if self.focus == Some(id) {
            self.current = Some(to);
        }
        self.relayout();
    }

    /// Release a slot: close its panes, then stash / reset it in the background.
    /// The Claude sessions that worked there (`first`, the one that claimed it
    /// and is heading back to the base, then any closed here) are kept with
    /// the parked branch for resuming.
    pub(super) fn release_location(&mut self, loc: Loc, first: Option<String>) {
        self.release_keeping(loc, first.into_iter().collect());
    }

    /// Release a slot, moving the panes open in it back to its repo's
    /// checkout, still running (as `velmux release` does for its own pane);
    /// their conversations are kept with the parked branch too.
    pub(super) fn release_moving_panes(&mut self, slot: Loc) {
        let base = Loc { project: slot.project, slot: None };
        let here: Vec<(u64, Option<String>)> = self.panes.iter().filter(|p| p.loc == slot).map(|p| (p.id, p.session.clone())).collect();
        for (id, _) in &here {
            self.move_pane(*id, base.clone());
        }
        if self.current.as_ref() == Some(&slot) && !here.is_empty() {
            self.go_to(base);
        }
        self.release_keeping(slot, here.into_iter().filter_map(|(_, s)| s).collect());
    }

    /// [`release_location`](Self::release_location), keeping `sessions` (ones
    /// that left the slot before it was released) with the parked branch too.
    pub(super) fn release_keeping(&mut self, loc: Loc, mut sessions: Vec<String>) {
        let Some(slot) = loc.slot.clone() else { return };
        let Some(pool) = self.projects[loc.project].pool.clone() else { return };
        for pane in self.panes.iter_mut().filter(|p| p.loc == loc) {
            if let Some(s) = pane.session.clone().filter(|s| !sessions.contains(s)) {
                sessions.push(s);
            }
            pane.kill();
        }
        let tx = self.events.clone();
        let project = loc.project;
        // A branch whose PR merged has nothing left to resume (unless there
        // were uncommitted changes): don't park it.
        let branch = self.projects[project].slots.iter().find(|s| s.id == slot).and_then(|s| s.branch.clone());
        let merged = branch.as_ref().and_then(|b| self.projects[project].prs.get(b)).is_some_and(|pr| pr.state == velmux_pool::gh::PrState::Merged);
        self.jobs.push((format!("release {slot}"), format!("releasing {slot}")));
        std::thread::spawn(move || {
            // Give killed processes a moment to exit before git touches the tree.
            std::thread::sleep(Duration::from_millis(300));
            let result = pool
                .release_with(&slot, false, &sessions)
                .map(|r| {
                    if let (true, None, Some(b)) = (merged, &r.stashed, &r.branch) {
                        let _ = pool.discard(b);
                    }
                    let mut msg = format!("released {}", r.id);
                    if let Some(b) = r.branch.as_ref().filter(|_| r.stashed.is_some()) {
                        msg = format!("parked {b}");
                    }

                    if let Some(b) = r.rescued {
                        msg.push_str(&format!(" · kept commits on {b}"));
                    }
                    msg
                })
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Released { project, slot, result });
        });
    }

    pub(super) fn claim(&mut self, branch: Option<String>) {
        let Some(loc) = self.current_loc() else { return };
        self.claim_in(loc.project, branch, None);
    }

    /// Claim a slot in `project` (in the background). With `for_pane`, that pane
    /// moves into the slot and the stream gets its path, instead of a new pane.
    pub(super) fn claim_in(&mut self, project: usize, branch: Option<String>, for_pane: Option<(u64, UnixStream)>) -> Option<String> {
        // Claiming a parked branch yourself resumes it: its Claude comes back too.
        let resume = match (&branch, &for_pane) {
            (Some(b), None) => self.projects[project].parked.iter().find(|p| &p.branch == b).map(|p| p.sessions.clone()).unwrap_or_default(),
            _ => Vec::new(),
        };
        self.claim_resuming(project, branch, for_pane, resume)
    }

    /// [`claim_in`](Self::claim_in), bringing back the Claude `resume` sessions in the slot.
    pub(super) fn claim_resuming(&mut self, project: usize, branch: Option<String>, for_pane: Option<(u64, UnixStream)>, resume: Vec<String>) -> Option<String> {
        let Some(pool) = self.projects[project].pool.clone() else {
            if let Some((_, mut stream)) = for_pane {
                let _ = ServerMsg::Reply("error: this folder isn't a git repo, so it has no slots".into()).write(&mut stream);
            }
            self.say("scratch sessions aren't git repos, so they have no slots");
            return None;
        };
        let tx = self.events.clone();
        self.job_seq += 1;
        let job = format!("claim {}", self.job_seq);
        self.jobs.push((job.clone(), format!("claiming {}", branch.as_deref().unwrap_or("a slot"))));
        let id = job.clone();
        std::thread::spawn(move || {
            let progress = {
                let (tx, job) = (tx.clone(), job.clone());
                move |text: &str| {
                    let _ = tx.send(Event::Progress { project, job: job.clone(), text: text.to_string() });
                }
            };
            let result = (|| {
                if pool.status()?.is_empty() {
                    pool.init(&|_| {})?;
                }
                let opts = ClaimOpts { branch, by: Some("velmux".into()), fetch: true, install: true };
                pool.claim(&opts, &progress)
            })()
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Claimed { project, job, result, for_pane, resume });
        });
        Some(id)
    }

    /// The pane where conversation `id` is running right now (Claude in the
    /// foreground: a pane whose Claude exited keeps the id, but is a shell).
    pub fn live_session_pane(&self, id: &str, project: Option<usize>) -> Option<u64> {
        self.panes
            .iter()
            .find(|p| p.session.as_deref() == Some(id) && p.agent.is_some() && project.is_none_or(|i| p.loc.project == i))
            .map(|p| p.id)
    }

    /// A resumed branch's Claude, in its new slot: the conversation itself if
    /// it's still open (back in the base after `velmux release`), moved over;
    /// else the session resumed from its transcript; else a shell there.
    pub(super) fn resume_sessions(&mut self, loc: Loc, sessions: &[String]) {
        for id in sessions {
            if let Some(pane) = self.live_session_pane(id, Some(loc.project)) {
                self.move_pane(pane, loc.clone());
                self.focus = Some(pane);
                self.current = Some(loc);
                self.full_redraw = true;
                return;
            }
        }
        let resumable = sessions.iter().find(|id| crate::agents::exists(id, &self.claude_dir)).cloned();
        let Some(id) = resumable else { return self.spawn(loc, Kind::Shell, None) };
        match self.launch(&loc, Kind::Agent, LaunchOpts { resume: Some(id), ..LaunchOpts::default() }) {
            Ok(pane) => {
                self.insert_into_view(&loc, pane, None);
                self.focus = Some(pane);
                self.current = Some(loc);
                self.relayout();
            }
            Err(e) => self.say(e),
        }
    }

    /// Sync every claimed slot that's behind, one after another, in the background.
    pub(super) fn sync_all(&mut self) {
        let Some(project) = self.current_loc().map(|l| l.project) else { return };
        let Some(pool) = self.projects[project].pool.clone() else { return self.say("scratch sessions have no slots") };
        let ids: Vec<String> = self.projects[project].slots.iter().filter(|s| s.status == Status::InUse && s.behind > 0).map(|s| s.id.clone()).collect();
        if ids.is_empty() {
            return self.toast("every slot is up to date");
        }
        self.job_seq += 1;
        let job = format!("sync {}", self.job_seq);
        self.jobs.push((job.clone(), format!("syncing {} slots", ids.len())));
        let tx = self.events.clone();
        std::thread::spawn(move || {
            let results: Vec<String> = ids.iter().map(|id| pool.sync(id, &|_| {}).unwrap_or_else(|e| format!("{id}: {e:#}"))).collect();
            let _ = tx.send(Event::Synced { job, result: Ok(results.join(" · ")) });
        });
    }

    /// Rebase (or merge) a slot's branch onto the base ref, in the background.
    pub(super) fn sync_slot(&mut self) {
        let Some(loc) = self.current_loc() else { return };
        let (Some(slot), Some(pool)) = (loc.slot.clone(), self.projects[loc.project].pool.clone()) else {
            return self.say("focus a slot to sync it with the base");
        };
        self.job_seq += 1;
        let job = format!("sync {}", self.job_seq);
        self.jobs.push((job.clone(), format!("syncing {slot}")));
        let tx = self.events.clone();
        std::thread::spawn(move || {
            let result = pool.sync(&slot, &|_| {}).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Synced { job, result });
        });
    }

    /// After a fetch: say what fast-forwarding the checkout did, and which
    /// slots that left behind (once per outcome, not every fetch).
    pub(super) fn base_updated(&mut self, project: usize, update: velmux_pool::BaseUpdate) {
        use velmux_pool::BaseUpdate as U;
        let p = &mut self.projects[project];
        if p.base_said.as_ref() == Some(&update) && update != U::UpToDate {
            return;
        }
        let first = p.base_said.is_none();
        p.base_said = Some(update.clone());
        let name = p.name().to_string();
        let base_ref = p.pool.as_ref().map(|pool| pool.base_ref().to_string()).unwrap_or_default();
        let behind: Vec<String> = p.slots.iter().filter(|s| s.status == Status::InUse && s.behind > 0).map(|s| s.id.clone()).collect();
        let slots = match behind.len() {
            0 => String::new(),
            1 => format!(" · {} is behind: sync it from its menu", behind[0]),
            n => format!(" · {n} slots are behind: right-click {name} › sync all slots"),
        };
        match update {
            U::FastForwarded(n) => {
                let msg = format!("{name}: checkout fast-forwarded {n} commit{} to {base_ref}{slots}", if n == 1 { "" } else { "s" });
                self.toast(&msg);
                self.say(msg);
            }
            U::Blocked(n) => self.say(format!("{name}: checkout is {n} behind {base_ref}, but its local changes are in the way (move them to a slot)")),
            U::Diverged { behind, ahead } if !first || behind > 0 => {
                self.say(format!("{name}: checkout has {ahead} commit(s) of its own and is {behind} behind {base_ref}; left as it is"))
            }
            _ => {}
        }
    }
}
