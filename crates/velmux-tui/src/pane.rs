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
    pub env: &'a [(&'a str, String)],
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
    idle_streak: u8,
    /// Agent finished while the pane wasn't focused.
    pub unseen_done: bool,
    pub last_output: Instant,
    /// Claude session this pane runs, when the agent is Claude Code.
    pub session: Option<String>,
    pub started_at: SystemTime,
    /// Launch overrides, persisted so restore reproduces the pane.
    pub cwd_override: Option<std::path::PathBuf>,
    pub command_override: Option<String>,
    /// Output held back while copy mode reads the history (`None` = live).
    frozen: Option<Vec<u8>>,
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
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.arg("-l");
        if let Some(command) = launch.command {
            // Drop back into a shell when the command exits so the pane survives.
            cmd.arg("-c");
            cmd.arg(format!("{command}; exec \"$SHELL\" -l"));
        }
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
    pub fn adopt(id: u64, loc: Loc, kind: Kind, live: &crate::session::LivePane, events: Sender<Event>) -> Result<Pane> {
        let master: Box<dyn MasterPty + Send> = Box::new(crate::pty_raw::RawMaster::new(live.fd));
        let child: Box<dyn Child + Send + Sync> = Box::new(crate::pty_raw::RawChild::new(live.pid));
        let mut pane = Pane::assemble(id, loc, kind, master, child, (live.cols, live.rows), events)?;
        pane.parser.process(live.screen.as_bytes());
        pane.parser.callbacks_mut().title = live.title.clone();
        Ok(pane)
    }

    /// Keep this pane's pty open across `exec`, describing it for the next server.
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
            idle_streak: 0,
            unseen_done: false,
            last_output: Instant::now(),
            session: None,
            started_at: SystemTime::now(),
            cwd_override: None,
            command_override: None,
            frozen: None,
        })
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.last_output = Instant::now();
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
        let pgid = self.master.process_group_leader();
        let agent = detect::foreground_agent(pgid);
        let fg = detect::foreground(pgid);
        let fg_changed = fg != self.fg;
        let live = match (agent, fg.pid) {
            (Some("claude"), Some(pid)) => velmux_pool::claude::running_session(claude_dir, pid),
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
            Some("claude") => {
                let screen = self.parser.screen();
                let (_, cols) = screen.size();
                let lines: Vec<String> = screen.rows(0, cols.get()).collect();
                Some(detect::classify_claude(self.title(), &lines))
            }
            // Other agents: working while output is flowing.
            Some(_) if self.last_output.elapsed().as_millis() < 1500 => Some(AgentState::Working),
            Some(_) => Some(AgentState::Idle),
            None => None,
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
        self.state = state;
        changed
    }

    /// Terminate everything running in the pane, not just its shell: `sh -c 'claude; …'`
    /// keeps the agent in the shell's process group, and it outlives a lone SIGHUP to
    /// the shell (reparented to init). Signal the groups, then SIGKILL stragglers.
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
