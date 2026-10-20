use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};

use velmux_pool::Status;

use crate::copy::Copy;
use crate::detect::AgentState;
use crate::layout::Dir;
use crate::pane::{Kind, Loc, Pane};
use crate::server::{App, Mode};

pub fn sidebar_width(cols: u16, hidden: bool) -> u16 {
    if hidden {
        return 0;
    }
    32.min(cols / 3).max(12)
}

/// The rectangle every pane is sized to.
pub fn pane_area(cols: u16, rows: u16, hidden: bool) -> Rect {
    let x = if hidden { 0 } else { sidebar_width(cols, false) + 1 };
    Rect::new(x, 1, cols.saturating_sub(x).max(1), rows.saturating_sub(2).max(1))
}

/// A pane's terminal area within its outer rect (minus the title row when split).
pub fn content_rect(outer: Rect, titled: bool) -> Rect {
    if titled && outer.height > 1 {
        Rect::new(outer.x, outer.y + 1, outer.width, outer.height - 1)
    } else {
        outer
    }
}

/// One sidebar line. Panes nest under the base/slot they run in.
/// One row of [`rows`] (navigation order) or of the sidebar.
pub enum Row {
    Project(usize),
    Location(Loc),
    Pane(usize),
    /// Sidebar only: a section heading.
    Heading(&'static str),
    /// Sidebar only: the rule between repos and folders.
    Divider,
    /// Sidebar only: `⊕ new` and `≡ menu`.
    TopBar,
    /// Sidebar only: the `spaces · agents` switch.
    ViewTabs,
    Blank,
    /// Sidebar agents view: an agent's two lines (location, then agent and task).
    Agent { pane: usize, second: bool },
}

/// Which list the sidebar shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarView {
    Spaces,
    Agents,
}

/// The short menu (from the `≡ menu` button or `prefix+m`).
pub const MENU: &[(&str, crate::keys::Action)] = {
    use crate::keys::Action::*;
    &[
        ("new conversation", AgentBase),
        ("claim a slot…", ClaimSlot),
        ("go to…", LocationPicker),
        ("new tab", NewTab),
        ("name this place", RenameLocation),
        ("rename tab", RenameTab),
        ("settings", Settings),
        ("restart server", RestartServer),
        ("detach", Detach),
        ("stop server", Quit),
    ]
};

/// Where the menu popup sits (anchored under the top bar).
pub fn menu_area(app: &App, cols: u16, rows: u16) -> Rect {
    let side = sidebar_width(cols, app.sidebar_hidden);
    let width = 30.min(cols.saturating_sub(2)).max(side.saturating_sub(2).min(30));
    let height = (MENU.len() as u16 + 2).min(rows.saturating_sub(2));
    let x = if app.sidebar_hidden { 1 } else { side.saturating_sub(width).max(1) };
    Rect::new(x, 1, width, height)
}

/// The sidebar's x where the `spaces` / `agents` labels split.
pub const VIEW_SPLIT_X: u16 = 9;

pub fn rows(app: &App) -> Vec<Row> {
    let mut out = Vec::new();
    // Repos in display order, then the unclassified group of plain folders.
    let mut order = app.ordered_projects();
    order.sort_by_key(|&p| app.projects[p].is_unclassified());
    for p in order {
        let project = &app.projects[p];
        out.push(Row::Project(p));
        let locs: Vec<Loc> = if project.is_unclassified() {
            project.folders.iter().map(|f| Loc { project: p, slot: Some(f.id.clone()) }).collect()
        } else {
            let mut locs = vec![Loc { project: p, slot: None }];
            locs.extend(project.slots.iter().map(|s| Loc { project: p, slot: Some(s.id.clone()) }));
            locs
        };
        for loc in locs {
            let panes: Vec<usize> = app
                .panes
                .iter()
                .enumerate()
                .filter(|(_, pane)| pane.loc == loc)
                .map(|(i, _)| i)
                .collect();
            out.push(Row::Location(loc));
            out.extend(panes.into_iter().map(Row::Pane));
        }
    }
    out
}

