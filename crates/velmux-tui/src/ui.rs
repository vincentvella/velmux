use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};

use velmux_pool::Status;

use crate::copy::Copy;
use crate::detect::AgentState;
use crate::theme::Theme;
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
    Rect::new(x, 1, cols.saturating_sub(x).max(1), rows.saturating_sub(1).max(1))
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
    /// The rule above the scratch sessions, with a `+` for a new one.
    SessionsDivider,
    /// A parked branch: (project, index into its `parked`).
    Parked(usize, usize),
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
        // Things for one place (create, rename, remove) live in the sidebar's
        // right-click menus.
        ("new space…", NewSpace),
        ("new session", NewSession),
        ("go to…", LocationPicker),
        ("find conversation…", FindSession),
        ("rename tab", RenameTab),
        ("theme…", ThemePicker),
        ("check setup (doctor)", Doctor),
        ("sounds…", SoundPicker),
        ("settings", Settings),
        ("restart server", RestartServer),
        ("detach", Detach),
        ("stop server", Quit),
    ]
};

/// The open menu's rows: a right-click menu's, else the global one.
pub fn menu_items(app: &App) -> &[(&'static str, crate::keys::Action)] {
    match &app.mode {
        Mode::Menu { context: Some(c), .. } => &c.items,
        _ => MENU,
    }
}

/// Where the menu popup sits: under the top bar, or at a right-click (below
/// the clicked row, kept on screen).
pub fn menu_area(app: &App, cols: u16, rows: u16) -> Rect {
    if let Mode::Menu { context: Some(c), .. } = &app.mode {
        // Wide enough for the longest item (and the title), at least 26.
        let longest = c.items.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(0).max(c.title.chars().count().saturating_sub(2));
        let width = (longest as u16 + 5).max(26).min(cols.saturating_sub(2));
        let height = (c.items.len() as u16 + 2).min(rows.saturating_sub(1));
        let x = c.at.0.min(cols.saturating_sub(width));
        let below = c.at.1 + 1;
        let y = if below + height <= rows { below } else { c.at.1.saturating_sub(height) };
        return Rect::new(x, y, width, height);
    }
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
    // (ordered_projects already leaves out removed repos.)
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
        out.extend((0..app.projects[p].parked.len()).map(|i| Row::Parked(p, i)));
    }
    let folders: Vec<Loc> = app
        .projects
        .iter()
        .enumerate()
        .filter(|(_, p)| p.is_unclassified())
        .flat_map(|(i, p)| p.folders.iter().map(move |f| Loc { project: i, slot: Some(f.id.clone()) }))
        .collect();
    // Always there: its `+` starts a scratch session.
    out.push(Row::SessionsDivider);
    out.extend(folders.into_iter().map(Row::Location));
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
fn rollup_of<'a>(t: &Theme, panes: impl Iterator<Item = &'a Pane>) -> Vec<(&'static str, usize, Style)> {
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
        ("▲", blocked, Style::new().fg(t.blocked).add_modifier(Modifier::BOLD)),
        ("✓", done, Style::new().fg(t.done)),
        ("●", working, Style::new().fg(t.working)),
        ("○", idle, Style::new().fg(t.idle)),
    ]
    .into_iter()
    .filter(|(_, n, _)| *n > 0)
    .collect()
}


/// One location row: glyph, label (name, branch or folder), a dim slot number,
/// and its agents' states on the right. The current one gets a highlight bar.
/// `◌ feat/login   2 files · 3h`: work waiting on a branch, dimmed.
fn draw_parked_row(app: &App, project: usize, parked: &velmux_pool::ParkedView, y: u16, side: u16, buf: &mut Buffer) {
    let t = &app.theme;
    let dim = Style::new().fg(t.dim);
    let now = velmux_pool::state::now();
    let right_edge = side.saturating_sub(1);
    let merged = app.projects[project].prs.get(&parked.branch).is_some_and(|pr| pr.state == velmux_pool::gh::PrState::Merged);
    let full = if merged { format!("merged · {}", parked.age(now)) } else { format!("{} · {}", parked.what(), parked.age(now)) };
    let x = 3;
    buf.set_string(x, y, "◌", dim);
    let label_x = x + 2;
    // Room permitting, what's waiting and how long; else just the age.
    let room = right_edge.saturating_sub(label_x) as usize;
    let branch_w = parked.branch.chars().count();
    let right = if branch_w + 2 + full.chars().count() <= room { full } else { parked.age(now) };
    let right_w = right.chars().count();
    let room_for_label = room.saturating_sub(right_w + 1);
    let shown: String = if branch_w > room_for_label {
        parked.branch.chars().take(room_for_label.saturating_sub(1)).chain(std::iter::once('…')).collect()
    } else {
        parked.branch.clone()
    };
    buf.set_string(label_x, y, &shown, Style::new().fg(t.text2).add_modifier(Modifier::ITALIC));
    buf.set_string(right_edge.saturating_sub(right_w as u16), y, &right, dim);
}

