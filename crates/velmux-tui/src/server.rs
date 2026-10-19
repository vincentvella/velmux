use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use velmux_pool::{ClaimOpts, Env, Pool, SlotView, Status};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::pane::{Kind, Launch, Loc, Pane};
use crate::proto::{ClientMsg, ServerMsg};
use crate::{paths, ui};

const PREFIX: u8 = 0x02; // ctrl+b

pub enum Event {
    Client(UnixStream),
    FromClient(u64, ClientMsg),
    ClientGone(u64),
    PtyOutput(u64, Vec<u8>),
    PtyExit(u64),
    Claimed { project: usize, result: Result<SlotView, String> },
    Released { project: usize, slot: String, result: Result<String, String> },
}

pub struct Project {
    pub pool: Arc<Pool>,
    pub slots: Vec<SlotView>,
}

pub enum Mode {
    Normal,
    Prefix,
    Prompt { label: String, input: String },
    Confirm { label: String, action: Confirm },
}

pub enum Confirm {
    Release(Loc),
    Quit,
}

struct Client {
    conn: u64,
    stream: UnixStream,
}

pub struct App {
    pub projects: Vec<Project>,
    pub panes: Vec<Pane>,
    focus: Option<u64>,
    current: Option<Loc>,
    pub mode: Mode,
    pub jobs: Vec<String>,
    status: Option<(String, Instant)>,
    size: (u16, u16),
    events: Sender<Event>,
    client: Option<Client>,
    next_conn: u64,
    next_pane: u64,
    prev: Buffer,
    full_redraw: bool,
    dirty: bool,
    quit: bool,
}

