use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::{Instant, SystemTime};

use anyhow::{anyhow, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::detect::{self, AgentState};
use crate::server::Event;

/// Where a pane lives: a project's base checkout (`slot: None`) or one of its slots.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Loc {
    pub project: usize,
    pub slot: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    Agent,
    Shell,
    Editor,
}

/// The terminal side of a pane: captures OSC titles (agents report progress
/// there) and answers the queries a real terminal answers, so apps that ask
/// where the cursor is or what the background colour is don't stall.
#[derive(Default)]
pub struct Term {
    pub title: String,
    /// What the program reports about itself (OSC 7501 `state=`: Codex, or
    /// any program that isn't a known agent; see [`Pane::reported_state`]).
    pub status: Option<String>,
    /// Its last progress report (OSC 9;4), as `4;<state>;<percent>`.
    pub progress: String,
    /// Replies to write back to the app.
    replies: Vec<u8>,
}

impl vt100::Callbacks for Term {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
    }

    fn unhandled_csi(&mut self, screen: &mut vt100::Screen, i1: Option<u8>, _: Option<u8>, params: &[&[u16]], c: char) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let reply = match (i1, c, first) {
            // Device status: OK.
            (None, 'n', 5) => "\x1b[0n".to_string(),
            // Cursor position report (1-based).
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                format!("\x1b[{};{}R", row + 1, col + 1)
            }
            // Primary / secondary device attributes, answered like tmux.
            (None, 'c', 0) => "\x1b[?1;2c".to_string(),
            (Some(b'>'), 'c', 0) => "\x1b[>84;0;0c".to_string(),
            _ => return,
        };
        self.replies.extend_from_slice(reply.as_bytes());
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        // OSC 7501 ; state=<idle|working|blocked|done|error>[:app=…][:kind=…]
        if params.first() == Some(&&b"7501"[..]) {
            let text = params[1..].iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect::<Vec<_>>().join(";");
            if let Some(state) = text.strip_prefix("state=") {
                let state = state.split(':').next().unwrap_or_default();
                self.status = (state != "clear" && !state.is_empty()).then(|| state.to_string());
            }
            return;
        }
        // OSC 9;4 ; <state> ; <percent>: a progress report (some agents' "working").
        if params.first() == Some(&&b"9"[..]) && params.get(1) == Some(&&b"4"[..]) {
            self.progress = params[1..].iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect::<Vec<_>>().join(";");
            return;
        }
        // OSC 10/11 colour queries: report a dark theme (light fg on black).
        if let [which @ (b"10" | b"11"), b"?"] = params {
            let rgb = if *which == b"10" { "c0c0/c0c0/c0c0" } else { "0000/0000/0000" };
            let which = String::from_utf8_lossy(which);
            self.replies.extend_from_slice(format!("\x1b]{which};rgb:{rgb}\x1b\\").as_bytes());
        }
    }
}

/// What to run in a new pane.
pub struct Launch<'a> {
    pub cwd: &'a Path,
    /// Run through the login shell, which takes over when it exits; `None` = plain shell.
    pub command: Option<&'a str>,
    pub env: &'a [(String, String)],
}

pub struct Pane {
    pub id: u64,
    pub loc: Loc,
    pub kind: Kind,
    pub parser: vt100::Parser<Term>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// Agent currently in the foreground (re-detected on each tick).
    pub agent: Option<&'static str>,
    /// Foreground program and its working directory, for the label.
    pub fg: detect::Foreground,
    pub state: Option<AgentState>,
    /// When `state` last changed (how long it's been blocked, working…).
    pub state_since: Instant,
    idle_streak: u8,
    /// The process whose OSC 7501 report `state` comes from.
    reporter: Option<detect::Pid>,
    /// Agent finished while the pane wasn't focused.
    pub unseen_done: bool,
    pub last_output: Instant,
    /// The agent session this pane runs (a reference naming its agent; see
    /// `agents`), so restore and search can bring it back.
    pub session: Option<String>,
    /// When its session was last looked for (agents found by scanning).
    probed: Option<Instant>,
    pub started_at: SystemTime,
    /// Launch overrides, persisted so restore reproduces the pane.
    pub cwd_override: Option<std::path::PathBuf>,
    pub command_override: Option<String>,
    /// Output held back while copy mode reads the history (`None` = live).
    frozen: Option<Vec<u8>>,
    /// Name of this pane's saved history file (see [`history_formatted`](Pane::history_formatted)).
    pub history_key: String,
    /// Bumped on output; the history is rewritten when it moved past `dumped`.
    pub output_seq: u64,
    pub dumped_seq: u64,
}