fn draw_location_row(app: &App, loc: &Loc, y: u16, side: u16, selected: bool, buf: &mut Buffer) {
    let t = &app.theme;
    let base = if selected { Style::new().bg(t.selected_bg) } else { Style::new() };
    if selected {
        buf.set_style(Rect::new(0, y, side, 1), base);
        buf.set_string(0, y, "▌", base.fg(t.accent));
    }
    let project = &app.projects[loc.project];
    let name = app.views.get(loc).and_then(|v| v.name.clone());
    let (glyph, glyph_color, label, number, dirty) = match &loc.slot {
        None => (Some("◆"), t.base_glyph, name.unwrap_or_else(|| "base".into()), None, false),
        Some(id) if project.is_unclassified() => (None, Color::Reset, name.unwrap_or_else(|| id.clone()), None, false),
        Some(id) => {
            let slot = project.slots.iter().find(|s| &s.id == id);
            let branch = slot.and_then(|s| s.branch.clone()).unwrap_or_else(|| "detached".into());
            let n = id.trim_start_matches("slot-").to_string();
            (Some("▣"), t.slot_glyph, name.unwrap_or(branch), Some(n), slot.is_some_and(|s| s.dirty))
        }
    };
    let text = if selected { base.fg(t.text).add_modifier(Modifier::BOLD) } else { base.fg(t.text2) };
    let dim = base.fg(t.dim);

    // Right side first, so the label knows how much room it has: the slot's
    // health (still being prepared, a rebase/merge stopped on conflicts,
    // commits behind the base), then its agents' states.
    let mut marks: Vec<(String, Style)> = Vec::new();
    let slot = loc.slot.as_ref().filter(|_| !project.is_unclassified()).and_then(|id| project.slots.iter().find(|s| &s.id == id));
    if app.preparing.contains_key(loc) {
        marks.push(("⋯".into(), Style::new().fg(t.dim)));
    }
    // Changes in a repo's checkout: work that belongs in a slot.
    if loc.slot.is_none() && project.base_changes > 0 {
        marks.push((format!("✎{}", project.base_changes), Style::new().fg(t.alert).add_modifier(Modifier::BOLD)));
    }
    if let Some(s) = slot {
        if s.conflict.is_some() {
            marks.push(("!".into(), Style::new().fg(t.blocked).add_modifier(Modifier::BOLD)));
        }
        if s.behind > 0 {
            marks.push((format!("↓{}", s.behind), Style::new().fg(t.alert)));
        }
        // Its PR: number and checks (`#142✓`), or that it merged.
        if let Some(pr) = s.branch.as_ref().and_then(|b| project.prs.get(b)) {
            use velmux_pool::gh::{Checks, PrState};
            let (glyph, color) = match (pr.state, pr.checks) {
                (PrState::Merged, _) => ("⇡", t.accent),
                (PrState::Closed, _) => ("×", t.dim),
                (_, Checks::Failing) => ("✗", t.blocked),
                (_, Checks::Pending) => ("…", t.working),
                (_, Checks::Passing) => ("✓", t.done),
                (_, Checks::None) => ("", t.dim),
            };
            let number = if pr.draft || pr.state == PrState::Closed { t.dim } else { t.text2 };
            marks.push((format!("#{}{glyph}", pr.number), Style::new().fg(if glyph.is_empty() { number } else { color })));
        }
    }
    for (g, n, style) in rollup_of(t, app.panes.iter().filter(|p| p.loc == *loc)) {
        marks.push((if n > 1 { format!("{g}{n}") } else { g.to_string() }, style));
    }
    let badge_w = marks.iter().map(|(m, _)| m.chars().count() as u16).sum::<u16>() + marks.len().saturating_sub(1) as u16;
    let right_edge = side.saturating_sub(1);
    if badge_w > 0 && badge_w + 6 < side {
        let mut x = right_edge - badge_w;
        for (m, style) in &marks {
            buf.set_string(x, y, m, style.patch(base));
            x += m.chars().count() as u16 + 1;
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
        buf.set_string(x, y, "*", base.fg(t.dirty));
        x += 1;
    }
    if !suffix.is_empty() {
        buf.set_stringn(x, y, &suffix, right_edge.saturating_sub(x) as usize, dim);
    }
}

fn state_glyph(t: &Theme, pane: &Pane) -> (&'static str, Style) {
    if pane.unseen_done {
        return ("✓", Style::new().fg(t.done));
    }
    match pane.state {
        Some(AgentState::Working) => ("●", Style::new().fg(t.working)),
        Some(AgentState::Blocked) => ("▲", Style::new().fg(t.blocked).add_modifier(Modifier::BOLD)),
        Some(AgentState::Idle) => ("○", Style::new().fg(t.idle)),
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
pub fn useful_title(pane: &Pane) -> Option<&str> {
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
    let t = &app.theme;
    let dim = Style::new().fg(t.dim);
    let sidebar = if app.sidebar_hidden { Vec::new() } else { sidebar_rows(app, rows) };
    for (y, row) in sidebar {
        match row {
            Row::Heading(title) => {
                buf.set_stringn(1, y, title, side as usize, dim.add_modifier(Modifier::BOLD));
            }
            Row::Divider => {
                buf.set_string(1, y, "─".repeat(side.saturating_sub(2) as usize), dim);
            }
            Row::SessionsDivider => {
                buf.set_string(1, y, "─".repeat(side.saturating_sub(5) as usize), dim);
                buf.set_string(side.saturating_sub(3), y, "+", Style::new().fg(t.accent).add_modifier(Modifier::BOLD));
            }
            Row::Project(p) => {
                let here = current.as_ref().is_some_and(|c| c.project == p);
                let style = Style::new().add_modifier(Modifier::BOLD).fg(if here { t.text } else { t.text2 });
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
            Row::Parked(p, i) => draw_parked_row(app, p, &app.projects[p].parked[i], y, side, buf),
            Row::Pane(_) | Row::Blank => {}
            Row::TopBar => {
                let gray = Style::new().fg(t.text2);
                buf.set_string(1, y, "⊕", Style::new().fg(t.accent));
                buf.set_string(3, y, "new space", gray);
                let menu_open = matches!(app.mode, Mode::Menu { context: None, .. });
                let label = "menu";
                let x = side.saturating_sub(label.chars().count() as u16 + 3);
                buf.set_string(x, y, "≡", Style::new().fg(if menu_open { t.accent } else { t.menu_border }));
                buf.set_string(x + 2, y, label, if menu_open { gray.add_modifier(Modifier::BOLD) } else { gray });
            }
            Row::ViewTabs => {
                let on = Style::new().fg(t.text).add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
                let off = Style::new().fg(t.dim);
                let agents_on = app.sidebar_view == SidebarView::Agents;
                buf.set_string(1, y, "spaces", if agents_on { off } else { on });
                buf.set_string(8, y, "·", off);
                buf.set_string(VIEW_SPLIT_X + 1, y, "agents", if agents_on { on } else { off });
                // How many agents want you, even while looking at spaces.
                let waiting = app.panes.iter().filter(|p| p.unseen_done || p.state == Some(AgentState::Blocked)).count();
                if waiting > 0 {
                    buf.set_string(VIEW_SPLIT_X + 8, y, format!("▲{waiting}"), Style::new().fg(t.blocked).add_modifier(Modifier::BOLD));
                }
            }
            Row::Agent { pane, second } => {
                let p = &app.panes[pane];
                let selected = focused.map(|f| f.id) == Some(p.id);
                let base = if selected { Style::new().bg(t.selected_bg) } else { Style::new() };
                if selected {
                    buf.set_style(Rect::new(0, y, side, 1), base);
                    buf.set_string(0, y, "▌", base.fg(t.accent));
                }
                if second {
                    let label = p.agent.unwrap_or("agent");
                    let task = useful_title(p).map(|t| format!(" · {t}")).unwrap_or_default();
                    let text = format!("{label}{task}");
                    buf.set_stringn(3, y, text, side.saturating_sub(4) as usize, base.fg(t.dim));
                } else {
                    let (glyph, gstyle) = state_glyph(&app.theme, p);
                    let glyph = if glyph == " " { "○" } else { glyph };
                    buf.set_string(1, y, glyph, gstyle.patch(base));
                    let text = if selected { base.fg(t.text).add_modifier(Modifier::BOLD) } else { base.fg(t.text2).add_modifier(Modifier::BOLD) };
                    buf.set_stringn(3, y, short_loc(app, &p.loc), side.saturating_sub(4) as usize, text);
                }
            }
        }
    }
    // Reordering: an accent line in the gap where the dragged repo / folder
    // will land (an underline on the row above it reads as a line between rows).
    if let (Some((_, y)), false) = (&app.sidebar_drag, app.sidebar_hidden) {
        let line = Style::new().add_modifier(Modifier::UNDERLINED).underline_color(t.accent);
        buf.set_style(Rect::new(1, *y, side.saturating_sub(2), 1), line);
    }
    if !app.sidebar_hidden {
        for y in 0..rows {
            buf.set_string(side, y, "│", dim);
        }
    }

    // Header + pane body.
    let body = pane_area(cols, rows, app.sidebar_hidden);
    draw_header(app, body, buf);

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
                    cursor = draw_copy(&app.theme, c, pane, inner, buf);
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
            draw_empty(app, current.as_ref(), body, buf);
        }
    }

    // Status line: the bottom row of the sidebar (or, with it hidden, the pane
    // area's bottom row, only while there's something to say).
    let sy = rows - 1;
    let bar = Style::new().bg(t.bar_bg).fg(t.bar_fg);
    let (text, style) = match &app.mode {
        Mode::Prefix => (format!("{} …", app.prefix.label), bar.fg(t.accent).add_modifier(Modifier::BOLD)),
        Mode::Picker(_) => ("enter go · esc cancel".into(), bar.fg(t.dim)),
        Mode::Sessions { .. } => ("enter open · esc cancel".into(), bar.fg(t.dim)),
        Mode::NewSpace { .. } => ("enter add · esc cancel".into(), bar.fg(t.dim)),
        Mode::Menu { .. } | Mode::ThemePicker { .. } => ("enter pick · esc close".into(), bar.fg(t.dim)),
        Mode::SoundPicker { .. } => ("↑↓ hear · ←→ column · enter keep".into(), bar.fg(t.dim)),
        Mode::Prompt { .. } => ("enter save · esc cancel".into(), bar.fg(t.dim)),
        Mode::Confirm { .. } => ("y yes · n no".into(), bar.fg(t.dim)),
        Mode::Copy => match app.copy.as_ref() {
            Some(c) => match (&c.search, &c.message) {
                (Some(q), _) => (format!("search {} {q}", if c.search_up { "↑" } else { "↓" }), bar.fg(t.alert)),
                (None, Some(m)) => (m.clone(), bar.fg(t.alert)),
                (None, None) => ("COPY · v select · y copy · / search · q exit".into(), bar.fg(t.alert)),
            },
            None => (String::new(), bar),
        },
        Mode::Normal => match app.status_text() {
            Some(s) => (s, bar),
            None if app.sidebar_hidden => (String::new(), bar),
            None => (format!("{} for keys", app.prefix.label), bar.fg(t.dim)),
        },
    };
    let jobs = (!app.jobs.is_empty()).then(|| format!(" {} ", app.jobs.iter().map(|(_, text)| text.as_str()).collect::<Vec<_>>().join(" · ")));
    let strip = if app.sidebar_hidden {
        (!text.is_empty() || jobs.is_some()).then(|| Rect::new(0, sy, cols, 1))
    } else {
        Some(Rect::new(0, sy, side, 1))
    };
    if let Some(strip) = strip {
        buf.set_style(strip, bar);
        // Clear what was drawn underneath (a pane's last row when the sidebar is hidden).
        buf.set_string(strip.x, sy, " ".repeat(strip.width as usize), bar);
        let jobs_w = jobs.as_ref().map_or(0, |j| j.chars().count() as u16);
        let room = strip.width.saturating_sub(2 + jobs_w) as usize;
        buf.set_stringn(strip.x + 1, sy, &text, room, style);
        if let Some(j) = jobs {
            if jobs_w < strip.width {
                buf.set_string(strip.right() - jobs_w, sy, j, bar.fg(t.jobs));
            }
        }
        if let Some(q) = app.copy.as_ref().and_then(|c| c.search.as_ref()) {
            let x = 1 + ("search ↑ ".chars().count() + q.chars().count()) as u16;
            cursor = Some(Position::new(x.min(strip.right().saturating_sub(1)), sy));
        }
    }

    if let Some(text) = app.toast_text() {
        let area = draw_toast(&app.theme, text, body, buf);
        // The pane's cursor would show through the toast as a highlighted cell.
        cursor = cursor.filter(|p| !area.contains(*p));
    }
    if matches!(app.mode, Mode::Prefix) {
        draw_which_key(app, body, buf);
        cursor = None;
    }
    if let Mode::Menu { selected, .. } = &app.mode {
        draw_menu(app, *selected, buf);
    }
    if let Mode::SoundPicker { col, done, blocked, .. } = &app.mode {
        draw_sound_picker(app, *col, *done, *blocked, body, buf);
    }
    if let Mode::ThemePicker { selected, .. } = &app.mode {
        draw_theme_picker(app, *selected, buf);
    }
    if let Mode::NewSpace { query, selected, repos } = &app.mode {
        cursor = draw_new_space(app, query, *selected, repos, body, buf);
    }
    if let Mode::Sessions { query, selected } = &app.mode {
        cursor = draw_sessions(app, query, *selected, body, buf);
    }
    if let Mode::Picker(p) = &app.mode {
        cursor = draw_picker(app, p, body, buf);
    }
    if let Mode::Prompt { label, input, .. } = &app.mode {
        cursor = draw_prompt(t, label, input, body, buf);
    }
    if let Mode::Confirm { label, .. } = &app.mode {
        draw_confirm(t, label, body, buf);
        cursor = None;
    }
    cursor
}

/// A rounded box centred near the top of the pane area, cleared and titled.
fn popup(t: &Theme, title: &str, width: u16, height: u16, body: Rect, buf: &mut Buffer) -> Rect {
    use ratatui::widgets::{Block, BorderType, Borders, Clear, Widget};
    let width = width.min(body.width.saturating_sub(2)).max(10);
    let height = height.min(body.height);
    let area = Rect::new(body.x + body.width.saturating_sub(width) / 2, body.y + body.height.saturating_sub(height) / 4, width, height);
    Clear.render(area, buf);
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.menu_border))
        .title(ratatui::text::Span::styled(format!(" {title} "), Style::new().fg(t.text).add_modifier(Modifier::BOLD)))
        .render(area, buf);
    Rect::new(area.x + 2, area.y + 1, area.width.saturating_sub(4), area.height.saturating_sub(2))
}