/// The sidebar, top to bottom: each repo (its name, then its base and claimed
/// slots), then a divider and the folders outside any repo. Nothing collapses,
/// so moving around never changes the layout; free slots are a count on the
/// repo's line rather than rows of their own.
pub fn display_rows(app: &App) -> Vec<Row> {
    let mut out = Vec::new();
    let repos: Vec<usize> = app.ordered_projects().into_iter().filter(|&p| !app.projects[p].is_unclassified()).collect();
    if !repos.is_empty() {
        out.push(Row::Heading("REPOS"));
    }
    for p in repos {
        out.push(Row::Project(p));
        out.push(Row::Location(Loc { project: p, slot: None }));
        for s in &app.projects[p].slots {
            let loc = Loc { project: p, slot: Some(s.id.clone()) };
            if s.status == Status::InUse || app.views.contains_key(&loc) {
                out.push(Row::Location(loc));
            }
        }
    }
    let folders: Vec<Loc> = app
        .projects
        .iter()
        .enumerate()
        .filter(|(_, p)| p.is_unclassified())
        .flat_map(|(i, p)| p.folders.iter().map(move |f| Loc { project: i, slot: Some(f.id.clone()) }))
        .collect();
    if !folders.is_empty() {
        out.push(Row::Divider);
        out.extend(folders.into_iter().map(Row::Location));
    }
    out
}

/// Sidebar rows with their screen line, scrolled just enough to keep the
/// current location visible. Shared by drawing and mouse hit-testing.
pub fn sidebar_rows(app: &App, height: u16) -> Vec<(u16, Row)> {
    // Fixed header: buttons, a rule, the view switch, a gap.
    let header = [Row::TopBar, Row::Divider, Row::ViewTabs, Row::Blank];
    let top = header.len();
    let (body, at) = match app.sidebar_view {
        SidebarView::Spaces => {
            let rows = display_rows(app);
            let current = app.current_loc();
            let at = rows.iter().position(|r| matches!(r, Row::Location(l) if Some(l) == current.as_ref()));
            (rows, at)
        }
        SidebarView::Agents => {
            let rows = agent_rows(app);
            let focus = app.focused().map(|p| p.id);
            let at = rows.iter().position(|r| matches!(r, Row::Agent { pane, second: true } if Some(app.panes[*pane].id) == focus));
            (rows, at)
        }
    };
    let usable = (height.saturating_sub(1) as usize).saturating_sub(top); // minus the status line
    let start = at.map_or(0, |at| (at + 1).saturating_sub(usable));
    header
        .into_iter()
        .chain(body.into_iter().skip(start).take(usable))
        .enumerate()
        .map(|(y, r)| (y as u16, r))
        .collect()
}

/// Every agent, the ones needing you first (blocked, finished unseen, working,
/// idle), then in sidebar order. Two rows each.
pub fn agent_rows(app: &App) -> Vec<Row> {
    let order: Vec<usize> = rows(app)
        .into_iter()
        .filter_map(|r| match r {
            Row::Pane(i) => Some(i),
            _ => None,
        })
        .filter(|&i| app.panes[i].agent.is_some() || app.panes[i].kind == Kind::Agent)
        .collect();
    let rank = |i: usize| {
        let p = &app.panes[i];
        match (p.state, p.unseen_done) {
            (Some(AgentState::Blocked), _) => 0,
            (_, true) => 1,
            (Some(AgentState::Working), _) => 2,
            _ => 3,
        }
    };
    let mut sorted: Vec<(usize, usize)> = order.into_iter().enumerate().collect();
    sorted.sort_by_key(|&(n, i)| (rank(i), n));
    sorted.into_iter().flat_map(|(_, i)| [Row::Agent { pane: i, second: false }, Row::Agent { pane: i, second: true }]).collect()
}

/// A location as a short label: `caliburr › feat/auth`, `kino › base`, `Helm`.
pub fn short_loc(app: &App, loc: &Loc) -> String {
    let project = &app.projects[loc.project];
    let name = app.views.get(loc).and_then(|v| v.name.clone());
    match &loc.slot {
        Some(id) if project.is_unclassified() => name.unwrap_or_else(|| id.clone()),
        None => format!("{} › {}", project.name(), name.unwrap_or_else(|| "base".into())),
        Some(id) => {
            let branch = project.slots.iter().find(|s| &s.id == id).and_then(|s| s.branch.clone());
            format!("{} › {}", project.name(), name.or(branch).unwrap_or_else(|| id.clone()))
        }
    }
}

