//! Pull requests: checking them, noticing changes, and opening PRs, checks and links.

use super::*;

impl App {
    /// Check PRs for every GitHub repo with claimed or parked branches, about
    /// once a minute (sooner after opening one), in the background.
    pub(super) fn refresh_prs_bg(&mut self) {
        // About once a minute (`VELMUX_PR_EVERY` seconds, for tests).
        let every = std::env::var("VELMUX_PR_EVERY").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
        if self.checking_prs || self.prs_checked.is_some_and(|t| t.elapsed() < Duration::from_secs(every)) || !velmux_pool::gh::available() {
            return;
        }
        let work: Vec<(usize, Arc<Pool>)> = self
            .projects
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.removed && (p.slots.iter().any(|s| s.status == Status::InUse) || !p.parked.is_empty()))
            .filter_map(|(i, p)| p.pool.clone().map(|pool| (i, pool)))
            .collect();
        if work.is_empty() {
            return;
        }
        self.checking_prs = true;
        self.prs_checked = Some(Instant::now());
        let tx = self.events.clone();
        std::thread::spawn(move || {
            let fresh = work
                .into_iter()
                .filter(|(_, pool)| velmux_pool::gh::on_github(&pool.base))
                .filter_map(|(i, pool)| velmux_pool::gh::prs(&pool.base).ok().map(|prs| (i, prs)))
                .collect();
            let _ = tx.send(Event::Prs(fresh));
        });
    }

    /// Tell you when a claimed branch's PR merges or its checks fail.
    pub(super) fn pr_changes(&mut self, project: usize, fresh: &HashMap<String, velmux_pool::gh::Pr>) {
        use velmux_pool::gh::{Checks, PrState};
        let Some(p) = self.projects.get(project) else { return };
        let mut news = Vec::new();
        for slot in p.slots.iter().filter(|s| s.status == Status::InUse) {
            let Some(branch) = &slot.branch else { continue };
            let (Some(old), Some(new)) = (p.prs.get(branch), fresh.get(branch)) else { continue };
            if old.state != PrState::Merged && new.state == PrState::Merged {
                news.push((format!("#{} merged ({branch}) — release {} from its menu", new.number, slot.id), true));
            } else if old.checks != Checks::Failing && new.checks == Checks::Failing {
                news.push((format!("checks failed on #{} ({branch}): {}", new.number, new.failed.join(", ")), false));
            }
        }
        for (body, good) in news {
            self.toast(&body);
            self.notify_text(&body, good);
        }
    }

    /// A desktop notification and sound (good news: the done sound).
    pub(super) fn notify_text(&mut self, body: &str, good: bool) {
        use crate::notify;
        if self.notify.desktop {
            if self.client.is_some() {
                self.pending_out.extend_from_slice(&notify::osc777("velmux", body));
                self.dirty = true;
            } else {
                notify::system("velmux", body);
            }
        }
        if self.notify.sound {
            notify::play(if good { &self.notify.done_sound } else { &self.notify.blocked_sound });
        }
    }

    /// The current slot's branch and its PR, if it has one.
    pub(super) fn current_pr(&self) -> (Option<Loc>, Option<String>, Option<velmux_pool::gh::Pr>) {
        let Some(loc) = self.current_loc() else { return (None, None, None) };
        let project = &self.projects[loc.project];
        let branch = loc.slot.as_ref().and_then(|id| project.slots.iter().find(|s| &s.id == id)).and_then(|s| s.branch.clone());
        let pr = branch.as_ref().and_then(|b| project.prs.get(b)).cloned();
        (Some(loc), branch, pr)
    }

    /// Open the slot's PR, or push the branch and open GitHub's new-PR page.
    pub(super) fn open_pr(&mut self) {
        let (loc, branch, pr) = self.current_pr();
        if let Some(pr) = pr {
            return self.open_url(&pr.url);
        }
        let (Some(loc), Some(branch)) = (loc, branch) else { return self.say("focus a slot on a branch to open its PR") };
        if !velmux_pool::gh::available() {
            return self.say("install the GitHub CLI (gh) for PRs");
        }
        let Some(dir) = self.loc_cwd(&loc) else { return };
        self.job_seq += 1;
        let job = format!("pr {}", self.job_seq);
        self.jobs.push((job.clone(), format!("pushing {branch}")));
        let tx = self.events.clone();
        std::thread::spawn(move || {
            let result = velmux_pool::gh::push_and_open(&dir, &branch).map(|_| format!("pushed {branch}; finish the PR in your browser")).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::PrOpened { job, result });
        });
    }

    pub(super) fn pr_action(&mut self, action: Action) {
        let (loc, _, pr) = self.current_pr();
        let Some(pr) = pr else { return self.say("this slot's branch has no PR yet") };
        match action {
            Action::OpenChecks => self.open_url(&format!("{}/checks", pr.url)),
            Action::CopyPrLink => {
                self.set_clipboard(&pr.url);
                self.toast("PR link copied");
            }
            _ => {
                // Hand the failures to the slot's Claude, as if you'd typed it.
                let Some(loc) = loc else { return };
                let Some(pane) = self.panes.iter_mut().find(|p| p.loc == loc && (p.agent.is_some() || p.session.is_some())) else {
                    return self.say("no Claude in this slot to send it to");
                };
                let failed = if pr.failed.is_empty() { "some checks".to_string() } else { pr.failed.join(", ") };
                let text = format!(
                    "CI failed on PR #{} ({failed}). Run `gh pr checks {}` and `gh run view --log-failed` to see why, then fix it.",
                    pr.number, pr.number
                );
                let id = pane.id;
                pane.write_input(format!("\x1b[200~{text}\x1b[201~").as_bytes());
                pane.write_input(b"\r");
                self.focus = Some(id);
                self.toast("sent the failures to Claude");
            }
        }
    }

    /// Open a URL in the default browser (or `$BROWSER`).
    pub(super) fn open_url(&mut self, url: &str) {
        let url = url.to_string();
        let opener = std::env::var("BROWSER").ok().filter(|b| !b.is_empty()).unwrap_or_else(|| if cfg!(target_os = "macos") { "open" } else { "xdg-open" }.into());
        match std::process::Command::new(&opener).arg(&url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
                self.toast(&format!("opened {url}"));
            }
            Err(e) => self.say(format!("can't open {url}: {e}")),
        }
    }

    /// `http://localhost:<port>` for the current slot, in the default browser.
    pub(super) fn open_browser(&mut self) {
        let Some(port) = self.current_loc().and_then(|l| self.loc_port(&l)) else {
            return self.say("only slots have their own port (see [ports] in the config)");
        };
        self.open_url(&format!("http://localhost:{port}"));
    }
}