/// A text prompt as a popup; returns the cursor (end of the input).
fn draw_prompt(t: &Theme, label: &str, input: &str, body: Rect, buf: &mut Buffer) -> Option<Position> {
    let inner = popup(t, label, 52, 3, body, buf);
    let chars: Vec<char> = input.chars().collect();
    // Keep the end of a long input in view.
    let room = inner.width.saturating_sub(1) as usize;
    let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
    buf.set_string(inner.x, inner.y, &shown, Style::new().fg(t.text));
    Some(Position::new(inner.x + shown.chars().count() as u16, inner.y))
}

/// A yes/no question as a popup.
fn draw_confirm(t: &Theme, label: &str, body: Rect, buf: &mut Buffer) {
    let width = (label.chars().count() as u16 + 6).clamp(30, 64);
    let lines = wrap(label, width.saturating_sub(4) as usize);
    let inner = popup(t, "confirm", width, lines.len() as u16 + 4, body, buf);
    for (i, line) in lines.iter().enumerate() {
        buf.set_stringn(inner.x, inner.y + i as u16, line, inner.width as usize, Style::new().fg(t.text));
    }
    let y = inner.y + lines.len() as u16 + 1;
    let key = Style::new().fg(t.accent).add_modifier(Modifier::BOLD);
    buf.set_string(inner.x, y, "y", key);
    buf.set_string(inner.x + 2, y, "yes", Style::new().fg(t.text2));
    buf.set_string(inner.x + 8, y, "n", key);
    buf.set_string(inner.x + 10, y, "no", Style::new().fg(t.text2));
}