fn free_slots(app: &App, project: usize) -> usize {
    app.projects[project]
        .slots
        .iter()
        .filter(|s| {
            s.status == Status::Free && !app.views.contains_key(&Loc { project, slot: Some(s.id.clone()) })
        })
        .count()
}

/// Agent-state glyphs for some panes: (glyph, count, style), non-zero only.
fn rollup_of<'a>(panes: impl Iterator<Item = &'a Pane>) -> Vec<(&'static str, usize, Style)> {
    let (mut working, mut blocked, mut done, mut idle) = (0, 0, 0, 0);
    for p in panes {
        if p.unseen_done {
            done += 1;
            continue;
        }
        match p.state {
            Some(AgentState::Working) => working += 1,
            Some(AgentState::Blocked) => blocked += 1,
            Some(AgentState::Idle) => idle += 1,
            None => {}
        }
    }
    [
        ("▲", blocked, Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)),
        ("✓", done, Style::new().fg(Color::Green)),
        ("●", working, Style::new().fg(Color::Yellow)),
        ("○", idle, Style::new().fg(Color::DarkGray)),
    ]
    .into_iter()
    .filter(|(_, n, _)| *n > 0)
    .collect()
}

const ACCENT: Color = Color::Indexed(39);
const SELECTED_BG: Color = Color::Indexed(236);

/// One location row: glyph, label (name, branch or folder), a dim slot number,
/// and its agents' states on the right. The current one gets a highlight bar.
fn draw_location_row(app: &App, loc: &Loc, y: u16, side: u16, selected: bool, buf: &mut Buffer) {
    let base = if selected { Style::new().bg(SELECTED_BG) } else { Style::new() };
    if selected {
        buf.set_style(Rect::new(0, y, side, 1), base);
        buf.set_string(0, y, "▌", base.fg(ACCENT));
    }
    let project = &app.projects[loc.project];
    let name = app.views.get(loc).and_then(|v| v.name.clone());
    let (glyph, glyph_color, label, number, dirty) = match &loc.slot {
        None => (Some("◆"), Color::Magenta, name.unwrap_or_else(|| "base".into()), None, false),
        Some(id) if project.is_unclassified() => (None, Color::Reset, name.unwrap_or_else(|| id.clone()), None, false),
        Some(id) => {
            let slot = project.slots.iter().find(|s| &s.id == id);
            let branch = slot.and_then(|s| s.branch.clone()).unwrap_or_else(|| "detached".into());
            let n = id.trim_start_matches("slot-").to_string();
            (Some("▣"), Color::Cyan, name.unwrap_or(branch), Some(n), slot.is_some_and(|s| s.dirty))
        }
    };
    let text = if selected { base.fg(Color::White).add_modifier(Modifier::BOLD) } else { base.fg(Color::Gray) };
    let dim = base.fg(Color::DarkGray);

    // Right side first, so the label knows how much room it has.
    let badges = rollup_of(app.panes.iter().filter(|p| p.loc == *loc));
    let badge_text: Vec<String> =
        badges.iter().map(|(g, n, _)| if *n > 1 { format!("{g}{n}") } else { g.to_string() }).collect();
    let badge_w = badge_text.iter().map(|b| b.chars().count() as u16).sum::<u16>() + badge_text.len().saturating_sub(1) as u16;
    let right_edge = side.saturating_sub(1);
    if badge_w > 0 && badge_w + 6 < side {
        let mut x = right_edge - badge_w;
        for ((_, _, style), t) in badges.iter().zip(&badge_text) {
            buf.set_string(x, y, t, style.patch(base));
            x += t.chars().count() as u16 + 1;
        }
    }
    let mut x = 3;
    if let Some(g) = glyph {
        buf.set_string(x, y, g, base.fg(glyph_color));
        x += 2;
    }
    let suffix = number.map(|n| format!(" ·{n}")).unwrap_or_default();
    let room = right_edge.saturating_sub(badge_w + 1).saturating_sub(x) as usize;
    let room_for_label = room.saturating_sub(suffix.chars().count() + dirty as usize);
    let shown: String = if label.chars().count() > room_for_label {
        label.chars().take(room_for_label.saturating_sub(1)).chain(std::iter::once('…')).collect()
    } else {
        label
    };
    buf.set_string(x, y, &shown, text);
    x += shown.chars().count() as u16;
    if dirty {
        buf.set_string(x, y, "*", base.fg(Color::Yellow));
        x += 1;
    }
    if !suffix.is_empty() {
        buf.set_stringn(x, y, &suffix, right_edge.saturating_sub(x) as usize, dim);
    }
}

