//! Agent conversations: the search index, finding and resuming one, and
//! naming them after their slot.

use super::*;

impl App {
    /// Name Claude conversations after where they are, as `/rename` would
    /// (the `/resume` picker, and the prompt box once resumed): a slot's after
    /// its name if you gave it one, else its branch; a workstream's after it;
    /// one in a repo's checkout `<repo> · <Claude's own title>`. Ones you've named yourself are left
    /// alone: velmux only renames a conversation that has no name, or still
    /// has the one velmux gave it.
    /// A session's transcript, found once and remembered.
    fn transcript_of(&self, session: &str) -> Option<PathBuf> {
        let mut known = self.transcripts.borrow_mut();
        if let Some(path) = known.get(session).filter(|p| p.is_file()) {
            return Some(path.clone());
        }
        let path = claude::transcript(&self.claude_dir, session)?;
        known.insert(session.to_string(), path.clone());
        Some(path)
    }

    /// Claude's own title for a transcript, read again only when it's changed.
    fn ai_title_of(&self, file: &Path) -> Option<String> {
        let modified = std::fs::metadata(file).and_then(|m| m.modified()).ok()?;
        let mut known = self.ai_titles.borrow_mut();
        if let Some((_, title)) = known.get(file).filter(|(at, _)| *at == modified) {
            return title.clone();
        }
        let title = claude::ai_title(file);
        known.insert(file.to_path_buf(), (modified, title.clone()));
        title
    }

    pub(super) fn name_conversations(&mut self) {
        let mut todo = Vec::new();
        for pane in &self.panes {
            let (Some(session), Some("claude")) = (&pane.session, pane.agent) else { continue };
            let project = &self.projects[pane.loc.project];
            if !project.cfg.name_conversations {
                continue;
            }
            // Its transcript appears with its first message: until then, next time.
            let Some(file) = self.transcript_of(session) else { continue };
            let name = self.views.get(&pane.loc).and_then(|v| v.name.clone());
            let want = match &pane.loc.slot {
                // A workstream's: its name, as a slot's is its branch.
                Some(id) if self.is_workstream(&pane.loc) => Some(name.unwrap_or_else(|| id.clone())),
                Some(_) if project.is_unclassified() => None, // a scratch session's
                Some(slot) => name.or_else(|| project.slots.iter().find(|s| &s.id == slot).and_then(|s| s.branch.clone())),
                // In a repo's checkout, where many conversations plan different
                // things: the repo, then Claude's own title once it has one.
                None => Some(match self.ai_title_of(&file) {
                    Some(title) => format!("{} · {title}", project.name()),
                    None => project.name().to_string(),
                }),
            };
            let Some(want) = want else { continue };
            if self.named.get(session) != Some(&want) {
                todo.push((session.clone(), file, want, self.named.get(session).cloned()));
            }
        }
        if todo.is_empty() {
            return;
        }
        for (session, _, want, _) in &todo {
            self.named.insert(session.clone(), want.clone());
        }
        if let Ok(dir) = crate::paths::state_dir() {
            let _ = std::fs::write(dir.join("conversation-names.json"), serde_json::to_vec(&self.named).unwrap_or_default());
        }
        // Reading a long transcript for its current name: off the main thread.
        std::thread::spawn(move || {
            for (session, file, want, ours) in todo {
                let current = claude::custom_title(&file);
                if (current.is_none() || current == ours) && current.as_ref() != Some(&want) {
                    let _ = claude::set_title(&file, &session, &want);
                }
            }
        });
    }