/// Every scrollback row (and, `with_screen` and not on the alternate screen,
/// the visible rows), formatted, one per line, oldest first.
pub fn history_formatted<CB: vt100::Callbacks>(p: &mut vt100::Parser<CB>, with_screen: bool) -> String {
    let mut out = String::new();
    for text in history_rows(p, with_screen, true) {
        out.push_str(text.trim_end_matches(' '));
        out.push_str("\x1b[m\r\n");
    }
    out
}

/// The same rows as plain text, for reading in an editor.
pub fn history_text<CB: vt100::Callbacks>(p: &mut vt100::Parser<CB>) -> String {
    let mut out = String::new();
    for text in history_rows(p, true, false) {
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out
}

fn history_rows<CB: vt100::Callbacks>(p: &mut vt100::Parser<CB>, with_screen: bool, formatted: bool) -> Vec<String> {
    let saved = p.screen().scrollback();
    let total = p.screen().scrollback_len();
    let (rows, cols) = p.screen().size();
    let (rows, cols) = (rows.get() as usize, cols.get());
    let visible = |p: &vt100::Parser<CB>| -> Vec<String> {
        if formatted { p.screen().rows_formatted(0, cols).collect() } else { p.screen().rows(0, cols).collect() }
    };
    let mut out = Vec::new();
    // At offset `o` the top visible row is history line `total - o`.
    let mut offset = total;
    while offset > 0 {
        p.screen_mut().set_scrollback(offset);
        let take = rows.min(offset);
        out.extend(visible(p).into_iter().take(take));
        offset -= take;
    }
    p.screen_mut().set_scrollback(0);
    if with_screen && !p.screen().alternate_screen() {
        let rows = visible(p);
        // Blank rows at the bottom of the screen aren't history.
        let plain: Vec<String> = p.screen().rows(0, cols).collect();
        let used = plain.iter().rposition(|r| !r.trim().is_empty()).map_or(0, |i| i + 1);
        out.extend(rows.into_iter().take(used));
    }
    p.screen_mut().set_scrollback(saved);
    out
}

/// Feed `history` (from [`history_formatted`]) into a fresh parser so it all
/// lands in the scrollback, leaving the screen empty with the cursor home.
pub fn replay_history<CB: vt100::Callbacks>(p: &mut vt100::Parser<CB>, history: &str, note: Option<&str>) {
    if history.is_empty() {
        return;
    }
    let rows = p.screen().size().0.get();
    p.process(b"\x1b[H\x1b[2J");
    p.process(history.as_bytes());
    if let Some(note) = note {
        p.process(format!("\x1b[2m── {note} ──\x1b[m\r\n").as_bytes());
    }
    // Push what's still on screen up into the scrollback: from the bottom row,
    // each newline scrolls the top row off.
    let on_screen = p.screen().cursor_position().0;
    p.process(format!("\x1b[{rows};1H").as_bytes());
    p.process("\n".repeat(on_screen as usize).as_bytes());
    p.process(b"\x1b[H\x1b[2J");
}

/// Past this much held output, copy mode stops freezing the pane.
const FREEZE_CAP: usize = 16 << 20;

fn nz(v: u16) -> NonZeroU16 {
    NonZeroU16::new(v.max(1)).unwrap()
}

impl Pane {
    pub fn spawn(
        id: u64,
        loc: Loc,
        kind: Kind,
        launch: Launch,
        (cols, rows): (u16, u16),
        events: Sender<Event>,
    ) -> Result<Pane> {
        let size = PtySize { rows: rows.max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0 };
        let pair = native_pty_system().openpty(size).map_err(|e| anyhow!("openpty: {e}"))?;
        let mut cmd = shell_command(launch.command);
        cmd.cwd(launch.cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("VELMUX", "1");
        cmd.env("VELMUX_PANE", id.to_string());
        for (k, v) in launch.env {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).map_err(|e| anyhow!("spawn: {e}"))?;
        drop(pair.slave);

        Pane::assemble(id, loc, kind, pair.master, child, (cols, rows), events)
    }

    /// Rebuild a pane from a pty kept open across a live restart.
    #[cfg(unix)]
    pub fn adopt(id: u64, loc: Loc, kind: Kind, live: &crate::session::LivePane, events: Sender<Event>) -> Result<Pane> {
        let master: Box<dyn MasterPty + Send> = Box::new(crate::pty_raw::RawMaster::new(live.fd));
        let child: Box<dyn Child + Send + Sync> = Box::new(crate::pty_raw::RawChild::new(live.pid));
        let mut pane = Pane::assemble(id, loc, kind, master, child, (live.cols, live.rows), events)?;
        replay_history(&mut pane.parser, &live.history, None);
        pane.parser.process(live.screen.as_bytes());
        pane.output_seq = 1; // its history isn't saved under this pane's key yet
        pane.parser.callbacks_mut().title = live.title.clone();
        Ok(pane)
    }

    /// Windows has no live restart: its panes are started again instead.
    #[cfg(windows)]
    pub fn adopt(_: u64, _: Loc, _: Kind, _: &crate::session::LivePane, _: Sender<Event>) -> Result<Pane> {
        Err(anyhow!("no live restart on Windows"))
    }

    /// Keep this pane's pty open across `exec`, describing it for the next server.
    #[cfg(unix)]
    pub fn handoff(&mut self) -> Option<crate::session::LivePane> {
        if let Some(held) = self.frozen.take() {
            self.parser.process(&held);
        }
        self.parser.screen_mut().set_scrollback(0);
        let fd = self.master.as_raw_fd()?;
        crate::pty_raw::keep_across_exec(fd).ok()?;
        let (rows, cols) = self.parser.screen().size();
        Some(crate::session::LivePane {
            id: self.id,
            fd,
            pid: self.child.process_id()?,
            rows: rows.get(),
            cols: cols.get(),
            screen: self.parser.screen().state_formatted(),
            history: history_formatted(&mut self.parser, false),
            title: self.title().to_string(),
        })
    }

    fn assemble(
        id: u64,
        loc: Loc,
        kind: Kind,
        master: Box<dyn MasterPty + Send>,
        child: Box<dyn Child + Send + Sync>,
        (cols, rows): (u16, u16),
        events: Sender<Event>,
    ) -> Result<Pane> {
        let mut reader = master.try_clone_reader().map_err(|e| anyhow!("reader: {e}"))?;
        let writer = master.take_writer().map_err(|e| anyhow!("writer: {e}"))?;
        std::thread::spawn(move || {
            let mut buf = [0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if events.send(Event::PtyOutput(id, buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = events.send(Event::PtyExit(id));
        });

        Ok(Pane {
            id,
            loc,
            kind,
            parser: vt100::Parser::new_with_callbacks(nz(rows), nz(cols), 5000, Term::default()),
            master,
            writer,
            child,
            agent: None,
            fg: detect::Foreground::default(),
            state: None,
            state_since: Instant::now(),
            idle_streak: 0,
            reporter: None,
            unseen_done: false,
            last_output: Instant::now(),
            session: None,
            probed: None,
            started_at: SystemTime::now(),
            cwd_override: None,
            command_override: None,
            frozen: None,
            history_key: format!("{}-{id}", SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis())),
            output_seq: 0,
            dumped_seq: 0,
        })
    }

    /// The pane's history as replayable terminal text, oldest first: the
    /// scrollback, then (`with_screen`, main screen only) the visible rows.
    pub fn history_formatted(&mut self, with_screen: bool) -> String {
        history_formatted(&mut self.parser, with_screen)
    }

    /// Put saved history back into the scrollback (above an empty screen),
    /// with `note` as a dim last line when given.
    pub fn restore_history(&mut self, history: &str, note: Option<&str>) {
        replay_history(&mut self.parser, history, note);
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.last_output = Instant::now();
        self.output_seq += 1;
        if let Some(held) = &mut self.frozen {
            if held.len() + bytes.len() <= FREEZE_CAP {
                held.extend_from_slice(bytes);
                return;
            }
            let held = self.frozen.take().unwrap_or_default();
            self.parser.process(&held);
        }
        self.parser.process(bytes);
        self.send_replies();
    }

    fn send_replies(&mut self) {
        let replies = std::mem::take(&mut self.parser.callbacks_mut().replies);
        if !replies.is_empty() {
            self.write_raw(&replies);
        }
    }

    /// Hold output so the history copy mode is reading doesn't move.
    pub fn freeze(&mut self) {
        self.frozen.get_or_insert_with(Vec::new);
    }

    /// Back to the live view, applying everything held while frozen.
    pub fn thaw(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
        if let Some(held) = self.frozen.take() {
            self.parser.process(&held);
            self.send_replies();
        }
    }

    /// The pty's own size (cols, rows), which can differ from the parser's.
    pub fn pty_size(&self) -> Option<(u16, u16)> {
        self.master.get_size().ok().map(|s| (s.cols, s.rows))
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (cur_rows, cur_cols) = self.parser.screen().size();
        if cur_rows.get() == rows.max(1) && cur_cols.get() == cols.max(1) {
            return;
        }
        let _ = self.master.resize(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        });
        self.parser.set_size(nz(rows), nz(cols));
    }

    /// Forward client keystrokes, adapting them to the modes the app asked for.
    pub fn write_input(&mut self, bytes: &[u8]) {
        let screen = self.parser.screen();
        let mut data = bytes.to_vec();
        if !screen.bracketed_paste() {
            data = strip(&data, b"\x1b[200~");
            data = strip(&data, b"\x1b[201~");
        }
        if screen.application_cursor() {
            for k in *b"ABCD" {
                data = replace(&data, &[0x1b, b'[', k], &[0x1b, b'O', k]);
            }
        }
        let _ = self.writer.write_all(&data);
        let _ = self.writer.flush();
    }

    /// Send bytes verbatim (already encoded for the app, e.g. mouse reports).
    pub fn write_raw(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    pub fn title(&self) -> &str {
        &self.parser.callbacks().title
    }

    /// Re-detect the foreground agent and its state. Returns true if anything changed.
    /// Re-detect agent, state and foreground program; also learn which Claude
    /// session is running here (from Claude's own pid record), so restore can
    /// resume it, even if you started `claude` by hand or ran `/clear`.
    pub fn detect(&mut self, focused: bool, claude_dir: &Path) -> bool {
        // A frozen pane shows scrolled history, not the agent's live screen.
        if self.frozen.is_some() {
            return false;
        }
        let pgid = self.group();
        let agent = detect::foreground_agent(pgid);
        let fg = detect::foreground(pgid);
        let fg_changed = fg != self.fg;
        let live = match (agent.and_then(crate::agents::Agent::from_name), fg.pid) {
            // Claude's per-process record is cheap to read every time.
            (Some(a @ crate::agents::Agent::Claude), Some(pid)) => crate::agents::running_session(a, pid, None, self.started_at, claude_dir),
            // The others mean scanning transcripts: only until it's known,
            // and not more than every few seconds.
            (Some(a), Some(pid)) if !self.session.as_deref().is_some_and(|s| crate::agents::parse_ref(s).0 == a) => {
                if self.probed.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(3)) {
                    None
                } else {
                    self.probed = Some(Instant::now());
                    crate::agents::running_session(a, pid, fg.cwd.as_deref(), self.started_at, claude_dir)
                }
            }
            _ => None,
        };
        match live {
            Some(id) => self.session = Some(id),
            // A shell whose Claude has exited is just a shell again.
            None if agent.is_none() && self.kind == Kind::Shell => self.session = None,
            None => {}
        }
        self.fg = fg;
        let state = match agent {
            Some(name) => {
                let screen = self.parser.screen();
                let (_, cols) = screen.size();
                let lines: Vec<String> = screen.rows(0, cols.get()).collect();
                let busy = self.last_output.elapsed().as_millis() < 1500;
                let cb = self.parser.callbacks();
                Some(detect::classify(name, self.title(), &lines, cb.status.as_deref(), &cb.progress, busy))
            }
            None => self.reported_state(),
        };
        // Debounce working → idle so a redraw gap doesn't flicker the sidebar.
        let state = if self.state == Some(AgentState::Working) && state == Some(AgentState::Idle) {
            self.idle_streak += 1;
            if self.idle_streak < 3 { self.state } else { state }
        } else {
            self.idle_streak = 0;
            state
        };
        if self.state == Some(AgentState::Working) && state == Some(AgentState::Idle) && !focused {
            self.unseen_done = true;
        }
        if focused {
            self.unseen_done = false;
        }
        let changed = agent != self.agent || state != self.state || fg_changed;
        self.agent = agent;
        if state != self.state {
            self.state_since = Instant::now();
        }
        self.state = state;
        changed
    }

    /// The state a program that isn't a known agent reports itself (OSC 7501
    /// `state=working|blocked|idle|done|error`, as Codex does), while it's in
    /// front: a report outlives its program, so it ends when the pane's own
    /// shell, or another program, is in front instead. `error` needs you.
    fn reported_state(&mut self) -> Option<AgentState> {
        let shell = self.child.process_id().map(|p| p as detect::Pid);
        let front = self.fg.pid;
        let reported = self.parser.callbacks().status.is_some();
        let stale = match self.reporter {
            _ if !reported => true,
            None => front.is_none() || front == shell,
            Some(pid) => Some(pid) != front,
        };
        if stale {
            self.parser.callbacks_mut().status = None;
            self.reporter = None;
            return None;
        }
        self.reporter = front;
        Some(match self.parser.callbacks().status.as_deref() {
            Some("working") => AgentState::Working,
            Some("blocked" | "error") => AgentState::Blocked,
            _ => AgentState::Idle,
        })
    }

    /// What detection looks through: the pty's foreground process group on
    /// Unix; on Windows, which has no such thing, the pane's process tree
    /// (named by its root, the shell).
    #[cfg(unix)]
    fn group(&self) -> Option<detect::Pid> {
        self.master.process_group_leader()
    }

    #[cfg(windows)]
    fn group(&self) -> Option<detect::Pid> {
        self.child.process_id().map(|p| p as detect::Pid)
    }

    /// Terminate everything running in the pane: its whole process tree (the
    /// console closing doesn't reach processes that left it).
    #[cfg(windows)]
    pub fn kill(&mut self) {
        if let Some(pid) = self.child.process_id() {
            detect::kill_tree(pid as detect::Pid);
        }
        let _ = self.child.kill();
    }

    /// Terminate everything running in the pane, not just its shell: `sh -c 'claude; …'`
    /// keeps the agent in the shell's process group, and it outlives a lone SIGHUP to
    /// the shell (reparented to init). Signal the groups, then SIGKILL stragglers.
    #[cfg(unix)]
    pub fn kill(&mut self) {
        let mut groups: Vec<libc::pid_t> = Vec::new();
        if let Some(pid) = self.child.process_id() {
            groups.push(pid as libc::pid_t);
        }
        if let Some(fg) = self.master.process_group_leader() {
            if !groups.contains(&fg) {
                groups.push(fg);
            }
        }
        groups.retain(|&g| g > 1);
        for &g in &groups {
            unsafe {
                libc::killpg(g, libc::SIGHUP);
                libc::killpg(g, libc::SIGTERM);
            }
        }
        let _ = self.child.kill();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            for g in groups {
                unsafe {
                    if libc::killpg(g, 0) == 0 {
                        libc::killpg(g, libc::SIGKILL);
                    }
                }
            }
        });
    }

    pub fn reap(&mut self) {
        let _ = self.child.try_wait();
    }

    /// Its program has exited. (On Windows the pane's output stays open until
    /// the pane itself is closed, so that's how its end is noticed there.)
    #[cfg(windows)]
    pub fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

/// The pane's shell, running `command` first when there is one.
#[cfg(unix)]
fn shell_command(command: Option<&str>) -> CommandBuilder {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut cmd = CommandBuilder::new(&shell);
    cmd.arg("-l");
    if let Some(command) = command {
        // Drop back into a shell when the command exits so the pane survives:
        // the one the pane started with, interactive for certain (`-i`), so
        // it reads your `.zshrc` / `.bashrc` whatever state the agent left
        // the terminal in.
        let quoted = format!("'{}'", shell.replace('\'', r"'\''"));
        cmd.arg("-c");
        cmd.arg(format!("{command}; exec {quoted} -l -i"));
    }
    cmd
}

/// The pane's shell on Windows: PowerShell, staying on once `command` exits
/// (`-NoExit`) so the pane survives, unless it's `exec <command>` (as on
/// Unix: the pane closes with it).
#[cfg(windows)]
fn shell_command(command: Option<&str>) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(velmux_pool::sys::powershell());
    // portable-pty starts a Windows program in the registry's environment (a
    // fresh login's), not velmux's: pass velmux's on, as a Unix pane gets it,
    // so a PATH or variable set where velmux was started reaches its panes.
    for (key, value) in std::env::vars_os() {
        cmd.env(key, value);
    }
    cmd.arg("-NoLogo");
    if let Some(command) = command {
        let (command, stay) = match command.strip_prefix("exec ") {
            Some(rest) => (rest, false),
            None => (command, true),
        };
        if stay {
            cmd.arg("-NoExit");
        }
        cmd.arg("-Command");
        // A quoted program is a string to PowerShell unless it's called (`&`).
        let call = if command.starts_with(['\'', '"']) { "& " } else { "" };
        cmd.arg(format!("{call}{command}"));
    }
    cmd
}

fn strip(data: &[u8], pat: &[u8]) -> Vec<u8> {
    replace(data, pat, &[])
}

fn replace(data: &[u8], pat: &[u8], with: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i..].starts_with(pat) {
            out.extend_from_slice(with);
            i += pat.len();
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(input: &[u8]) -> Vec<u8> {
        let mut p = vt100::Parser::new_with_callbacks(nz(10), nz(40), 0, Term::default());
        p.process(input);
        std::mem::take(&mut p.callbacks_mut().replies)
    }

    #[test]
    fn history_survives_a_dump_and_replay() {
        let mut p = vt100::Parser::new_with_callbacks(nz(5), nz(20), 100, Term::default());
        for i in 0..30 {
            p.process(format!("\x1b[31mline {i}\x1b[m\r\n").as_bytes());
        }
        p.process(b"prompt$ ");
        let text = history_text(&mut p);
        let dump = history_formatted(&mut p, true);

        let mut q = vt100::Parser::new_with_callbacks(nz(5), nz(20), 100, Term::default());
        replay_history(&mut q, &dump, None);
        assert_eq!(history_text(&mut q), text, "same lines, all in the scrollback");
        assert_eq!(q.screen().contents().trim(), "", "screen left empty");
        let back = q.screen().scrollback_len();
        q.screen_mut().set_scrollback(back);
        assert_eq!(q.screen().cell(0, 0).unwrap().fgcolor(), vt100::Color::Idx(1), "colours kept");

        // A note goes last, and history from the alternate screen isn't taken.
        let mut r = vt100::Parser::new_with_callbacks(nz(5), nz(20), 100, Term::default());
        replay_history(&mut r, &dump, Some("restored"));
        assert!(history_text(&mut r).trim_end().ends_with("── restored ──"));
        p.process(b"\x1b[?1049hfullscreen app");
        assert!(!history_text(&mut p).contains("fullscreen app"));
    }

    #[test]
    fn answers_terminal_queries() {
        assert_eq!(term(b"\x1b[3;7H\x1b[6n"), b"\x1b[3;7R");
        assert_eq!(term(b"\x1b[5n"), b"\x1b[0n");
        assert_eq!(term(b"\x1b[c"), b"\x1b[?1;2c");
        assert_eq!(term(b"\x1b[>c"), b"\x1b[>84;0;0c");
        assert_eq!(term(b"\x1b]11;?\x07"), b"\x1b]11;rgb:0000/0000/0000\x1b\\");
        assert_eq!(term(b"plain text\x1b[2J"), b"");
        let mut p = vt100::Parser::new_with_callbacks(nz(10), nz(40), 0, Term::default());
        p.process(b"\x1b]0;\xe2\x9c\xb3 Claude Code\x07");
        assert_eq!(p.callbacks().title, "✳ Claude Code");
    }
}