fn state_glyph(pane: &Pane) -> (&'static str, Style) {
    if pane.unseen_done {
        return ("✓", Style::new().fg(Color::Green));
    }
    match pane.state {
        Some(AgentState::Working) => ("●", Style::new().fg(Color::Yellow)),
        Some(AgentState::Blocked) => ("▲", Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)),
        Some(AgentState::Idle) => ("○", Style::new().fg(Color::DarkGray)),
        None => (" ", Style::new()),
    }
}

/// What a pane is: its agent, the program it's running, or `shell`, plus its
/// folder when that isn't the location's root (`shell · packages/web`).
pub fn pane_label(app: &App, pane: &Pane) -> String {
    let program = pane.fg.program.as_deref();
    let name = match (pane.agent, program) {
        (Some(agent), _) => agent.to_string(),
        (None, Some(p)) if !crate::detect::is_shell(p) => p.to_string(),
        (None, Some(_)) => "shell".into(),
        (None, None) => match pane.kind {
            Kind::Agent => "agent".into(),
            Kind::Shell => "shell".into(),
            Kind::Editor => "editor".into(),
        },
    };
    let place = match (pane.fg.cwd.as_deref(), app.loc_cwd(&pane.loc)) {
        (Some(cwd), Some(root)) => crate::detect::short_cwd(cwd, &root),
        _ => None,
    };
    match place {
        Some(p) => format!("{name} · {p}"),
        None => name,
    }
}

/// The window title worth showing: agents put their task there; shells just
/// put `user@host:path`, which the label already covers.
fn useful_title(pane: &Pane) -> Option<&str> {
    let t = pane.title().trim();
    (pane.agent.is_some() && !t.is_empty()).then_some(t)
}