/// Greedy word wrap.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// After the prefix: every key you can press next, in groups side by side
/// (stacked when the pane area is narrow).
fn draw_which_key(app: &App, body: Rect, buf: &mut Buffer) {
    use ratatui::widgets::{Block, BorderType, Borders, Clear, Widget};
    let t = &app.theme;
    let groups = app.keymap.help_groups();
    let key_w = groups.iter().flat_map(|(_, g)| g.iter().map(|(k, _)| k.chars().count())).max().unwrap_or(1) as u16;
    let label_w = groups.iter().flat_map(|(_, g)| g.iter().map(|(_, l)| l.chars().count())).max().unwrap_or(1) as u16;
    let col_w = key_w + 2 + label_w;
    let gap = 4;
    let fit = ((body.width.saturating_sub(4) + gap) / (col_w + gap)).max(1) as usize;
    let per_row = fit.min(groups.len());
    let bands: Vec<&[crate::keys::HelpGroup]> = groups.chunks(per_row).collect();
    let band_h = |b: &[crate::keys::HelpGroup]| 1 + b.iter().map(|(_, g)| g.len()).max().unwrap_or(0) as u16;
    // Borders, each band (title + rows), a blank line between bands.
    let height = (bands.iter().map(|b| band_h(b)).sum::<u16>() + 2 + bands.len().saturating_sub(1) as u16).min(body.height);
    let width = (per_row as u16 * (col_w + gap) - gap + 4).min(body.width);
    // In the lower part of the pane area but well clear of the bottom, where
    // Claude's input box sits.
    let y = body.y + body.height.saturating_sub(height) * 3 / 5;
    let area = Rect::new(body.x + body.width.saturating_sub(width) / 2, y, width, height);
    Clear.render(area, buf);
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.accent))
        .title(ratatui::text::Span::styled(format!(" {} ", app.prefix.label), Style::new().fg(t.accent).add_modifier(Modifier::BOLD)))
        .title_bottom(ratatui::text::Span::styled(" esc cancel ", Style::new().fg(t.dim)))
        .render(area, buf);
    let mut y = area.y + 1;
    for band in bands {
        for (i, (title, items)) in band.iter().enumerate() {
            let x = area.x + 2 + i as u16 * (col_w + gap);
            if y >= area.bottom() - 1 {
                break;
            }
            buf.set_string(x, y, *title, Style::new().fg(t.dim).add_modifier(Modifier::BOLD));
            for (j, (key, label)) in items.iter().enumerate() {
                let ky = y + 1 + j as u16;
                if ky >= area.bottom() - 1 {
                    break;
                }
                let kw = key.chars().count() as u16;
                buf.set_string(x + key_w - kw, ky, key, Style::new().fg(t.accent).add_modifier(Modifier::BOLD));
                buf.set_string(x + key_w + 2, ky, *label, Style::new().fg(t.text2));
            }
        }
        y += band_h(band) + 1;
    }
}

/// What a click in the header row lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabHit {
    Tab(usize),
    New,
}

