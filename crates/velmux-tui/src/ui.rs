use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};

use velmux_pool::Status;

use crate::copy::Copy;
use crate::detect::AgentState;
use crate::layout::Dir;
use crate::pane::{Kind, Loc, Pane};
use crate::server::{App, Mode};

pub fn sidebar_width(cols: u16) -> u16 {
    32.min(cols / 3).max(12)
}

/// The rectangle every pane is sized to.
pub fn pane_area(cols: u16, rows: u16) -> Rect {
    let x = sidebar_width(cols) + 1;
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
pub enum Row {
    Project(usize),
    Location(Loc),
    Pane(usize),
}

pub fn rows(app: &App) -> Vec<Row> {
    let mut out = Vec::new();
    // Repos first, then the unclassified group of plain folders.
    let mut order: Vec<usize> = (0..app.projects.len()).collect();
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

/// Sidebar rows with the screen line each is drawn on (a blank line separates
/// projects). Shared by drawing and mouse hit-testing.
pub fn sidebar_rows(app: &App, height: u16) -> Vec<(u16, Row)> {
    let mut out = Vec::new();
    let mut y = 0;
    for row in rows(app) {
        if matches!(row, Row::Project(_)) && y > 0 {
            y += 1;
        }
        if y + 1 >= height {
            break;
        }
        out.push((y, row));
        y += 1;
    }
    out
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

pub fn pane_label(pane: &Pane) -> &'static str {
    match (pane.agent, pane.kind) {
        (Some(agent), _) => agent,
        (None, Kind::Agent) => "agent",
        (None, Kind::Shell) => "shell",
        (None, Kind::Editor) => "editor",
    }
}

/// Draw the whole UI; returns where the hardware cursor belongs, if visible.
pub fn draw(app: &App, buf: &mut Buffer) -> Option<Position> {
    let area = buf.area;
    let (cols, rows) = (area.width, area.height);
    let side = sidebar_width(cols);
    let focused = app.focused();
    let current = app.current_loc();

    // Sidebar.
    let dim = Style::new().fg(Color::DarkGray);
    for (y, row) in sidebar_rows(app, rows) {
        match row {
            Row::Project(p) => {
                let name = app.projects[p].name();
                buf.set_stringn(0, y, name, side as usize, Style::new().add_modifier(Modifier::BOLD));
            }
            Row::Location(loc) => {
                let is_current = current.as_ref() == Some(&loc);
                let marker = if is_current { "›" } else { " " };
                let unclassified = app.projects[loc.project].is_unclassified();
                let (text, style) = match &loc.slot {
                    Some(id) if unclassified => (format!("▸ {id}"), Style::new().fg(Color::Yellow)),
                    None => ("◆ base".to_string(), Style::new().fg(Color::Magenta)),
                    Some(id) => {
                        let slot = app.projects[loc.project].slots.iter().find(|s| &s.id == id);
                        match slot {
                            Some(s) if s.status == Status::InUse => {
                                let branch = s.branch.as_deref().unwrap_or("detached");
                                let dirty = if s.dirty { "*" } else { "" };
                                (format!("▣ {id} {branch}{dirty}"), Style::new().fg(Color::Cyan))
                            }
                            _ => (format!("□ {id} free"), dim),
                        }
                    }
                };
                buf.set_stringn(0, y, format!("{marker}{text}"), side as usize, style);
            }
            Row::Pane(i) => {
                let pane = &app.panes[i];
                let selected = focused.map(|f| f.id) == Some(pane.id);
                let (glyph, gstyle) = state_glyph(pane);
                let style = if selected {
                    Style::new().add_modifier(Modifier::REVERSED)
                } else {
                    Style::new()
                };
                buf.set_stringn(3, y, glyph, 1, gstyle);
                let label = format!(" {} ", pane_label(pane));
                buf.set_stringn(4, y, label, side.saturating_sub(4) as usize, style);
            }
        }
    }
    for y in 0..rows.saturating_sub(1) {
        buf.set_string(side, y, "│", dim);
    }

    // Header + pane body.
    let body = pane_area(cols, rows);
    let header = match (&focused, &current) {
        (Some(pane), _) => {
            let title = pane.title().trim();
            format!("{} · {}{}", app.loc_name(&pane.loc), pane_label(pane),
                if title.is_empty() { String::new() } else { format!(" — {title}") })
        }
        (None, Some(loc)) => app.loc_name(loc),
        (None, None) => "velmux".into(),
    };
    buf.set_stringn(body.x, 0, header, body.width as usize, Style::new().add_modifier(Modifier::BOLD));

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
                    draw_title(pane, outer, is_focused, buf);
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
        Mode::Prefix => "n slot · a agent@base · c/e shell/editor · |/- split · arrows move · z zoom · HJKL resize · j/k cycle · 1-9 slot · r release · x close · d detach · Q quit".to_string(),
        Mode::Prompt { label, input, .. } => format!("{label}: {input}"),
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

fn draw_title(pane: &Pane, outer: Rect, focused: bool, buf: &mut Buffer) {
    let row = Rect::new(outer.x, outer.y, outer.width, 1);
    let style = if focused {
        Style::new().bg(Color::Indexed(24)).fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::new().bg(Color::Indexed(236)).fg(Color::Gray)
    };
    buf.set_style(row, style);
    let (glyph, gstyle) = state_glyph(pane);
    buf.set_stringn(row.x + 1, row.y, glyph, 1, gstyle.patch(Style::new().bg(style.bg.unwrap_or(Color::Reset))));
    let title = pane.title().trim();
    let text = if title.is_empty() {
        pane_label(pane).to_string()
    } else {
        format!("{} — {title}", pane_label(pane))
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