/// Draw the whole UI; returns where the hardware cursor belongs, if visible.
pub fn draw(app: &App, buf: &mut Buffer) -> Option<Position> {
    let area = buf.area;
    let (cols, rows) = (area.width, area.height);
    let side = sidebar_width(cols, app.sidebar_hidden);
    let focused = app.focused();
    let current = app.current_loc();

    // Sidebar.
    let dim = Style::new().fg(Color::DarkGray);
    let sidebar = if app.sidebar_hidden { Vec::new() } else { sidebar_rows(app, rows) };
    for (y, row) in sidebar {
        match row {
            Row::Heading(title) => {
                buf.set_stringn(1, y, title, side as usize, dim.add_modifier(Modifier::BOLD));
            }
            Row::Divider => {
                buf.set_string(1, y, "─".repeat(side.saturating_sub(2) as usize), dim);
            }
            Row::Project(p) => {
                let here = current.as_ref().is_some_and(|c| c.project == p);
                let style = Style::new().add_modifier(Modifier::BOLD).fg(if here { Color::White } else { Color::Gray });
                let free = free_slots(app, p);
                let right = if free > 0 { format!("{free} free") } else { String::new() };
                let rw = right.chars().count() as u16;
                let room = side.saturating_sub(rw + 3) as usize;
                buf.set_stringn(1, y, app.projects[p].name(), room, style);
                if rw > 0 {
                    buf.set_string(side.saturating_sub(rw + 1), y, right, dim);
                }
            }
            Row::Location(loc) => draw_location_row(app, &loc, y, side, current.as_ref() == Some(&loc), buf),
            Row::Pane(_) | Row::Blank => {}
            Row::TopBar => {
                let gray = Style::new().fg(Color::Gray);
                buf.set_string(1, y, "⊕", Style::new().fg(ACCENT));
                buf.set_string(3, y, "new", gray);
                let menu_open = matches!(app.mode, Mode::Menu { .. });
                let label = "menu";
                let x = side.saturating_sub(label.chars().count() as u16 + 3);
                buf.set_string(x, y, "≡", Style::new().fg(if menu_open { ACCENT } else { Color::Magenta }));
                buf.set_string(x + 2, y, label, if menu_open { gray.add_modifier(Modifier::BOLD) } else { gray });
            }
            Row::ViewTabs => {
                let on = Style::new().fg(Color::White).add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
                let off = Style::new().fg(Color::DarkGray);
                let agents_on = app.sidebar_view == SidebarView::Agents;
                buf.set_string(1, y, "spaces", if agents_on { off } else { on });
                buf.set_string(8, y, "·", off);
                buf.set_string(VIEW_SPLIT_X + 1, y, "agents", if agents_on { on } else { off });
                // How many agents want you, even while looking at spaces.
                let waiting = app.panes.iter().filter(|p| p.unseen_done || p.state == Some(AgentState::Blocked)).count();
                if waiting > 0 {
                    buf.set_string(VIEW_SPLIT_X + 8, y, format!("▲{waiting}"), Style::new().fg(Color::Red).add_modifier(Modifier::BOLD));
                }
            }
            Row::Agent { pane, second } => {
                let p = &app.panes[pane];
                let selected = focused.map(|f| f.id) == Some(p.id);
                let base = if selected { Style::new().bg(SELECTED_BG) } else { Style::new() };
                if selected {
                    buf.set_style(Rect::new(0, y, side, 1), base);
                    buf.set_string(0, y, "▌", base.fg(ACCENT));
                }
                if second {
                    let label = p.agent.unwrap_or("agent");
                    let task = useful_title(p).map(|t| format!(" · {t}")).unwrap_or_default();
                    let text = format!("{label}{task}");
                    buf.set_stringn(3, y, text, side.saturating_sub(4) as usize, base.fg(Color::DarkGray));
                } else {
                    let (glyph, gstyle) = state_glyph(p);
                    let glyph = if glyph == " " { "○" } else { glyph };
                    buf.set_string(1, y, glyph, gstyle.patch(base));
                    let text = if selected { base.fg(Color::White).add_modifier(Modifier::BOLD) } else { base.fg(Color::Gray).add_modifier(Modifier::BOLD) };
                    buf.set_stringn(3, y, short_loc(app, &p.loc), side.saturating_sub(4) as usize, text);
                }
            }
        }
    }
    // Reordering: an accent line in the gap where the dragged repo / folder
    // will land (an underline on the row above it reads as a line between rows).
    if let (Some((_, y)), false) = (&app.sidebar_drag, app.sidebar_hidden) {
        let line = Style::new().add_modifier(Modifier::UNDERLINED).underline_color(ACCENT);
        buf.set_style(Rect::new(1, *y, side.saturating_sub(2), 1), line);
    }
    if !app.sidebar_hidden {
        for y in 0..rows.saturating_sub(1) {
            buf.set_string(side, y, "│", dim);
        }
    }

    // Header + pane body.
    let body = pane_area(cols, rows, app.sidebar_hidden);
    let header = match (&focused, &current) {
        (Some(pane), _) => {
            let title = useful_title(pane).map(|t| format!(" — {t}")).unwrap_or_default();
            format!("{} · {}{title}", app.loc_name(&pane.loc), pane_label(app, pane))
        }
        (None, Some(loc)) => app.loc_name(loc),
        (None, None) => "velmux".into(),
    };
    // Tab bar, right-aligned in the header, when there's more than one; the
    // header text is clipped to leave it room.
    let tabbed = current.as_ref().and_then(|l| app.views.get(l)).filter(|v| v.tabs.len() > 1 || v.tabs.iter().any(|t| t.name.is_some()));
    let tab_label = |i: usize, t: &crate::server::Tab| match &t.name {
        Some(name) => format!(" {}:{name} ", i + 1),
        None => format!(" {} ", i + 1),
    };
    let bar_width: u16 = tabbed.map_or(0, |v| {
        v.tabs.iter().enumerate().map(|(i, t)| tab_label(i, t).chars().count() as u16).sum::<u16>() + 1
    });
    let header_width = body.width.saturating_sub(bar_width) as usize;
    buf.set_stringn(body.x, 0, header, header_width, Style::new().add_modifier(Modifier::BOLD));
    if let Some(view) = tabbed {
        let mut x = body.right();
        for i in (0..view.tabs.len()).rev() {
            let label = tab_label(i, &view.tabs[i]);
            let w = label.chars().count() as u16;
            if x < body.x + w {
                break;
            }
            x -= w;
            let style = if i == view.active {
                Style::new().bg(Color::Indexed(24)).fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::DarkGray)
            };
            buf.set_string(x, 0, label, style);
        }
    }

    let mut cursor = None;
    match current.as_ref().filter(|loc| app.views.contains_key(loc)) {
        Some(loc) => {
            let titled = app.titled(loc);
            let (rects, dividers) = app.view_rects(loc);
            for d in dividers {
                let glyph = match d.dir {
                    Dir::Right => "│",
                    Dir::Down => "─",
                };
                for y in d.rect.y..d.rect.bottom() {
                    for x in d.rect.x..d.rect.right() {
                        buf.set_string(x, y, glyph, dim);
                    }
                }
            }
            for (id, outer) in rects {
                let Some(pane) = app.pane(id) else { continue };
                let is_focused = focused.map(|f| f.id) == Some(id);
                if titled {
                    draw_title(app, pane, outer, is_focused, buf);
                }
                let inner = content_rect(outer, titled);
                draw_screen(pane.parser.screen(), inner, buf);
                if let Some(c) = app.copy.as_ref().filter(|c| c.pane == id) {
                    cursor = draw_copy(c, pane, inner, buf);
                    continue;
                }
                let screen = pane.parser.screen();
                if is_focused && !screen.hide_cursor() && matches!(app.mode, Mode::Normal | Mode::Prefix) {
                    let (r, c) = screen.cursor_position();
                    if r < inner.height && c < inner.width {
                        cursor = Some(Position::new(inner.x + c, inner.y + r));
                    }
                }
            }
        }
        None => {
            let p = &app.prefix.label;
            let lines = [
                "No pane focused.".to_string(),
                String::new(),
                format!("{p} a   Claude in the base checkout"),
                format!("{p} n   claim a slot and start Claude there"),
                format!("{p} c   shell here    {p} e   editor here"),
                format!("{p} ?   all keys"),
            ];
            for (i, l) in lines.iter().enumerate() {
                buf.set_stringn(body.x + 2, body.y + 1 + i as u16, l, body.width as usize, dim);
            }
        }
    }

    // Status line.
    let sy = rows - 1;
    let bar = Style::new().bg(Color::Indexed(236)).fg(Color::White);
    buf.set_style(Rect::new(0, sy, cols, 1), bar);
    let text = match &app.mode {
        Mode::Prefix => app.keymap.help(&app.prefix.short),
        Mode::Prompt { label, input, .. } => format!("{label}: {input}"),
        Mode::Picker(_) => "type to filter · ↑↓ / ^p ^n select · enter go · esc cancel".into(),
        Mode::Menu { .. } => "↑↓ / j k select · enter run · esc close".into(),
        Mode::Confirm { label, .. } => format!("{label} [y/n]"),
        Mode::Copy => match app.copy.as_ref() {
            Some(c) => match (&c.search, &c.message) {
                (Some(q), _) => format!("search {} {q}", if c.search_up { "↑" } else { "↓" }),
                (None, Some(m)) => m.clone(),
                (None, None) => "COPY · hjkl/arrows · ^u ^d ^b ^f · g G · v/V select · y copy · / ? search · n N · q exit".into(),
            },
            None => String::new(),
        },
        Mode::Normal => app
            .status_text()
            .unwrap_or_else(|| format!("{} ? keys", app.prefix.label)),
    };
    let style = if matches!(app.mode, Mode::Normal) { bar } else { bar.fg(Color::Yellow) };
    buf.set_stringn(1, sy, &text, cols.saturating_sub(2) as usize, style);
    if let Some(q) = app.copy.as_ref().and_then(|c| c.search.as_ref()) {
        let x = 1 + ("search ↑ ".chars().count() + q.chars().count()) as u16;
        cursor = Some(Position::new(x.min(cols - 1), sy));
    }
    if let Mode::Menu { selected } = &app.mode {
        draw_menu(app, *selected, buf);
    }
    if let Mode::Picker(p) = &app.mode {
        cursor = draw_picker(app, p, body, buf);
    }
    if let Mode::Prompt { label, input, .. } = &app.mode {
        let x = 1 + (label.chars().count() + 2 + input.chars().count()) as u16;
        cursor = Some(Position::new(x.min(cols - 1), sy));
    }
    if !app.jobs.is_empty() {
        let jobs = format!(" {} ", app.jobs.join(" · "));
        let w = jobs.chars().count() as u16;
        if w < cols {
            buf.set_string(cols - w, sy, jobs, bar.fg(Color::Cyan));
        }
    }
    cursor
}