/// One tab in the header strip.
struct TabCell {
    rect: Rect,
    hit: TabHit,
    number: String,
    label: String,
    glyph: Option<(&'static str, Style)>,
    active: bool,
}

/// Whether the current place shows a tab strip (more than one tab, or a named one).
fn tabbed(app: &App) -> Option<&crate::server::View> {
    let loc = app.current_loc()?;
    app.views.get(&loc).filter(|v| v.tabs.len() > 1 || v.tabs.iter().any(|t| t.name.is_some()))
}

/// The header's clickable cells: each tab (left to right) and the `+` new tab
/// button after them, or on its own at the right when there's a single tab.
fn tab_cells(app: &App, body: Rect) -> Vec<TabCell> {
    let t = &app.theme;
    let y = 0;
    let plus = |x: u16| TabCell {
        rect: Rect::new(x, y, 3, 1),
        hit: TabHit::New,
        number: String::new(),
        label: "+".into(),
        glyph: None,
        active: false,
    };
    let Some(view) = tabbed(app) else {
        return if app.current_loc().is_some() && body.width > 3 { vec![plus(body.right() - 3)] } else { Vec::new() };
    };
    let mut cells = Vec::new();
    let mut x = body.x;
    for (i, tab) in view.tabs.iter().enumerate() {
        let active = i == view.active;
        let leaves = tab.layout.leaves();
        let shown = if active { app.focused().map(|p| p.id).filter(|f| tab.layout.contains(*f)) } else { None }
            .or(tab.last_focus.filter(|f| tab.layout.contains(*f)))
            .or(leaves.first().copied());
        let label = match (&tab.name, shown.and_then(|id| app.pane(id))) {
            (Some(name), _) => name.clone(),
            (None, Some(p)) => pane_label(app, p).split(" · ").next().unwrap_or_default().to_string(),
            (None, None) => "tab".into(),
        };
        let label: String = label.chars().take(18).collect();
        // The most urgent state in the tab, so background tabs still tell you.
        let glyph = rollup_of(t, leaves.iter().filter_map(|id| app.pane(*id))).first().map(|(g, _, st)| (*g, *st));
        let number = format!("{}", i + 1);
        let width = 1 + number.chars().count() + 1 + label.chars().count() + glyph.map_or(0, |_| 2) + 1;
        let width = width as u16;
        if x + width + 3 > body.right() {
            break;
        }
        cells.push(TabCell { rect: Rect::new(x, y, width, 1), hit: TabHit::Tab(i), number, label, glyph, active });
        x += width;
    }
    cells.push(plus(x));
    cells
}

/// What's under a click in the header row.
pub fn tab_hit(app: &App, body: Rect, x: u16) -> Option<TabHit> {
    tab_cells(app, body).into_iter().find(|c| x >= c.rect.x && x < c.rect.right()).map(|c| c.hit)
}

/// The header row: the tab strip (when there's more than one tab) and where
/// you are, the focused pane and its title.
fn draw_header(app: &App, body: Rect, buf: &mut Buffer) {
    let t = &app.theme;
    let header = match (app.focused(), app.current_loc()) {
        (Some(pane), _) => {
            let title = useful_title(pane).map(|t| format!(" — {t}")).unwrap_or_default();
            let port = app.loc_port(&pane.loc).map(|p| format!(" :{p}")).unwrap_or_default();
            format!("{}{port} · {}{title}", app.loc_name(&pane.loc), pane_label(app, pane))
        }
        (None, Some(loc)) => app.loc_name(&loc),
        (None, None) => "velmux".into(),
    };
    let cells = tab_cells(app, body);
    for c in &cells {
        let x = c.rect.x;
        if c.hit == TabHit::New {
            buf.set_string(x + 1, 0, "+", Style::new().fg(t.dim));
            continue;
        }
        let base = if c.active { Style::new().bg(t.focus_bg) } else { Style::new() };
        buf.set_style(c.rect, base);
        let num = if c.active { base.fg(t.focus_fg) } else { base.fg(t.dim) };
        let label = if c.active { base.fg(t.focus_fg).add_modifier(Modifier::BOLD) } else { base.fg(t.text2) };
        buf.set_string(x + 1, 0, &c.number, num);
        let lx = x + 2 + c.number.chars().count() as u16;
        buf.set_string(lx, 0, &c.label, label);
        if let Some((g, st)) = c.glyph {
            buf.set_string(lx + c.label.chars().count() as u16 + 1, 0, g, st.patch(base));
        }
    }
    if tabbed(app).is_some() {
        // Tabs on the left; where you are, dimmed, in the room that's left.
        let from = cells.last().map_or(body.x, |c| c.rect.right()) + 2;
        let room = body.right().saturating_sub(from) as usize;
        let len = header.chars().count();
        let x = if len < room { body.right() - len as u16 } else { from };
        buf.set_stringn(x, 0, &header, room, Style::new().fg(t.dim));
    } else {
        let room = body.width.saturating_sub(4) as usize;
        buf.set_stringn(body.x, 0, &header, room, Style::new().add_modifier(Modifier::BOLD));
    }
}

/// One thing to do from an empty screen: press its key (no prefix needed,
/// there's no pane to take it) or click it.
pub struct EmptyItem {
    pub key: char,
    pub label: String,
    /// Its usual shortcut, shown dimmed.
    pub hint: String,
    pub action: crate::keys::Action,
}

/// What an empty screen says and offers: (title, lines about it, actions,
/// setup steps still to do).
pub fn empty_screen(app: &App, loc: Option<&Loc>) -> (String, Vec<&'static str>, Vec<EmptyItem>, Vec<EmptyItem>) {
    use crate::keys::Action as A;
    let item = |key: char, label: &str, action: A| EmptyItem {
        key,
        label: label.to_string(),
        hint: app.keymap.hint(action, &app.prefix.short).unwrap_or_default(),
        action,
    };
    let (title, about, items) = match loc {
        None => (
            "Welcome to velmux".to_string(),
            vec![
                "Run many coding agents at once, each on its own warm git worktree.",
                "Add a repo: agents plan in its checkout, then claim a slot (a",
                "worktree with its dependencies already installed) to work in.",
            ],
            vec![item('n', "add a repo", A::NewSpace), item('s', "start a scratch session", A::NewSession), item('?', "every key", A::Help)],
        ),
        Some(l) if app.projects[l.project].is_unclassified() => (
            app.loc_name(l),
            vec!["a scratch session"],
            vec![
                item('a', "Claude here", A::AgentHere),
                item('c', "shell here", A::NewShell),
                item('w', "go somewhere else", A::LocationPicker),
                item('x', "remove this session", A::RemovePlace),
            ],
        ),
        Some(l) if l.slot.is_some() => (
            app.loc_name(l),
            vec!["a claimed slot"],
            vec![
                item('a', "Claude in this slot", A::AgentHere),
                item('c', "shell here", A::NewShell),
                item('w', "go somewhere else", A::LocationPicker),
                item('x', "release this slot", A::RemovePlace),
            ],
        ),
        Some(l) => (
            app.loc_name(l),
            vec!["the repo's checkout: agents plan here, then claim a slot to work in"],
            vec![
                item('a', "new Claude conversation", A::AgentBase),
                item('n', "claim a slot yourself", A::ClaimSlot),
                item('c', "shell here", A::NewShell),
                item('w', "go somewhere else", A::LocationPicker),
                item('x', "remove this repo from velmux", A::RemovePlace),
            ],
        ),
    };
    let base = loc.and_then(|l| app.projects[l.project].base());
    let mut setup = Vec::new();
    if !crate::skill::installed(&app.claude_dir, base) {
        setup.push(item('k', "teach Claude to work in velmux (installs a skill)", A::InstallSkill));
    }
    if crate::ghostty::detected() && !crate::ghostty::configured() {
        setup.push(item('g', "set up the ⌘ shortcuts in Ghostty", A::SetupGhostty));
    }
    if !setup.is_empty() || loc.is_none() {
        setup.push(item('d', "check your setup", A::Doctor));
    }
    (title, about, items, setup)
}

/// Where an empty screen's parts go.
struct EmptyLayout {
    area: Rect,
    /// Each item with its row.
    rows: Vec<(u16, EmptyItem)>,
    title: String,
    about: Vec<&'static str>,
    /// The "get set up" heading's row, when there's setup left.
    setup_title: Option<u16>,
}

/// The empty screen's layout: where each part goes, and each item's row.
fn empty_layout(app: &App, loc: Option<&Loc>, body: Rect) -> EmptyLayout {
    let (title, about, items, setup) = empty_screen(app, loc);
    let setup_rows = if setup.is_empty() { 0 } else { setup.len() as u16 + 2 };
    let height = 2 + about.len() as u16 + 1 + items.len() as u16 + setup_rows;
    let width = 72.min(body.width.saturating_sub(4));
    let x = body.x + body.width.saturating_sub(width) / 2;
    let y = body.y + body.height.saturating_sub(height) / 3;
    let area = Rect::new(x, y, width, height);
    let mut rows = Vec::new();
    let mut ry = y + 2 + about.len() as u16;
    for it in items {
        rows.push((ry, it));
        ry += 1;
    }
    let setup_title = (!setup.is_empty()).then_some(ry + 1);
    ry += 2;
    for it in setup {
        rows.push((ry, it));
        ry += 1;
    }
    EmptyLayout { area, rows, title, about, setup_title }
}

/// The action under a click on the empty screen.
pub fn empty_hit(app: &App, body: Rect, x: u16, y: u16) -> Option<crate::keys::Action> {
    let EmptyLayout { area, rows, .. } = empty_layout(app, app.current_loc().as_ref(), body);
    if x < area.x || x >= area.right() {
        return None;
    }
    rows.into_iter().find(|(ry, _)| *ry == y).map(|(_, it)| it.action)
}

/// A place with no panes (or nothing added yet: the welcome): what it is, and
/// what you can do, a key each.
fn draw_empty(app: &App, loc: Option<&Loc>, body: Rect, buf: &mut Buffer) {
    let t = &app.theme;
    let EmptyLayout { area, rows, title, about, setup_title } = empty_layout(app, loc, body);
    let (x, y, width) = (area.x, area.y, area.width);
    buf.set_stringn(x, y, &title, width as usize, Style::new().fg(t.text).add_modifier(Modifier::BOLD));
    for (i, line) in about.iter().enumerate() {
        buf.set_stringn(x, y + 1 + i as u16, line, width as usize, Style::new().fg(t.dim));
    }
    if let Some(sy) = setup_title {
        buf.set_stringn(x, sy, "get set up", width as usize, Style::new().fg(t.dim).add_modifier(Modifier::BOLD));
    }
    for (ry, it) in rows {
        buf.set_string(x, ry, it.key.to_string(), Style::new().fg(t.accent).add_modifier(Modifier::BOLD));
        let hint_w = it.hint.chars().count() as u16;
        let room = width.saturating_sub(4 + hint_w + 2) as usize;
        buf.set_stringn(x + 3, ry, &it.label, room, Style::new().fg(t.text2));
        if hint_w > 0 && hint_w + 8 < width {
            buf.set_string(x + width - hint_w, ry, &it.hint, Style::new().fg(t.dim));
        }
    }
}

/// A small floating box at the bottom centre of the pane area.
fn draw_toast(t: &Theme, text: &str, body: Rect, buf: &mut Buffer) -> Rect {
    use ratatui::widgets::{Block, BorderType, Borders, Clear, Widget};
    let width = (text.chars().count() as u16 + 6).min(body.width);
    let area = Rect::new(body.x + body.width.saturating_sub(width) / 2, body.bottom().saturating_sub(4), width, 3);
    Clear.render(area, buf);
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.toast_border))
        .render(area, buf);
    buf.set_string(area.x + 2, area.y + 1, "●", Style::new().fg(t.accent));
    buf.set_stringn(area.x + 4, area.y + 1, text, width.saturating_sub(5) as usize, Style::new().fg(t.text).add_modifier(Modifier::BOLD));
    area
}

