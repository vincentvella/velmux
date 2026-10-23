//! The menus: right-click menus for places, panes and parked work, and running their items.

use super::*;

impl App {
    /// Right-click on a sidebar row: the menu of things to do in that place.
    pub(super) fn open_context_menu(&mut self, x: u16, y: u16) {
        let Some((_, row)) = ui::sidebar_rows(self, self.size.1).into_iter().find(|(ry, _)| *ry == y) else { return };
        if let ui::Row::ParkedWorkstream(i) = row {
            let Some(pw) = self.parked_workstreams.get(i) else { return };
            let items = vec![("resume workstream", Action::ResumeParkedWorkstream(i as u8)), ("forget it (branches stay parked)", Action::ForgetParkedWorkstream(i as u8))];
            let first_repo = || self.projects.iter().position(|p| !p.removed && !p.is_unclassified()).map(|p| Loc { project: p, slot: None });
            let Some(target) = self.current_loc().or_else(first_repo) else { return };
            let title = format!("parked · {}", pw.name);
            self.mode = Mode::Menu { selected: 0, context: Some(MenuContext { pane: None, target, parked: None, title, items, at: (x, y) }) };
            return;
        }
        if let ui::Row::Parked(p, i) = row {
            let Some(branch) = self.projects[p].parked.get(i).map(|v| v.branch.clone()) else { return };
            let items = vec![("resume in a slot", Action::ResumeParked), ("discard changes…", Action::DiscardParked)];
            let context = MenuContext { pane: None, target: Loc { project: p, slot: None }, parked: Some(branch.clone()), title: format!("parked · {branch}"), items, at: (x, y) };
            self.mode = Mode::Menu { selected: 0, context: Some(context) };
            return;
        }
        let target = match row {
            ui::Row::Location(loc) | ui::Row::Detail(loc) => loc,
            ui::Row::Project(p) if !self.projects[p].is_unclassified() => Loc { project: p, slot: None },
            ui::Row::Agent { pane, .. } => self.panes[pane].loc.clone(),
            _ => return,
        };
        self.open_place_menu(target, (x, y));
    }

    /// A place's menu (a repo's checkout, a slot, a workstream, a scratch
    /// session), at `at`.
    pub(super) fn open_place_menu(&mut self, target: Loc, at: (u16, u16)) {
        let (x, y) = at;
        let folder = self.projects[target.project].is_unclassified();
        let items: Vec<(&'static str, Action)> = match (folder, target.slot.is_some()) {
            // The space: start work (Claude plans in the base and claims its
            // own slot), or take a slot yourself; manage the repo.
            (false, false) => {
                let p = &self.projects[target.project];
                let mut items = Vec::new();
                if p.base_changes > 0 {
                    items.push(("move changes to a slot…", Action::MoveToSlot));
                }
                items.push(("new conversation", Action::AgentBase));
                if crate::agents::ALL.iter().filter(|a| a.installed()).count() > 1 {
                    items.push(("new conversation with…", Action::AgentPicker));
                }
                items.extend([
                ("claim a slot…", Action::ClaimSlot),
                ("shell in base", Action::NewShell),
                ("open base in editor", Action::NewEditor),
                ("rename space", Action::RenameLocation),
                ("remove repo", Action::RemovePlace),
                ]);
                if p.slots.iter().any(|s| s.status == Status::InUse && s.behind > 0) {
                    items.insert(items.len() - 2, ("sync all slots", Action::SyncAll));
                }
                items
            }
            // A slot: work on what's already there, get it reviewed, hand it back.
            (false, true) => {
                use velmux_pool::gh::{Checks, PrState};
                let project = &self.projects[target.project];
                let branch = project.slots.iter().find(|s| Some(&s.id) == target.slot.as_ref()).and_then(|s| s.branch.clone());
                let pr = branch.as_ref().and_then(|b| project.prs.get(b));
                let mut items = Vec::new();
                if pr.is_some_and(|p| p.state == PrState::Merged) {
                    items.push(("release (PR merged)", Action::RemovePlace));
                }
                items.push(("sync with base", Action::SyncSlot));
                // Workstreams: join one (or start one) with this slot, or leave its own.
                if self.workstream_of(&target).is_some() {
                    items.push(("leave workstream", Action::LeaveWorkstream));
                } else {
                    items.push(("add to workstream…", Action::AddToWorkstream(u8::MAX)));
                }
                match pr {
                    Some(p) => {
                        items.push(("open PR", Action::OpenPr));
                        if p.checks == Checks::Failing {
                            items.push(("send failures to Claude", Action::SendFailures));
                        }
                        items.extend([("open checks", Action::OpenChecks), ("copy PR link", Action::CopyPrLink)]);
                    }
                    None if branch.is_some() => items.push(("push & open PR", Action::OpenPr)),
                    None => {}
                }
                items.extend([
                    ("open in browser", Action::OpenBrowser),
                    ("shell here", Action::NewShell),
                    ("new tab", Action::NewTab),
                    ("open in editor", Action::NewEditor),
                    ("copy path", Action::CopyPath),
                    ("rename slot", Action::RenameLocation),
                ]);
                if !pr.is_some_and(|p| p.state == PrState::Merged) {
                    items.push(("release slot", Action::RemovePlace));
                }
                items
            }
            (true, _) if self.is_workstream(&target) => {
                let mut items = vec![("claim a slot in a repo…", Action::ClaimIntoWorkstream(u8::MAX))];
                if !self.workstream_prs(&target).is_empty() {
                    items.push(("open all PRs", Action::OpenWorkstreamPrs));
                }
                if !self.members(&target).is_empty() {
                    items.push(("sync all slots", Action::SyncWorkstream));
                    if self.workstream_prs(&target).len() < self.members(&target).len() {
                        items.push(("push & open PRs", Action::PushWorkstream));
                    }
                }
                items.extend([
                ("claude here", Action::AgentHere),
                ("claude here", Action::AgentHere),
                ("shell here", Action::NewShell),
                ("new tab", Action::NewTab),
                ("open in editor", Action::NewEditor),
                ("copy path", Action::CopyPath),
                ("rename workstream", Action::RenameLocation),
                ("release workstream…", Action::ReleaseWorkstream),
                ("remove workstream", Action::RemovePlace),
                ]);
                items
            }
            (true, _) => vec![
                ("claude here", Action::AgentHere),
                ("shell here", Action::NewShell),
                ("new tab", Action::NewTab),
                ("open in editor", Action::NewEditor),
                ("copy path", Action::CopyPath),
                ("rename session", Action::RenameLocation),
                ("remove session", Action::RemovePlace),
            ],
        };
        let title = self.loc_name(&target);
        self.mode = Mode::Menu { selected: 0, context: Some(MenuContext { pane: None, target, parked: None, title, items, at: (x, y) }) };
    }