    /// Claude's transcript folders for every repo (base and slots) and folder.
    pub(super) fn transcript_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        for project in self.projects.iter().filter(|p| !p.removed) {
            if let Some(base) = project.base() {
                dirs.push(claude::project_dir(&self.claude_dir, base));
            }
            dirs.extend(project.slots.iter().map(|s| claude::project_dir(&self.claude_dir, &s.path)));
            dirs.extend(project.folders.iter().map(|f| claude::project_dir(&self.claude_dir, &f.path)));
        }
        dirs.sort();
        dirs.dedup();
        dirs
    }

    /// Rebuild the conversation index in the background (unchanged files are reused).
    pub(super) fn reindex_sessions(&mut self) {
        if self.indexing {
            return;
        }
        self.indexing = true;
        let (dirs, old, tx) = (self.transcript_dirs(), self.sessions.clone(), self.events.clone());
        // Codex and Gemini keep every project's chats together: keep the ones
        // that ran in velmux's places.
        let mut roots: Vec<PathBuf> = Vec::new();
        for project in self.projects.iter().filter(|p| !p.removed) {
            roots.extend(project.base().map(Path::to_path_buf));
            roots.extend(project.slots.iter().map(|s| s.path.clone()));
            roots.extend(project.folders.iter().map(|f| f.path.clone()));
        }
        std::thread::spawn(move || {
            let mut docs = crate::search::scan_others(&roots, &old);
            docs.extend(crate::search::scan(&dirs, old));
            docs.sort_by_key(|d| std::cmp::Reverse(d.mtime));
            let _ = tx.send(Event::Indexed(docs));
        });
    }

    /// Where a conversation ran, as a place velmux knows: (place, its slot is
    /// still on the conversation's branch).
    pub fn session_loc(&self, doc: &crate::search::Doc) -> Option<(Loc, bool)> {
        let cwd = doc.cwd.as_ref()?;
        for (i, project) in self.projects.iter().enumerate().filter(|(_, p)| !p.removed) {
            for s in &project.slots {
                if cwd.starts_with(&s.path) {
                    let same = s.status == Status::InUse && (doc.branch.is_none() || s.branch == doc.branch);
                    return Some((Loc { project: i, slot: Some(s.id.clone()) }, same));
                }
            }
            if project.base().is_some_and(|b| cwd.starts_with(b)) {
                return Some((Loc { project: i, slot: None }, true));
            }
            if let Some(f) = project.folders.iter().find(|f| cwd.starts_with(&f.path)) {
                return Some((Loc { project: i, slot: Some(f.id.clone()) }, true));
            }
        }
        None
    }

    /// Open a conversation: its pane if it's running; else resumed where it
    /// ran (its slot, if still on that branch), by resuming the branch if it's
    /// parked, or in the repo's base.
    pub(super) fn open_session(&mut self, doc: &crate::search::Doc) {
        if let Some(pane) = self.live_session_pane(&doc.id, None) {
            if self.copy.is_some() {
                self.exit_copy();
            }
            self.focus = Some(pane);
            self.current = self.pane(pane).map(|p| p.loc.clone());
            self.reveal_focus();
            self.full_redraw = true;
            return;
        }
        let Some((loc, same)) = self.session_loc(doc) else {
            return self.say("that conversation ran somewhere velmux doesn't know");
        };
        let unclassified = self.projects[loc.project].is_unclassified();
        let parked = doc.branch.as_ref().filter(|b| self.projects[loc.project].parked.iter().any(|p| &p.branch == *b));
        let target = match (unclassified, loc.slot.is_some() && same, parked) {
            (true, _, _) | (false, true, _) => loc,
            (false, false, Some(branch)) => {
                let branch = branch.clone();
                self.claim_resuming(loc.project, Some(branch), None, vec![doc.id.clone()]);
                return;
            }
            (false, false, None) => Loc { project: loc.project, slot: None },
        };
        self.say(format!("resuming “{}” in {}", doc.title, self.loc_name(&target)));
        self.resume_sessions(target, std::slice::from_ref(&doc.id));
    }

    pub(super) fn sessions_input(&mut self, bytes: &[u8]) -> usize {
        let Mode::Sessions { query, selected } = &mut self.mode else { return bytes.len() };
        let count = crate::search::search(&self.sessions, query, SESSION_HITS).len();
        let (used, key) = match bytes {
            [0x1b, b'[', b'A', ..] => (3, Some(0x10)),
            [0x1b, b'[', b'B', ..] => (3, Some(0x0e)),
            [0x1b, b'[', rest @ ..] => {
                let len = rest.iter().position(|b| (0x40..=0x7e).contains(b)).map_or(bytes.len(), |i| i + 3);
                (len, None)
            }
            [0x1b] => (1, Some(0x1b)),
            [b, ..] if b.is_ascii_control() || *b == 0x7f => (1, Some(*b)),
            _ => {
                let end = bytes.iter().position(|b| b.is_ascii_control()).unwrap_or(bytes.len());
                query.push_str(&String::from_utf8_lossy(&bytes[..end]));
                *selected = 0;
                return end;
            }
        };
        match key {
            Some(0x10) => *selected = selected.checked_sub(1).unwrap_or(count.saturating_sub(1)),
            Some(0x0e) => *selected = if count == 0 { 0 } else { (*selected + 1) % count },
            Some(0x7f | 0x08) => {
                query.pop();
                *selected = 0;
            }
            Some(0x15) => {
                query.clear();
                *selected = 0;
            }
            Some(0x1b | 0x03) => self.mode = Mode::Normal,
            Some(b'\r' | b'\n') => {
                let doc = crate::search::search(&self.sessions, query, SESSION_HITS).get(*selected).map(|h| h.doc.clone());
                self.mode = Mode::Normal;
                if let Some(doc) = doc {
                    self.open_session(&doc);
                }
            }
            _ => {}
        }
        used
    }
}