/// The theme list, each with a swatch of its main colours; the highlighted one
/// is already applied to the whole UI as a preview.
fn draw_theme_picker(app: &App, selected: usize, buf: &mut Buffer) {
    use ratatui::widgets::{Block, BorderType, Borders, Clear, Widget};
    let t = &app.theme;
    let names = crate::theme::NAMES;
    let width = 34.min(buf.area.width.saturating_sub(2));
    let height = (names.len() as u16 + 2).min(buf.area.height.saturating_sub(2));
    let area = Rect::new(buf.area.width.saturating_sub(width) / 2, 2, width, height);
    Clear.render(area, buf);
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.menu_border))
        .title(" theme ")
        .render(area, buf);
    let inner = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), area.height.saturating_sub(2));
    for (i, name) in names.iter().enumerate().take(inner.height as usize) {
        let y = inner.y + i as u16;
        let on = i == selected;
        let base = if on { Style::new().bg(t.selected_bg) } else { Style::new() };
        buf.set_style(Rect::new(inner.x, y, inner.width, 1), base);
        if on {
            buf.set_string(inner.x, y, "▌", base.fg(t.accent));
        }
        let text = if on { base.fg(t.text).add_modifier(Modifier::BOLD) } else { base.fg(t.text2) };
        buf.set_stringn(inner.x + 2, y, name, inner.width.saturating_sub(10) as usize, text);
        if let Some(theme) = crate::theme::builtin(name) {
            let swatch = [theme.accent, theme.base_glyph, theme.slot_glyph, theme.working, theme.blocked, theme.done];
            let x0 = inner.right().saturating_sub(swatch.len() as u16 + 1);
            for (k, c) in swatch.iter().enumerate() {
                buf.set_string(x0 + k as u16, y, "●", base.fg(*c));
            }
        }
    }
}