    /// A pane's right-click menu: split it, zoom it, copy from it, close it.
    pub(super) fn open_pane_menu(&mut self, id: u64, x: u16, y: u16) {
        let (Some(loc), Some(pane)) = (self.current_loc(), self.pane(id)) else { return };
        let title = ui::pane_label(self, pane);
        let view = self.views.get(&loc);
        let zoomed = view.is_some_and(|v| v.zoomed == Some(id));
        let split = view.is_some_and(|v| v.layout.leaves().len() > 1);
        let mut items: Vec<(&'static str, Action)> = vec![("split right", Action::SplitRight), ("split down", Action::SplitDown)];
        // A second conversation beside this one.
        items.push(("conversation to the right", Action::SplitConversation));
        if crate::agents::ALL.iter().filter(|a| a.installed()).count() > 1 {
            items.push(("agent to the right…", Action::SplitAgent(u8::MAX)));
        }
        if zoomed {
            items.push(("unzoom", Action::Zoom));
        } else if split {
            items.push(("zoom", Action::Zoom));
        }
        items.extend([("copy mode", Action::CopyMode), ("close pane", Action::ClosePane)]);
        // And the place it's in (its slot's, repo's or workstream's menu).
        items.push((self.intern(&format!("{}…", self.loc_name(&loc))), Action::PlaceMenu));
        self.focus = Some(id);
        self.mode = Mode::Menu { selected: 0, context: Some(MenuContext { pane: Some(id), target: loc, parked: None, title, items, at: (x, y) }) };
    }

    /// Run menu row `i`: the global menu's action here, or a context menu's
    /// action in its place (going there first).
    pub(super) fn run_menu_item(&mut self, context: Option<MenuContext>, i: usize) {
        match context {
            Some(c) => {
                let Some(&(_, action)) = c.items.get(i) else { return };
                // A menu this opens (a picker) shows where this one was.
                self.menu_at = Some(c.at);
                if let Some(branch) = c.parked {
                    let project = c.target.project;
                    return match action {
                        Action::ResumeParked => {
                            self.claim_in(project, Some(branch), None);
                        }
                        _ => {
                            self.mode = Mode::Confirm {
                                label: format!("discard the parked changes on {branch}? commits on the branch are kept"),
                                action: Confirm::DiscardParked { project, branch },
                            }
                        }
                    };
                }
                if self.current_loc().as_ref() != Some(&c.target) {
                    if self.copy.is_some() {
                        self.exit_copy();
                    }
                    self.go_to(c.target);
                }
                if let Some(id) = c.pane.filter(|id| self.pane(*id).is_some()) {
                    self.focus = Some(id);
                }
                self.run(action);
                self.menu_at = None;
            }
            None => {
                if let Some(&(_, action)) = ui::MENU.get(i) {
                    self.run(action);
                }
            }
        }
    }

    pub(super) fn menu_input(&mut self, bytes: &[u8]) -> usize {
        let Mode::Menu { selected, context } = &mut self.mode else { return bytes.len() };
        let n = context.as_ref().map_or(ui::MENU.len(), |c| c.items.len());
        let (used, key) = match bytes {
            [0x1b, b'[', b'A', ..] => (3, b'k'),
            [0x1b, b'[', b'B', ..] => (3, b'j'),
            [0x1b, b'[', rest @ ..] => {
                let len = rest.iter().position(|b| (0x40..=0x7e).contains(b)).map_or(bytes.len(), |i| i + 3);
                (len, 0)
            }
            [b, ..] => (1, *b),
            [] => return 0,
        };
        match key {
            b'k' | 0x10 => *selected = (*selected + n - 1) % n,
            b'j' | 0x0e => *selected = (*selected + 1) % n,
            b'\r' | b'\n' => {
                let i = *selected;
                let Mode::Menu { context, .. } = std::mem::replace(&mut self.mode, Mode::Normal) else { unreachable!() };
                self.run_menu_item(context, i);
            }
            0x1b | 0x03 | b'q' => self.mode = Mode::Normal,
            _ => {}
        }
        used
    }

    /// `<prefix> space`: the menu for what's focused, from the keyboard: the
    /// focused pane's (it leads on to its place's), else the place's.
    pub(super) fn open_focused_menu(&mut self) {
        let side = ui::sidebar_width(self.size.0, self.sidebar_hidden, self.sidebar_cols);
        let at = (side + 2, 2);
        match (self.focus, self.current_loc()) {
            (Some(id), _) => self.open_pane_menu(id, at.0, at.1),
            (None, Some(loc)) => self.open_place_menu(loc, at),
            (None, None) => self.mode = Mode::Menu { selected: 0, context: None },
        }
    }
}
