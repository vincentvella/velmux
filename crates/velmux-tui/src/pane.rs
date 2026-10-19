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

/// Captures OSC window titles, which agents use to report progress.
#[derive(Default)]
pub struct Title(pub String);

impl vt100::Callbacks for Title {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.0 = String::from_utf8_lossy(title).into_owned();
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
    pub parser: vt100::Parser<Title>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// Agent currently in the foreground (re-detected on each tick).
    pub agent: Option<&'static str>,
    pub state: Option<AgentState>,
    idle_streak: u8,
    /// Agent finished while the pane wasn't focused.
    pub unseen_done: bool,
    pub last_output: Instant,
    /// Claude session this pane runs, when the agent is Claude Code.
    pub session: Option<String>,
    pub started_at: SystemTime,
}

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

        let mut reader = pair.master.try_clone_reader().map_err(|e| anyhow!("reader: {e}"))?;
        let writer = pair.master.take_writer().map_err(|e| anyhow!("writer: {e}"))?;
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
            parser: vt100::Parser::new_with_callbacks(nz(rows), nz(cols), 5000, Title::default()),
            master: pair.master,
            writer,
            child,
            agent: None,
            state: None,
            idle_streak: 0,
            unseen_done: false,
            last_output: Instant::now(),
            session: None,
            started_at: SystemTime::now(),
        })
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        self.last_output = Instant::now();
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
            for k in [b'A', b'B', b'C', b'D'] {
                data = replace(&data, &[0x1b, b'[', k], &[0x1b, b'O', k]);
            }
        }
        let _ = self.writer.write_all(&data);
        let _ = self.writer.flush();
    }

    pub fn title(&self) -> &str {
        &self.parser.callbacks().0
    }

    /// Re-detect the foreground agent and its state. Returns true if anything changed.
    pub fn detect(&mut self, focused: bool) -> bool {
        let agent = detect::foreground_agent(self.master.process_group_leader());
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
        let changed = agent != self.agent || state != self.state;
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