/// The short menu: label left, its shortcut right.
fn draw_menu(app: &App, selected: usize, buf: &mut Buffer) {
    use ratatui::widgets::{Block, BorderType, Borders, Clear, Widget};
    let area = menu_area(app, buf.area.width, buf.area.height);
    Clear.render(area, buf);
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Magenta))
        .title(" menu ")
        .render(area, buf);
    let inner = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), area.height.saturating_sub(2));
    for (i, (label, action)) in MENU.iter().enumerate().take(inner.height as usize) {
        let y = inner.y + i as u16;
        let on = i == selected;
        let base = if on { Style::new().bg(SELECTED_BG) } else { Style::new() };
        buf.set_style(Rect::new(inner.x, y, inner.width, 1), base);
        if on {
            buf.set_string(inner.x, y, "▌", base.fg(ACCENT));
        }
        let text = if on { base.fg(Color::White).add_modifier(Modifier::BOLD) } else { base.fg(Color::Gray) };
        buf.set_stringn(inner.x + 2, y, label, inner.width.saturating_sub(3) as usize, text);
        if let Some(hint) = app.keymap.hint(*action, &app.prefix.short) {
            let w = hint.chars().count() as u16;
            if w + label.chars().count() as u16 + 4 < inner.width {
                buf.set_string(inner.right() - w - 1, y, hint, base.fg(Color::DarkGray));
            }
        }
    }
}