/// The short menu: label left, its shortcut right.
fn draw_menu(app: &App, selected: usize, buf: &mut Buffer) {
    let t = &app.theme;
    use ratatui::widgets::{Block, BorderType, Borders, Clear, Widget};
    let area = menu_area(app, buf.area.width, buf.area.height);
    Clear.render(area, buf);
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.menu_border))
        .title(match &app.mode {
            Mode::Menu { context: Some(c), .. } => format!(" {} ", c.title),
            _ => " menu ".into(),
        })
        .render(area, buf);
    // Key hints act on where you are, so a right-click menu (for another place) leaves them out.
    let hints = !matches!(app.mode, Mode::Menu { context: Some(_), .. });
    let inner = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), area.height.saturating_sub(2));
    for (i, (label, action)) in menu_items(app).iter().enumerate().take(inner.height as usize) {
        let y = inner.y + i as u16;
        let on = i == selected;
        let base = if on { Style::new().bg(t.selected_bg) } else { Style::new() };
        buf.set_style(Rect::new(inner.x, y, inner.width, 1), base);
        if on {
            buf.set_string(inner.x, y, "▌", base.fg(t.accent));
        }
        let text = if on { base.fg(t.text).add_modifier(Modifier::BOLD) } else { base.fg(t.text2) };
        buf.set_stringn(inner.x + 2, y, label, inner.width.saturating_sub(3) as usize, text);
        if let Some(hint) = app.keymap.hint(*action, &app.prefix.short).filter(|_| hints) {
            let w = hint.chars().count() as u16;
            if w + label.chars().count() as u16 + 4 < inner.width {
                buf.set_string(inner.right() - w - 1, y, hint, base.fg(t.dim));
            }
        }
    }
}

/// The conversation search: two lines per hit (title, where it ran and how
/// long ago; then the text around the match). Returns the cursor.
fn draw_sessions(app: &App, query: &str, selected: usize, body: Rect, buf: &mut Buffer) -> Option<Position> {
    use ratatui::widgets::{Block, BorderType, Borders, Clear, Widget};
    let t = &app.theme;
    let hits = crate::search::search(&app.sessions, query, crate::server::SESSION_HITS);
    let width = 96.min(body.width.saturating_sub(2)).max(24);
    let height = body.height.saturating_sub(2).clamp(6, 30);
    let area = Rect::new(body.x + body.width.saturating_sub(width) / 2, body.y + 1, width, height);
    Clear.render(area, buf);
    let title = if app.indexing { " find conversation · indexing… " } else { " find conversation " };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.picker_border))
        .title(title)
        .title_bottom(ratatui::text::Span::styled(format!(" {} of {} ", hits.len(), app.sessions.len()), Style::new().fg(t.dim)))
        .render(area, buf);
    let inner = Rect::new(area.x + 2, area.y + 1, area.width.saturating_sub(4), area.height.saturating_sub(2));
    let prompt = format!("› {query}");
    buf.set_stringn(inner.x, inner.y, &prompt, inner.width as usize, Style::new().fg(t.text).add_modifier(Modifier::BOLD));
    let per = 2usize;
    let visible = (inner.height.saturating_sub(2) as usize / per).max(1);
    let first = selected.saturating_sub(visible.saturating_sub(1));
    if hits.is_empty() {
        let msg = if app.indexing { "indexing…" } else { "no conversations match" };
        buf.set_stringn(inner.x, inner.y + 2, msg, inner.width as usize, Style::new().fg(t.dim));
    }
    let now = std::time::SystemTime::now();
    for (row, hit) in hits.iter().enumerate().skip(first).take(visible) {
        let y = inner.y + 2 + ((row - first) * per) as u16;
        let on = row == selected;
        let base = if on { Style::new().bg(t.selected_bg) } else { Style::new() };
        buf.set_style(Rect::new(inner.x.saturating_sub(1), y, inner.width + 2, per as u16), base);
        if on {
            buf.set_string(inner.x.saturating_sub(1), y, "▌", base.fg(t.accent));
            buf.set_string(inner.x.saturating_sub(1), y + 1, "▌", base.fg(t.accent));
        }
        // Right: open in a pane, where it ran, and its age.
        let live = app.live_session_pane(&hit.doc.id, None).is_some();
        let place = app.session_loc(hit.doc).map(|(l, _)| app.loc_name(&l)).unwrap_or_default();
        let branch = hit.doc.branch.as_deref().map(|b| format!(" · {b}")).unwrap_or_default();
        let age = ago(now.duration_since(hit.doc.mtime).unwrap_or_default().as_secs());
        let right = format!("{place}{branch} · {age}");
        let right_w = (right.chars().count() as u16).min(inner.width / 2);
        let live_w = if live { 2 } else { 0 };
        let title_room = inner.width.saturating_sub(right_w + live_w + 2) as usize;
        if live {
            buf.set_string(inner.x, y, "●", base.fg(t.working));
        }
        let title_style = base.fg(t.text).add_modifier(if on { Modifier::BOLD } else { Modifier::empty() });
        buf.set_stringn(inner.x + live_w, y, &hit.doc.title, title_room, title_style);
        buf.set_stringn(inner.right() - right_w, y, &right, right_w as usize, base.fg(t.dim));
        buf.set_stringn(inner.x + live_w, y + 1, &hit.snippet, inner.width.saturating_sub(live_w) as usize, base.fg(t.text2));
    }
    Some(Position::new((inner.x + prompt.chars().count() as u16).min(inner.right().saturating_sub(1)), inner.y))
}