pub fn serve() -> Result<()> {
    let sock = paths::socket()?;
    if UnixStream::connect(&sock).is_ok() {
        anyhow::bail!("a velmux server is already running at {}", sock.display());
    }
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    let (tx, rx) = channel();
    let accept_tx = tx.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if accept_tx.send(Event::Client(stream)).is_err() {
                break;
            }
        }
    });

    let mut app = App::new(tx);
    let (mut last_detect, mut last_slots, mut last_render) =
        (Instant::now(), Instant::now(), Instant::now());
    loop {
        match rx.recv_timeout(Duration::from_millis(30)) {
            Ok(ev) => app.handle(ev),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        for ev in rx.try_iter().take(256) {
            app.handle(ev);
        }
        if last_detect.elapsed() >= Duration::from_millis(300) {
            app.detect_all();
            last_detect = Instant::now();
        }
        if last_slots.elapsed() >= Duration::from_secs(2) {
            app.refresh_slots();
            last_slots = Instant::now();
        }
        if app.quit {
            break;
        }
        if app.dirty && last_render.elapsed() >= Duration::from_millis(16) {
            app.render();
            last_render = Instant::now();
        }
    }
    for pane in &mut app.panes {
        pane.kill();
    }
    app.say_bye("server stopped");
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

impl App {
    fn new(events: Sender<Event>) -> App {
        App {
            projects: Vec::new(),
            panes: Vec::new(),
            focus: None,
            current: None,
            mode: Mode::Normal,
            jobs: Vec::new(),
            status: None,
            size: (80, 24),
            events,
            client: None,
            next_conn: 0,
            next_pane: 1,
            prev: Buffer::empty(Rect::new(0, 0, 80, 24)),
            full_redraw: true,
            dirty: true,
            quit: false,
        }
    }

    // ---- queries used by the UI ----

    pub fn focused(&self) -> Option<&Pane> {
        self.focus.and_then(|id| self.panes.iter().find(|p| p.id == id))
    }

    pub fn current_loc(&self) -> Option<Loc> {
        self.focused()
            .map(|p| p.loc.clone())
            .or_else(|| self.current.clone())
            .or_else(|| (!self.projects.is_empty()).then_some(Loc { project: 0, slot: None }))
    }

    pub fn loc_name(&self, loc: &Loc) -> String {
        let project = &self.projects[loc.project];
        match &loc.slot {
            None => format!("{} › base", project.pool.name),
            Some(id) => {
                let branch = project
                    .slots
                    .iter()
                    .find(|s| &s.id == id)
                    .and_then(|s| s.branch.clone())
                    .map(|b| format!(" · {b}"))
                    .unwrap_or_default();
                format!("{} › {id}{branch}", project.pool.name)
            }
        }
    }

    pub fn status_text(&self) -> Option<String> {
        self.status
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(8))
            .map(|(s, _)| s.clone())
    }

    fn say(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
        self.dirty = true;
    }

    // ---- events ----

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::Client(stream) => self.attach(stream),
            Event::FromClient(conn, msg) => {
                if self.client.as_ref().map(|c| c.conn) == Some(conn) {
                    self.on_client(msg);
                }
            }
            Event::ClientGone(conn) => {
                if self.client.as_ref().map(|c| c.conn) == Some(conn) {
                    self.client = None;
                }
            }
            Event::PtyOutput(id, bytes) => {
                if let Some(pane) = self.panes.iter_mut().find(|p| p.id == id) {
                    pane.feed(&bytes);
                    if self.focus == Some(id) {
                        self.dirty = true;
                    }
                }
            }
            Event::PtyExit(id) => self.remove_pane(id),
            Event::Claimed { project, result } => {
                self.jobs.retain(|j| j != "claiming");
                self.refresh_slots();
                match result {
                    Ok(slot) => {
                        let loc = Loc { project, slot: Some(slot.id.clone()) };
                        self.say(format!("claimed {}", slot.id));
                        self.spawn(loc, Kind::Agent);
                    }
                    Err(e) => self.say(format!("claim failed: {e}")),
                }
            }
            Event::Released { project, slot, result } => {
                self.jobs.retain(|j| j != &format!("releasing {slot}"));
                self.refresh_slots();
                match result {
                    Ok(msg) => self.say(msg),
                    Err(e) => self.say(format!("release failed: {e}")),
                }
                if self.current.as_ref().is_some_and(|l| l.project == project && l.slot.as_deref() == Some(&slot)) {
                    let base = Loc { project, slot: None };
                    if self.focused().is_none() {
                        self.focus = self.panes.iter().find(|p| p.loc == base).map(|p| p.id);
                    }
                    self.current = Some(base);
                }
            }
        }
    }

    fn attach(&mut self, stream: UnixStream) {
        self.say_bye("attached from another terminal");
        let conn = self.next_conn;
        self.next_conn += 1;
        let Ok(mut reader) = stream.try_clone() else { return };
        let tx = self.events.clone();
        std::thread::spawn(move || {
            while let Ok(msg) = ClientMsg::read(&mut reader) {
                if tx.send(Event::FromClient(conn, msg)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Event::ClientGone(conn));
        });
        self.client = Some(Client { conn, stream });
    }

    fn say_bye(&mut self, reason: &str) {
        if let Some(mut old) = self.client.take() {
            let _ = ServerMsg::Bye(reason.into()).write(&mut old.stream);
        }
    }

    fn on_client(&mut self, msg: ClientMsg) {
        match msg {
            ClientMsg::Hello { cols, rows, cwd } => {
                self.resize(cols, rows);
                self.open_cwd(&cwd);
            }
            ClientMsg::Resize { cols, rows } => self.resize(cols, rows),
            ClientMsg::Input(bytes) => self.input(&bytes),
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.size = (cols.max(20), rows.max(5));
        let body = ui::pane_area(self.size.0, self.size.1);
        for pane in &mut self.panes {
            pane.resize(body.width, body.height);
        }
        self.full_redraw = true;
        self.dirty = true;
    }

    /// Register the repo containing `cwd` and make it current.
    fn open_cwd(&mut self, cwd: &Path) {
        let pool = match Env::from_process().and_then(|env| Pool::discover(env, cwd)) {
            Ok(pool) => pool,
            Err(_) => return,
        };
        let index = match self.projects.iter().position(|p| p.pool.base == pool.base) {
            Some(i) => i,
            None => {
                self.projects.push(Project { pool: Arc::new(pool), slots: Vec::new() });
                self.projects.len() - 1
            }
        };
        self.refresh_slots();
        let slot = self.projects[index].pool.slot_of(cwd).ok().flatten();
        self.current = Some(Loc { project: index, slot });
        if self.focused().is_none_or(|p| p.loc.project != index) {
            self.focus = None;
        }
    }

    pub fn refresh_slots(&mut self) {
        for project in &mut self.projects {
            if let Ok(slots) = project.pool.status() {
                project.slots = slots;
            }
        }
        self.dirty = true;
    }

    fn detect_all(&mut self) {
        let focus = self.focus;
        let mut changed = false;
        for pane in &mut self.panes {
            changed |= pane.detect(focus == Some(pane.id));
        }
        self.dirty |= changed || self.status.is_some();
    }

    // ---- input ----

    fn input(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let consumed = match self.mode {
                Mode::Normal => match bytes.iter().position(|&b| b == PREFIX) {
                    Some(at) => {
                        self.send_to_pane(&bytes[..at]);
                        self.mode = Mode::Prefix;
                        at + 1
                    }
                    None => {
                        self.send_to_pane(bytes);
                        bytes.len()
                    }
                },
                Mode::Prefix => {
                    self.mode = Mode::Normal;
                    let (key, len) = match bytes {
                        [0x1b, b'[', b'A', ..] => (b'k', 3),
                        [0x1b, b'[', b'B', ..] => (b'j', 3),
                        [b, ..] => (*b, 1),
                        [] => unreachable!(),
                    };
                    self.command(key);
                    len
                }
                Mode::Prompt { .. } => self.prompt_input(bytes),
                Mode::Confirm { .. } => {
                    let Mode::Confirm { action, .. } = std::mem::replace(&mut self.mode, Mode::Normal)
                    else {
                        unreachable!()
                    };
                    if matches!(bytes[0], b'y' | b'Y') {
                        self.confirm(action);
                    }
                    1
                }
            };
            bytes = &bytes[consumed.min(bytes.len())..];
            self.dirty = true;
        }
    }

    fn send_to_pane(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Some(pane) = self.focus.and_then(|id| self.panes.iter_mut().find(|p| p.id == id)) {
            pane.write_input(bytes);
        }
    }

    fn prompt_input(&mut self, bytes: &[u8]) -> usize {
        let Mode::Prompt { input, .. } = &mut self.mode else { return bytes.len() };
        let text = String::from_utf8_lossy(bytes);
        let mut chars = text.chars().peekable();
        let mut submit = None;
        let mut cancel = false;
        while let Some(c) = chars.next() {
            match c {
                '\r' | '\n' => {
                    submit = Some(input.trim().to_string());
                    break;
                }
                '\x7f' | '\x08' => {
                    input.pop();
                }
                '\x03' => {
                    cancel = true;
                    break;
                }
                '\x1b' => {
                    if chars.peek() == Some(&'[') {
                        // Skip a CSI sequence (arrow keys etc).
                        for c in chars.by_ref() {
                            if c.is_ascii_alphabetic() || c == '~' {
                                break;
                            }
                        }
                    } else {
                        cancel = true;
                        break;
                    }
                }
                c if !c.is_control() => input.push(c),
                _ => {}
            }
        }
        if cancel {
            self.mode = Mode::Normal;
        } else if let Some(branch) = submit {
            self.mode = Mode::Normal;
            self.claim((!branch.is_empty()).then_some(branch));
        }
        bytes.len()
    }

    fn command(&mut self, key: u8) {
        match key {
            PREFIX => self.send_to_pane(&[PREFIX]),
            b'n' => {
                if self.current_loc().is_none() {
                    self.say("not in a git repo — run velmux from inside one");
                } else {
                    self.mode = Mode::Prompt {
                        label: "branch for new slot (empty = detached)".into(),
                        input: String::new(),
                    };
                }
            }
            b'a' => {
                if let Some(loc) = self.current_loc() {
                    self.spawn(Loc { project: loc.project, slot: None }, Kind::Agent);
                }
            }
            b'c' => self.spawn_here(Kind::Shell),
            b'e' => self.spawn_here(Kind::Editor),
            b'j' => self.cycle(1),
            b'k' => self.cycle(-1),
            b'x' => {
                if let Some(pane) = self.focus.and_then(|id| self.panes.iter_mut().find(|p| p.id == id)) {
                    pane.kill();
                }
            }
            b'r' => match self.current_loc() {
                Some(loc) if loc.slot.is_some() => {
                    let label = format!(
                        "release {}? its panes close; uncommitted work is stashed",
                        self.loc_name(&loc)
                    );
                    self.mode = Mode::Confirm { label, action: Confirm::Release(loc) };
                }
                _ => self.say("focus a slot to release it (the base checkout is never released)"),
            },
            b'd' => self.say_bye("detached — run `velmux` to reattach"),
            b'Q' => {
                self.mode = Mode::Confirm { label: "quit server and close every pane?".into(), action: Confirm::Quit }
            }
            b'?' => self.say(
                "^b then: n claim slot · a Claude@base · c shell · e editor · j/k panes · 1-9 slot · r release · x close · d detach · Q quit · ^b literal",
            ),
            b'1'..=b'9' => self.jump_to_slot(format!("slot-{}", key - b'0')),
            _ => {}
        }
    }

    fn confirm(&mut self, action: Confirm) {
        match action {
            Confirm::Quit => self.quit = true,
            Confirm::Release(loc) => {
                let Some(slot) = loc.slot.clone() else { return };
                for pane in self.panes.iter_mut().filter(|p| p.loc == loc) {
                    pane.kill();
                }
                let pool = self.projects[loc.project].pool.clone();
                let tx = self.events.clone();
                let project = loc.project;
                self.jobs.push(format!("releasing {slot}"));
                std::thread::spawn(move || {
                    // Give killed processes a moment to exit before git touches the tree.
                    std::thread::sleep(Duration::from_millis(300));
                    let result = pool
                        .release(&slot, false)
                        .map(|r| {
                            let mut msg = format!("released {}", r.id);
                            if let Some(s) = r.stashed {
                                msg.push_str(&format!(" · stashed: {s}"));
                            }
                            if let Some(b) = r.rescued {
                                msg.push_str(&format!(" · kept commits on {b}"));
                            }
                            msg
                        })
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Event::Released { project, slot, result });
                });
            }
        }
    }

    fn claim(&mut self, branch: Option<String>) {
        let Some(loc) = self.current_loc() else { return };
        let pool = self.projects[loc.project].pool.clone();
        let tx = self.events.clone();
        self.jobs.push("claiming".into());
        std::thread::spawn(move || {
            let quiet = |_: &str| {};
            let result = (|| {
                if pool.status()?.is_empty() {
                    pool.init(&quiet)?;
                }
                let opts = ClaimOpts { branch, by: Some("velmux".into()), fetch: true, install: true };
                pool.claim(&opts, &quiet)
            })()
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Claimed { project: loc.project, result });
        });
    }

    fn spawn_here(&mut self, kind: Kind) {
        if let Some(loc) = self.current_loc() {
            self.spawn(loc, kind);
        } else {
            self.say("not in a git repo — run velmux from inside one");
        }
    }

    fn spawn(&mut self, loc: Loc, kind: Kind) {
        let project = &self.projects[loc.project];
        let pool = &project.pool;
        let cwd = match &loc.slot {
            None => pool.base.clone(),
            Some(id) => match project.slots.iter().find(|s| &s.id == id) {
                Some(s) => s.path.clone(),
                None => return self.say(format!("{id} not found")),
            },
        };
        let command = match kind {
            Kind::Agent => Some(pool.cfg.agent.clone()),
            Kind::Shell => None,
            Kind::Editor => {
                let editor = pool
                    .cfg
                    .editor
                    .clone()
                    .or_else(|| std::env::var("VISUAL").ok())
                    .or_else(|| std::env::var("EDITOR").ok())
                    .unwrap_or_else(|| "vi".into());
                Some(format!("{editor} ."))
            }
        };
        let mut env = vec![("VELMUX_BASE", pool.base.to_string_lossy().into_owned())];
        if let Some(id) = &loc.slot {
            env.push(("VELMUX_SLOT", id.clone()));
        }
        let body = ui::pane_area(self.size.0, self.size.1);
        let id = self.next_pane;
        self.next_pane += 1;
        let launch = Launch { cwd: &cwd, command: command.as_deref(), env: &env };
        match Pane::spawn(id, loc.clone(), kind, launch, (body.width, body.height), self.events.clone()) {
            Ok(pane) => {
                self.panes.push(pane);
                self.focus = Some(id);
                self.current = Some(loc);
            }
            Err(e) => self.say(format!("spawn failed: {e:#}")),
        }
        self.dirty = true;
    }

    fn remove_pane(&mut self, id: u64) {
        let Some(i) = self.panes.iter().position(|p| p.id == id) else { return };
        let mut pane = self.panes.remove(i);
        pane.reap();
        if self.focus == Some(id) {
            // Prefer another pane in the same place, else nothing.
            self.focus = self.panes.iter().find(|p| p.loc == pane.loc).map(|p| p.id);
            self.current = Some(pane.loc);
        }
        self.full_redraw = true;
        self.dirty = true;
    }

    /// Move focus along the sidebar's pane order.
    fn cycle(&mut self, step: isize) {
        let order: Vec<u64> = ui::rows(self)
            .into_iter()
            .filter_map(|r| match r {
                ui::Row::Pane(i) => Some(self.panes[i].id),
                _ => None,
            })
            .collect();
        if order.is_empty() {
            return;
        }
        let at = self.focus.and_then(|f| order.iter().position(|&id| id == f));
        let next = match at {
            Some(i) => (i as isize + step).rem_euclid(order.len() as isize) as usize,
            None => 0,
        };
        self.focus = Some(order[next]);
        self.full_redraw = true;
    }

    fn jump_to_slot(&mut self, slot: String) {
        let Some(loc) = self.current_loc() else { return };
        let target = Loc { project: loc.project, slot: Some(slot.clone()) };
        if !self.projects[loc.project].slots.iter().any(|s| s.id == slot) {
            return self.say(format!("no {slot} (run `velmux init` or claim with ^b n)"));
        }
        self.focus = self.panes.iter().find(|p| p.loc == target).map(|p| p.id);
        let in_use = self.projects[loc.project]
            .slots
            .iter()
            .any(|s| s.id == slot && s.status == Status::InUse);
        if self.focus.is_none() && in_use {
            self.say(format!("{slot}: no panes — ^b a/c/e to open one"));
        }
        self.current = Some(target);
        self.full_redraw = true;
    }

    // ---- output ----

    fn render(&mut self) {
        self.dirty = false;
        if self.client.is_none() {
            return;
        }
        let area = Rect::new(0, 0, self.size.0, self.size.1);
        let mut buf = Buffer::empty(area);
        let cursor = ui::draw(self, &mut buf);
        let mut out: Vec<u8> = Vec::new();
        if self.full_redraw || self.prev.area != area {
            out.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[H");
            self.prev = Buffer::empty(area);
            self.full_redraw = false;
        }
        {
            let mut backend = CrosstermBackend::new(&mut out);
            let _ = backend.draw(self.prev.diff(&buf).into_iter());
            let _ = match cursor {
                Some(pos) => backend.set_cursor_position(pos).and_then(|_| backend.show_cursor()),
                None => backend.hide_cursor(),
            };
            let _ = Backend::flush(&mut backend);
        }
        self.prev = buf;
        if let Some(client) = self.client.as_mut() {
            if ServerMsg::Output(out).write(&mut client.stream).is_err() {
                self.client = None;
            }
        }
    }
}