/// The location picker, centred over the pane area. Returns the cursor (query end).
fn draw_picker(app: &App, p: &crate::picker::Picker, body: Rect, buf: &mut Buffer) -> Option<Position> {
    use ratatui::widgets::{Block, Borders, Clear, Widget};
    let items = app.picker_items();
    let hits = crate::picker::filter(&items, &p.query);
    let width = 64.min(body.width.saturating_sub(2)).max(20);
    let list_rows = (hits.len() as u16).clamp(1, body.height.saturating_sub(5).max(1));
    let height = (list_rows + 3).min(body.height);
    let area = Rect::new(body.x + (body.width.saturating_sub(width)) / 2, body.y + 1, width, height);
    Clear.render(area, buf);
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Indexed(24)))
        .title(" go to ")
        .render(area, buf);
    let inner = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), area.height.saturating_sub(2));
    let prompt = format!("› {}", p.query);
    buf.set_stringn(inner.x, inner.y, &prompt, inner.width as usize, Style::new().add_modifier(Modifier::BOLD));
    let current = app.current_loc();
    // Keep the selection in view.
    let visible = inner.height.saturating_sub(1) as usize;
    let first = p.selected.saturating_sub(visible.saturating_sub(1));
    if hits.is_empty() {
        buf.set_stringn(inner.x, inner.y + 1, "no matches", inner.width as usize, Style::new().fg(Color::DarkGray));
    }
    for (row, item) in hits.iter().enumerate().skip(first).take(visible) {
        let y = inner.y + 1 + (row - first) as u16;
        let selected = row == p.selected;
        let base = if selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
        buf.set_style(Rect::new(inner.x, y, inner.width, 1), base);
        let marker = if current.as_ref() == Some(&item.loc) { "›" } else { " " };
        buf.set_stringn(inner.x, y, format!("{marker} {}", item.label), inner.width as usize, base);
        // Agent summary for that location, right-aligned.
        let glyphs: String = app
            .panes
            .iter()
            .filter(|pane| pane.loc == item.loc)
            .map(|pane| state_glyph(pane).0)
            .filter(|g| *g != " ")
            .collect();
        let w = glyphs.chars().count() as u16;
        if w > 0 && w + 2 < inner.width {
            buf.set_string(inner.right() - w - 1, y, glyphs, base.fg(Color::Yellow));
        }
    }
    Some(Position::new((inner.x + prompt.chars().count() as u16).min(inner.right().saturating_sub(1)), inner.y))
}