/// Two lists of sounds side by side; the active one's highlight is bright.
fn draw_sound_picker(app: &App, col: usize, done: usize, blocked: usize, body: Rect, buf: &mut Buffer) {
    use crate::notify::{BLOCKED_SOUNDS, DONE_SOUNDS};
    let t = &app.theme;
    let rows = DONE_SOUNDS.len().max(BLOCKED_SOUNDS.len()) as u16;
    let inner = popup(t, "sounds", 44, rows + 5, body, buf);
    let half = inner.width / 2;
    for (c, (title, list, at)) in [("finished", DONE_SOUNDS, done), ("blocked on you", BLOCKED_SOUNDS, blocked)].into_iter().enumerate() {
        let x = inner.x + c as u16 * half;
        let active = c == col;
        buf.set_stringn(x, inner.y, title, half as usize, Style::new().fg(if active { t.text } else { t.dim }).add_modifier(Modifier::BOLD));
        for (i, name) in list.iter().enumerate() {
            let y = inner.y + 2 + i as u16;
            let on = i == at;
            let style = match (on, active) {
                (true, true) => Style::new().bg(t.selected_bg).fg(t.text).add_modifier(Modifier::BOLD),
                (true, false) => Style::new().fg(t.accent),
                _ => Style::new().fg(t.text2),
            };
            if on && active {
                buf.set_style(Rect::new(x, y, half.saturating_sub(2), 1), style);
            }
            let mark = if on { "▌" } else { " " };
            buf.set_string(x, y, mark, style.fg(t.accent));
            buf.set_stringn(x + 2, y, name, half.saturating_sub(4) as usize, style);
        }
    }
    buf.set_stringn(inner.x, inner.y + rows + 2, "space plays it again", inner.width as usize, Style::new().fg(t.dim));
}

/// The `new space` chooser: repos in the scratch dir, or a typed path.
fn draw_new_space(app: &App, query: &str, selected: usize, repos: &[std::path::PathBuf], body: Rect, buf: &mut Buffer) -> Option<Position> {
    use ratatui::widgets::{Block, BorderType, Borders, Clear, Widget};
    let t = &app.theme;
    let hits = App::space_matches(repos, query);
    let width = 64.min(body.width.saturating_sub(2)).max(24);
    let rows = (hits.len() as u16).clamp(1, body.height.saturating_sub(6).max(1));
    let height = (rows + 4).min(body.height);
    let area = Rect::new(body.x + body.width.saturating_sub(width) / 2, body.y + 1, width, height);
    Clear.render(area, buf);
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.picker_border))
        .title(" new space ")
        .title_bottom(ratatui::text::Span::styled(format!(" repos in {} · or type a path ", short_home(&app.scratch_dir)), Style::new().fg(t.dim)))
        .render(area, buf);
    let inner = Rect::new(area.x + 2, area.y + 1, area.width.saturating_sub(4), area.height.saturating_sub(2));
    let prompt = format!("› {query}");
    buf.set_stringn(inner.x, inner.y, &prompt, inner.width as usize, Style::new().fg(t.text).add_modifier(Modifier::BOLD));
    let visible = inner.height.saturating_sub(2) as usize;
    let first = selected.saturating_sub(visible.saturating_sub(1));
    if hits.is_empty() {
        let msg = if repos.is_empty() { "no other repos here — type a path" } else { "no matches" };
        buf.set_stringn(inner.x, inner.y + 2, msg, inner.width as usize, Style::new().fg(t.dim));
    }
    let typed_path = query.trim_start().starts_with(['/', '~']);
    for (row, path) in hits.iter().enumerate().skip(first).take(visible) {
        let y = inner.y + 2 + (row - first) as u16;
        let on = row == selected;
        let base = if on { Style::new().bg(t.selected_bg) } else { Style::new() };
        buf.set_style(Rect::new(inner.x.saturating_sub(1), y, inner.width + 2, 1), base);
        if on {
            buf.set_string(inner.x.saturating_sub(1), y, "▌", base.fg(t.accent));
        }
        let name = if typed_path { format!("open {}", short_home(path)) } else { path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() };
        let style = if on { base.fg(t.text).add_modifier(Modifier::BOLD) } else { base.fg(t.text2) };
        buf.set_stringn(inner.x + 1, y, &name, inner.width.saturating_sub(1) as usize, style);
    }
    Some(Position::new((inner.x + prompt.chars().count() as u16).min(inner.right().saturating_sub(1)), inner.y))
}

/// `~/Workspace` for `$HOME/Workspace`.
fn short_home(path: &std::path::Path) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    match path.strip_prefix(&home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()).trim_end_matches('/').to_string(),
        _ => path.display().to_string(),
    }
}

/// `now`, `5m`, `3h`, `2d`.
fn ago(secs: u64) -> String {
    match secs {
        0..60 => "now".into(),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
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
        .border_style(Style::new().fg(app.theme.picker_border))
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
        buf.set_stringn(inner.x, inner.y + 1, "no matches", inner.width as usize, Style::new().fg(app.theme.dim));
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
            .map(|pane| state_glyph(&app.theme, pane).0)
            .filter(|g| *g != " ")
            .collect();
        let w = glyphs.chars().count() as u16;
        if w > 0 && w + 2 < inner.width {
            buf.set_string(inner.right() - w - 1, y, glyphs, base.fg(app.theme.working));
        }
    }
    Some(Position::new((inner.x + prompt.chars().count() as u16).min(inner.right().saturating_sub(1)), inner.y))
}

/// Selection highlight, scroll position and the copy cursor over a frozen pane.
fn draw_copy(t: &Theme, c: &Copy, pane: &Pane, area: Rect, buf: &mut Buffer) -> Option<Position> {
    let top = c.top(&pane.parser);
    let sel = Style::new().bg(t.copy_bg).fg(t.copy_fg);
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
        buf.set_string(area.right() - w, area.y, tag, Style::new().bg(t.tag_bg).fg(t.tag_fg));
    }
    let row = c.cursor.0.checked_sub(top).filter(|r| *r < area.height as usize)?;
    (c.search.is_none()).then(|| Position::new(area.x + c.cursor.1.min(area.width - 1), area.y + row as u16))
}

fn draw_title(app: &App, pane: &Pane, outer: Rect, focused: bool, buf: &mut Buffer) {
    let row = Rect::new(outer.x, outer.y, outer.width, 1);
    let style = if focused {
        Style::new().bg(app.theme.focus_bg).fg(app.theme.focus_fg).add_modifier(Modifier::BOLD)
    } else {
        Style::new().bg(app.theme.title_bg).fg(app.theme.title_fg)
    };
    buf.set_style(row, style);
    let (glyph, gstyle) = state_glyph(&app.theme, pane);
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
