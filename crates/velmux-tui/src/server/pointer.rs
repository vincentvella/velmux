//! Mouse input: clicks, drags (selections, dividers, the sidebar's edge,
//! reordering, dropping slots on workstreams) and the sidebar's click targets.

use super::*;

impl App {
    /// What mouse reporting to ask the terminal for.
    pub(super) fn mouse_mode(&self) -> &'static [u8] {
        if self.focus_follows_mouse { mouse::ENABLE_HOVER } else { mouse::ENABLE }
    }

    /// Focus follows mouse: the pane under the pointer, as it moves (only in
    /// the pane area, and not while a menu, picker or copy mode is open).
    pub(super) fn hover(&mut self, ev: MouseEvent) {
        if !self.focus_follows_mouse || !matches!(self.mode, Mode::Normal) || self.copy.is_some() {
            return;
        }
        let Some(loc) = self.current_loc() else { return };
        let at = Position::new(ev.x, ev.y);
        let (rects, _) = self.view_rects(&loc);
        if let Some((id, _)) = rects.into_iter().find(|(_, r)| r.contains(at)) {
            if self.focus != Some(id) {
                self.focus = Some(id);
                self.dirty = true;
            }
        }
    }

    pub(super) fn mouse_event(&mut self, ev: MouseEvent) {
        // Plain movement is only reported for focus follows mouse; it never
        // reaches the apps in panes (they asked for clicks and drags).
        if ev.is_hover() && self.drag.is_none() {
            return self.hover(ev);
        }
        self.dirty = true;
        if let Some(drag) = self.drag.take() {
            return self.continue_drag(drag, ev);
        }
        let (cols, rows) = self.size;
        let at = Position::new(ev.x, ev.y);
        if let Mode::Menu { .. } = self.mode {
            if ev.is_press() && (ev.is_left() || ev.is_right()) {
                let area = ui::menu_area(self, cols, rows);
                let item = (ev.is_left() && area.contains(at) && ev.y > area.y).then(|| (ev.y - area.y - 1) as usize);
                let Mode::Menu { context, .. } = std::mem::replace(&mut self.mode, Mode::Normal) else { unreachable!() };
                match item {
                    Some(i) => self.run_menu_item(context, i),
                    // Right-clicking another place opens its menu instead.
                    None if ev.is_right() && !area.contains(at) && ev.x < ui::sidebar_width(cols, self.sidebar_hidden, self.sidebar_cols) => {
                        self.open_context_menu(ev.x, ev.y)
                    }
                    None => {}
                }
            }
            return;
        }
        if !matches!(self.mode, Mode::Normal | Mode::Copy) {
            return;
        }
        let side = ui::sidebar_width(cols, self.sidebar_hidden, self.sidebar_cols);
        // Its edge drags to resize it.
        if !self.sidebar_hidden && ev.x == side && ev.is_press() && ev.is_left() {
            self.drag = Some(Drag::SidebarEdge);
            return;
        }
        if ev.x < side {
            if ev.is_press() && ev.is_right() {
                return self.open_context_menu(ev.x, ev.y);
            }
            if ev.is_press() && ev.is_left() {
                // Repo headings and folders can be dragged; decide on release.
                match self.grab_at(ev.y) {
                    Some(grab) => self.drag = Some(Drag::Sidebar { grab, start: (ev.x, ev.y), moved: false }),
                    None => self.sidebar_click(ev.x, ev.y),
                }
            }
            return;
        }
        if ev.y == 0 {
            // Scrolling over the header cycles tabs.
            if let Some(step) = ev.wheel() {
                if self.current_loc().and_then(|l| self.views.get(&l)).is_some_and(|v| v.tabs.len() > 1) {
                    self.step_tab(step);
                }
                return;
            }
            if ev.is_press() && ev.is_left() {
                let body = ui::pane_area(cols, rows, self.sidebar_hidden, self.sidebar_cols);
                match ui::tab_hit(self, body, ev.x) {
                    Some(ui::TabHit::Tab(i)) => self.goto_tab(i),
                    Some(ui::TabHit::New) => self.new_tab(),
                    None => {}
                }
            }
            return;
        }
        // An empty screen: its actions are clickable.
        if self.focused().is_none() && ev.is_press() && ev.is_left() {
            let body = ui::pane_area(cols, rows, self.sidebar_hidden, self.sidebar_cols);
            if let Some(action) = ui::empty_hit(self, body, ev.x, ev.y) {
                if action == Action::Help {
                    self.mode = Mode::Prefix;
                } else {
                    self.run(action);
                }
            }
            return;
        }
        if self.sidebar_hidden && ev.y + 1 >= rows && (self.status_text().is_some() || !self.jobs.is_empty()) {
            return; // the status strip
        }
        let Some(loc) = self.current_loc() else { return };
        let (rects, dividers) = self.view_rects(&loc);
        if ev.is_press() && ev.is_left() {
            if let Some(d) = dividers.into_iter().find(|d| d.rect.contains(at)) {
                self.drag = Some(Drag::Divider { loc, path: d.path, span: d.span, dir: d.dir });
                return;
            }
        }
        let titled = self.titled(&loc);
        let Some((id, outer)) = rects.into_iter().find(|(_, r)| r.contains(at)) else { return };
        if (ev.is_press() || ev.wheel().is_some()) && self.focus != Some(id) {
            if self.copy.is_some() {
                self.exit_copy();
            }
            self.focus = Some(id);
        }
        let inner = ui::content_rect(outer, titled);
        // Right-click: the pane's menu, unless it's in the content of an app
        // that takes the mouse itself (vim, not an agent); its title bar
        // always gives the menu.
        if ev.is_press() && ev.is_right() {
            let copying = self.copy.as_ref().is_some_and(|c| c.pane == id);
            // Agents (Claude Code turns the mouse on) don't use right-click: theirs is the menu.
            let app_mouse = self.pane(id).is_some_and(|p| p.agent.is_none() && p.parser.screen().mouse_protocol_mode() != MouseProtocolMode::None);
            if !(inner.contains(at) && app_mouse && !copying) {
                return self.open_pane_menu(id, ev.x, ev.y);
            }
        }
        if !inner.contains(at) {
            return; // title bar: focusing was all
        }
        let (lx, ly) = (ev.x - inner.x, ev.y - inner.y);
        let copying = self.copy.as_ref().is_some_and(|c| c.pane == id);
        let Some(pane) = self.panes.iter_mut().find(|p| p.id == id) else { return };
        let screen = pane.parser.screen();
        let app_mode = screen.mouse_protocol_mode();
        let clicks = self.count_click(id, (lx, ly), &ev);
        if clicks >= 2 && ev.is_left() {
            // Double-click copies a word, triple-click a line, in any app.
            return self.click_select(id, lx, ly, clicks);
        }
        let Some(pane) = self.panes.iter_mut().find(|p| p.id == id) else { return };
        let screen = pane.parser.screen();
        if !copying && app_mode != MouseProtocolMode::None {
            // A left drag selects text even in mouse-aware apps (vim, fullscreen
            // Claude); a click that doesn't move is handed to the app on release.
            if ev.is_press() && ev.is_left() {
                self.drag = Some(Drag::Select { pane: id, origin: (lx, ly), moved: false, click: true });
                return;
            }
            if let Some(bytes) = mouse::encode(&ev, lx, ly, app_mode, screen.mouse_protocol_encoding()) {
                pane.write_raw(&bytes);
            }
            return;
        }
        if let Some(dir) = ev.wheel() {
            if copying {
                self.copy_scroll(dir * 3);
            } else if screen.alternate_screen() {
                // Full-screen app without mouse support: scroll it with arrow keys.
                let key: &[u8] = if dir < 0 { b"\x1b[A" } else { b"\x1b[B" };
                for _ in 0..3 {
                    pane.write_input(key);
                }
            } else if dir < 0 {
                self.enter_copy(false);
                if let Some(c) = self.copy.as_mut() {
                    c.from_mouse = true;
                }
                self.copy_scroll(dir * 3);
            }
            return;
        }
        if ev.is_press() && ev.is_left() {
            if copying {
                self.with_copy(|c, p| {
                    c.anchor = None;
                    c.cursor = c.at(ly, lx, p);
                    c.sync(p);
                });
            }
            self.drag = Some(Drag::Select { pane: id, origin: (lx, ly), moved: false, click: false });
        }
    }

    /// How many quick presses in a row at the same cell (1 = single click).
    pub(super) fn count_click(&mut self, pane: u64, cell: (u16, u16), ev: &MouseEvent) -> u8 {
        if !(ev.is_press() && ev.is_left()) {
            return 0;
        }
        let n = match self.last_click {
            Some((at, p, c, n)) if p == pane && c == cell && at.elapsed() < Duration::from_millis(450) => n + 1,
            _ => 1,
        };
        self.last_click = Some((Instant::now(), pane, cell, n));
        n
    }

    /// Copy the word (2 clicks) or line (3+) under a cell, then flash a toast.
    pub(super) fn click_select(&mut self, pane: u64, lx: u16, ly: u16, clicks: u8) {
        if self.copy.as_ref().is_none_or(|c| c.pane != pane) {
            if self.focus != Some(pane) {
                self.focus = Some(pane);
            }
            self.enter_copy(false);
        }
        self.with_copy(|c, p| {
            if clicks == 2 {
                c.select_word(ly, lx, p);
            } else {
                c.select_line(ly, p);
            }
        });
        self.yank_selection();
    }

    pub(super) fn continue_drag(&mut self, drag: Drag, ev: MouseEvent) {
        match drag {
            Drag::Sidebar { grab, start, mut moved } => {
                if ev.release {
                    self.sidebar_drag = None;
                    if moved {
                        self.drop_on(&grab, ev.y);
                    } else {
                        self.sidebar_click(start.0, start.1);
                    }
                    self.full_redraw = true;
                    return;
                }
                if ev.is_motion() {
                    moved |= ev.y != start.1;
                    let line = match (&grab, moved) {
                        (_, false) => None,
                        (Grab::Slot(slot), true) => self.slot_drop(slot, ev.y).map(|(_, line)| line),
                        (_, true) => self.drop_gap(&grab, ev.y).map(|(_, line)| line),
                    };
                    self.sidebar_drag = line.map(|line| (grab.clone(), line));
                }
                self.drag = Some(Drag::Sidebar { grab, start, moved });
            }
            Drag::SidebarEdge => {
                if ev.release {
                    return self.save();
                }
                if ev.is_motion() {
                    self.set_sidebar_width(ev.x);
                }
                self.drag = Some(Drag::SidebarEdge);
            }
            Drag::Divider { loc, path, span, dir } => {
                if ev.release {
                    return;
                }
                if ev.is_motion() {
                    let (pos, start, len) = match dir {
                        Dir::Right => (ev.x, span.x, span.width),
                        Dir::Down => (ev.y, span.y, span.height),
                    };
                    let ratio = (pos.saturating_sub(start)) as f32 / len.saturating_sub(1).max(1) as f32;
                    if let Some(view) = self.views.get_mut(&loc) {
                        if view.layout.set_ratio(&path, ratio) {
                            self.relayout();
                        }
                    }
                }
                self.drag = Some(Drag::Divider { loc, path, span, dir });
            }
            Drag::Select { pane, origin, mut moved, click } => {
                if ev.release {
                    self.select_edge = None;
                    if moved {
                        self.yank_selection();
                    } else if click {
                        // Never dragged: it was a click meant for the app.
                        self.replay_click(pane, origin, &ev);
                    }
                    return;
                }
                if ev.is_motion() {
                    let Some(inner) = self.pane_inner(pane) else { return };
                    // Dragging past the top edge, or onto the bottom row (a pane at
                    // the foot of the screen has nothing below it), scrolls; held
                    // there, it keeps scrolling (`autoscroll_selection`).
                    let edge = if ev.y < inner.y { -1 } else if ev.y + 1 >= inner.bottom() { 1 } else { 0 };
                    let lx = ev.x.clamp(inner.x, inner.right().saturating_sub(1)) - inner.x;
                    let ly = ev.y.clamp(inner.y, inner.bottom().saturating_sub(1)) - inner.y;
                    if !moved {
                        if self.copy.as_ref().is_none_or(|c| c.pane != pane) {
                            self.enter_copy(false);
                        }
                        self.with_copy(|c, p| c.anchor = Some(c.at(origin.1, origin.0, p)));
                        moved = true;
                    }
                    self.with_copy(|c, p| {
                        if edge != 0 {
                            c.scroll(edge, p);
                        }
                        c.cursor = c.at(ly, lx, p);
                        c.sync(p);
                    });
                    self.select_edge = (edge != 0).then(|| (edge, lx, ly, Instant::now()));
                }
                self.drag = Some(Drag::Select { pane, origin, moved, click });
            }
        }
    }

    /// A selection held at a pane's top or bottom edge keeps scrolling (a
    /// line every 60ms), as it would in a terminal.
    pub(super) fn autoscroll_selection(&mut self) {
        let Some((edge, lx, ly, at)) = self.select_edge else { return };
        if !matches!(self.drag, Some(Drag::Select { moved: true, .. })) {
            self.select_edge = None;
            return;
        }
        if at.elapsed() < Duration::from_millis(60) {
            return;
        }
        self.with_copy(|c, p| {
            c.scroll(edge, p);
            c.cursor = c.at(ly, lx, p);
            c.sync(p);
        });
        self.select_edge = Some((edge, lx, ly, Instant::now()));
        self.dirty = true;
    }

    pub(super) fn pane_inner(&self, id: u64) -> Option<Rect> {
        let loc = self.pane(id)?.loc.clone();
        let titled = self.titled(&loc);
        let (rects, _) = self.view_rects(&loc);
        rects.into_iter().find(|(p, _)| *p == id).map(|(_, r)| ui::content_rect(r, titled))
    }

    /// What a press at sidebar line `y` would drag, if anything.
    pub(super) fn grab_at(&self, y: u16) -> Option<Grab> {
        if self.sidebar_view != ui::SidebarView::Spaces {
            return None;
        }
        match ui::sidebar_rows(self, self.size.1).into_iter().find(|(ry, _)| *ry == y)?.1 {
            ui::Row::Project(p) => Some(Grab::Repo(p)),
            ui::Row::Location(loc) if self.projects[loc.project].is_unclassified() => loc.slot.map(Grab::Folder),
            ui::Row::Location(loc) | ui::Row::Detail(loc) if loc.slot.is_some() => Some(Grab::Slot(loc)),
            _ => None,
        }
    }

    /// The draggable items of `grab`'s kind (repos or folders) in sidebar order,
    /// each with the screen rows it covers.
    pub(super) fn drag_items(&self, grab: &Grab) -> Vec<(Grab, u16, u16)> {
        let mut items: Vec<(Grab, u16, u16)> = Vec::new();
        for (y, row) in ui::sidebar_rows(self, self.size.1) {
            match (grab, row) {
                (Grab::Repo(_), ui::Row::Project(p)) => items.push((Grab::Repo(p), y, y)),
                (Grab::Repo(_), ui::Row::Location(loc) | ui::Row::Detail(loc)) if !self.projects[loc.project].is_unclassified() => {
                    if let Some(last) = items.last_mut().filter(|(g, ..)| *g == Grab::Repo(loc.project)) {
                        last.2 = y;
                    }
                }
                (Grab::Folder(_), ui::Row::Location(loc)) if self.projects[loc.project].is_unclassified() => {
                    if let Some(id) = loc.slot {
                        items.push((Grab::Folder(id), y, y));
                    }
                }
                _ => {}
            }
        }
        items
    }

    /// Where `grab` lands if dropped with the pointer on line `y`: the gap index
    /// (0 = before the first item) and the row whose bottom edge marks it. The
    /// top half of an item means "before it", the bottom half "after it"; a
    /// one-row item (a folder) splits by drag direction instead. `None` when
    /// it wouldn't move.
    pub(super) fn drop_gap(&self, grab: &Grab, y: u16) -> Option<(usize, u16)> {
        let items = self.drag_items(grab);
        let from = items.iter().position(|(g, ..)| g == grab)?;
        let (first, last) = (items.first()?.1, items.last()?.2);
        let k = items.iter().position(|(_, top, bottom)| y >= *top && y <= *bottom);
        let gap = match k {
            Some(k) => {
                let (_, top, bottom) = items[k];
                let before = if top == bottom { k < from } else { y < top + (bottom - top).div_ceil(2) };
                if before { k } else { k + 1 }
            }
            None if y < first => 0,
            None if y > last => items.len(),
            None => return None,
        };
        if gap == from || gap == from + 1 {
            return None;
        }
        // The line sits on the bottom edge of the row just above the gap.
        let line = if gap == 0 { first.saturating_sub(1) } else { items[gap - 1].2 };
        Some((gap, line))
    }

    /// Where a dragged slot lands with the pointer on line `y`: into the
    /// workstream there (its heading row or one of its slots), or, over the
    /// repos, out of its own workstream; and the line to mark.
    pub(super) fn slot_drop(&self, slot: &Loc, y: u16) -> Option<(Option<Loc>, u16)> {
        let (_, row) = ui::sidebar_rows(self, self.size.1).into_iter().find(|(ry, _)| *ry == y)?;
        let current = self.workstream_of(slot);
        let target = match row {
            ui::Row::Location(l) if self.is_workstream(&l) => Some(l),
            ui::Row::Location(l) | ui::Row::Detail(l) if self.workstream_of(&l).is_some() => self.workstream_of(&l),
            ui::Row::Project(_) | ui::Row::Parked(..) | ui::Row::Heading("REPOS") => None,
            ui::Row::Location(l) | ui::Row::Detail(l) if !self.projects[l.project].is_unclassified() => None,
            _ => return None,
        };
        (target != current).then_some((target, y))
    }

    /// Move `grab` into the gap under the pointer.
    pub(super) fn drop_on(&mut self, grab: &Grab, y: u16) {
        if let Grab::Slot(slot) = grab {
            match self.slot_drop(slot, y) {
                Some((Some(ws), _)) => {
                    self.add_to_workstream(&ws, slot);
                    self.toast(&format!("added to {}", ws.slot.unwrap_or_default()));
                }
                Some((None, _)) => self.remove_from_workstream(slot),
                None => {}
            }
            return;
        }
        let Some((gap, _)) = self.drop_gap(grab, y) else { return };
        let ids: Vec<Grab> = self.drag_items(grab).into_iter().map(|(g, ..)| g).collect();
        let Some(from) = ids.iter().position(|g| g == grab) else { return };
        let mut moved = ids.clone();
        let item = moved.remove(from);
        moved.insert(if gap > from { gap - 1 } else { gap }, item);
        match grab {
            Grab::Repo(_) => {
                // Visible repos in their new order, then any not on screen (scrolled off).
                let mut order: Vec<usize> = moved.iter().filter_map(|g| if let Grab::Repo(p) = g { Some(*p) } else { None }).collect();
                let rest: Vec<usize> = self.ordered_projects().into_iter().filter(|p| !order.contains(p)).collect();
                order.extend(rest);
                self.project_order = order;
            }
            Grab::Folder(_) => {
                if let Some(p) = self.projects.iter().position(Project::is_unclassified) {
                    let rank = |id: &String| moved.iter().position(|g| *g == Grab::Folder(id.clone())).unwrap_or(usize::MAX);
                    self.projects[p].folders.sort_by_key(|f| rank(&f.id));
                }
            }
            Grab::Slot(_) => unreachable!("handled above"),
        }
        self.save();
        self.dirty = true;
    }

    pub(super) fn sidebar_click(&mut self, x: u16, y: u16) {
        // `⏏ release` on a done workstream releases it (asking first).
        if let Some(ws) = ui::release_at(self, self.size.0, self.size.1, x, y) {
            self.go_to(ws);
            return self.run(Action::ReleaseWorkstream);
        }
        // `⏏ free` on a merged slot hands it back (asking first).
        if let Some(loc) = ui::free_at(self, self.size.0, self.size.1, x, y) {
            self.go_to(loc);
            return self.run(Action::RemovePlace);
        }
        // A PR number opens the PR.
        if let Some((.., url)) = ui::sidebar_links(self, self.size.0, self.size.1).into_iter().find(|&(ly, lx, w, _)| ly == y && (lx..lx + w).contains(&x)) {
            return self.open_url(&url);
        }
        let Some((_, row)) = ui::sidebar_rows(self, self.size.1).into_iter().find(|(ry, _)| *ry == y) else {
            return;
        };
        let side = ui::sidebar_width(self.size.0, self.sidebar_hidden, self.sidebar_cols);
        match row {
            ui::Row::TopBar => {
                return self.run(if x < side / 2 { Action::NewSpace } else { Action::Menu });
            }
            // A click on parked work offers resume / discard.
            ui::Row::Parked(..) => return self.open_context_menu(x, y),
            ui::Row::ViewTabs => {
                self.sidebar_view = if x <= ui::VIEW_SPLIT_X { ui::SidebarView::Spaces } else { ui::SidebarView::Agents };
                self.full_redraw = true;
                return;
            }
            ui::Row::Agent { pane, .. } => {
                if self.copy.is_some() {
                    self.exit_copy();
                }
                self.focus = Some(self.panes[pane].id);
                self.current = Some(self.panes[pane].loc.clone());
                self.full_redraw = true;
                return;
            }
            _ => {}
        }
        if self.copy.is_some() {
            self.exit_copy();
        }
        let loc = match row {
            ui::Row::Pane(i) => {
                self.focus = Some(self.panes[i].id);
                self.current = Some(self.panes[i].loc.clone());
                self.full_redraw = true;
                return;
            }
            // The `+` at the right of the scratch and workstreams headings.
            ui::Row::SessionsDivider => {
                if x + 4 >= side {
                    self.new_session();
                }
                return;
            }
            ui::Row::WorkstreamsHeading => {
                if x + 4 >= side {
                    self.mode = Mode::Prompt { label: "name for the workstream".into(), input: String::new(), purpose: PromptFor::NewWorkstream(None) };
                }
                return;
            }
            ui::Row::Heading(_) | ui::Row::Divider | ui::Row::Blank => return,
            ui::Row::TopBar | ui::Row::ViewTabs | ui::Row::Agent { .. } | ui::Row::Parked(..) => unreachable!("handled above"),
            ui::Row::Location(loc) | ui::Row::Detail(loc) => loc,
            ui::Row::Project(p) => {
                // Open the project at its first location with panes, else its root.
                let live = self.live_locations().into_iter().find(|l| l.project == p);
                let first = self.projects[p].folders.first().map(|f| f.id.clone());
                live.unwrap_or(Loc { project: p, slot: if self.projects[p].is_unclassified() { first } else { None } })
            }
        };
        self.go_to(loc);
    }
}