/// Selection highlight, scroll position and the copy cursor over a frozen pane.
fn draw_copy(c: &Copy, pane: &Pane, area: Rect, buf: &mut Buffer) -> Option<Position> {
    let top = c.top(&pane.parser);
    let sel = Style::new().bg(Color::Indexed(25)).fg(Color::White);
    if let Some((a, b)) = c.selection() {
        for r in 0..area.height {
            let abs = top + r as usize;
            if abs < a.0 || abs > b.0 {
                continue;
            }
            let from = if abs == a.0 { a.1 } else { 0 };
            let to = if abs == b.0 { b.1.min(area.width.saturating_sub(1)) } else { area.width.saturating_sub(1) };
            for col in from..=to {
                if let Some(cell) = buf.cell_mut((area.x + col, area.y + r)) {
                    cell.set_style(sel);
                }
            }
        }
    }
    let sb = pane.parser.screen().scrollback_len();
    let tag = format!("[{}/{}]", c.offset, sb);
    let w = tag.chars().count() as u16;
    if w < area.width {
        buf.set_string(area.right() - w, area.y, tag, Style::new().bg(Color::Yellow).fg(Color::Black));
    }
    let row = c.cursor.0.checked_sub(top).filter(|r| *r < area.height as usize)?;
    (c.search.is_none()).then(|| Position::new(area.x + c.cursor.1.min(area.width - 1), area.y + row as u16))
}

fn draw_title(app: &App, pane: &Pane, outer: Rect, focused: bool, buf: &mut Buffer) {
    let row = Rect::new(outer.x, outer.y, outer.width, 1);
    let style = if focused {
        Style::new().bg(Color::Indexed(24)).fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::new().bg(Color::Indexed(236)).fg(Color::Gray)
    };
    buf.set_style(row, style);
    let (glyph, gstyle) = state_glyph(pane);
    buf.set_stringn(row.x + 1, row.y, glyph, 1, gstyle.patch(Style::new().bg(style.bg.unwrap_or(Color::Reset))));
    let text = match useful_title(pane) {
        Some(title) => format!("{} — {title}", pane_label(app, pane)),
        None => pane_label(app, pane),
    };
    buf.set_stringn(row.x + 3, row.y, text, row.width.saturating_sub(4) as usize, style);
}

fn color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn draw_screen(screen: &vt100::Screen, area: Rect, buf: &mut Buffer) {
    for row in 0..area.height {
        for col in 0..area.width {
            let Some(cell) = screen.cell(row, col) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut style = Style::new().fg(color(cell.fgcolor())).bg(color(cell.bgcolor()));
            if cell.bold() {
                style = style.add_modifier(Modifier::BOLD);
            }
            if cell.dim() {
                style = style.add_modifier(Modifier::DIM);
            }
            if cell.italic() {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if cell.underline() {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if cell.inverse() {
                style = style.add_modifier(Modifier::REVERSED);
            }
            let symbol = if cell.has_contents() { cell.contents() } else { " " };
            if let Some(out) = buf.cell_mut((area.x + col, area.y + row)) {
                out.set_symbol(symbol).set_style(style);
            }
        }
    }
}
