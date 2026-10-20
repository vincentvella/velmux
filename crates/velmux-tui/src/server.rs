use std::collections::HashMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use velmux_pool::{claude, tui_settings, ClaimOpts, Config, Env, Pool, SlotView, Status, TuiSettings};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use vt100::MouseProtocolMode;

use crate::copy::{self, Action as CopyAction, Copy};
use crate::herdr::{self, Plan, Target};
use crate::keys::{Action, Keymap};
use crate::picker::{self, Picker};
use crate::layout::{self, Dir, Layout, Toward};
use crate::mouse::{self, Chunk, MouseEvent};
use crate::pane::Term;
use crate::pane::{Kind, Launch, Loc, Pane};
use crate::prefix::Prefix;
use crate::proto::{ClientMsg, ServerMsg};
use crate::session::{self, Saved, SavedFolder, SavedPane, SavedTab, SavedView};
use crate::{paths, ui};


pub enum Event {
    /// A new connection and its first message (`Hello` attaches).
    Client(UnixStream, ClientMsg),
    /// A one-shot control connection.
    Control(UnixStream, ClientMsg),
    FromClient(u64, ClientMsg),
    ClientGone(u64),
    PtyOutput(u64, Vec<u8>),
    PtyExit(u64),
    Claimed { project: usize, result: Result<SlotView, String> },
    Released { project: usize, slot: String, result: Result<String, String> },
}

/// A git repo with its pool, or the "unclassified" group of plain folders.
pub struct Project {
    /// `None` for the unclassified group.
    pub pool: Option<Arc<Pool>>,
    /// The repo's config, or global defaults for unclassified folders.
    pub cfg: Config,
    pub slots: Vec<SlotView>,
    /// Unclassified only: its folders, addressed by id in `Loc::slot`.
    pub folders: Vec<Folder>,
}

pub struct Folder {
    pub id: String,
    pub path: PathBuf,
}

impl Project {
    fn repo(pool: Pool) -> Project {
        Project { cfg: pool.cfg.clone(), pool: Some(Arc::new(pool)), slots: Vec::new(), folders: Vec::new() }
    }

    pub fn name(&self) -> &str {
        self.pool.as_ref().map_or("unclassified", |p| p.name.as_str())
    }

    pub fn base(&self) -> Option<&Path> {
        self.pool.as_ref().map(|p| p.base.as_path())
    }

    pub fn is_unclassified(&self) -> bool {
        self.pool.is_none()
    }
}

pub enum Mode {
    Normal,
    Prefix,
    Prompt { label: String, input: String, purpose: PromptFor },
    /// Fuzzy location picker.
    Picker(Picker),
    /// The short menu, with the highlighted row.
    Menu { selected: usize },
    Confirm { label: String, action: Confirm },
    /// Scrolling/selecting in the focused pane's history (state in `App::copy`).
    Copy,
}

/// A mouse drag in progress.
enum Drag {
    /// Resizing the split behind a divider.
    Divider { loc: Loc, path: Vec<bool>, span: Rect, dir: Dir },
    /// Selecting text; copy mode opens on the first motion, copies on release.
    Select { pane: u64, origin: (u16, u16), moved: bool },
}

/// Overrides for a new pane (restore and import).
#[derive(Default)]
struct LaunchOpts {
    resume: Option<String>,
    cwd: Option<PathBuf>,
    command: Option<String>,
}

/// Bytes used by a key with no binding after the prefix (so its tail doesn't leak
/// into the pane): a whole escape sequence, ESC + key, or one UTF-8 character.
fn unbound_len(bytes: &[u8]) -> usize {
    match bytes {
        [0x1b, b'[', rest @ ..] => rest.iter().position(|b| (0x40..=0x7e).contains(b)).map_or(bytes.len(), |i| i + 3),
        [0x1b, _, ..] => 2,
        [b, ..] if *b >= 0xc0 => {
            let n = b.leading_ones() as usize;
            n.min(bytes.len())
        }
        _ => 1,
    }
}

/// Written by `settings` when there's no config yet.
const CONFIG_TEMPLATE: &str = r#"# velmux global config. Save and quit the editor to apply (settings reloads it).

# prefix = "C-b"              # TUI prefix: C-a, ctrl+space, ^], ...
# mouse = true                # click / drag / wheel in the TUI
# agent = "claude"            # command for agent panes
# editor = "nvim"             # default: $VISUAL / $EDITOR
# slots = 5                   # worktree slots per repo
# claude_history = true       # group slot transcripts under the base repo

# Rebind any action ("prefix+x", or prefix-less like "super+d"; [] unbinds).
# `velmux keys` lists them all; `velmux keys --ghostty` prints terminal config.
# [keys]
# split_right = ["prefix+v", "super+d"]
# location = "alt+1..9"
"#;

/// What a text prompt's answer is for.
pub enum PromptFor {
    ClaimBranch,
    RenameTab,
    RenameLocation,
}

pub enum Confirm {
    Release(Loc),
    Quit,
}

struct Client {
    conn: u64,
    stream: UnixStream,
}

/// One tab: a tiled set of panes.
pub struct Tab {
    pub layout: Layout,
    /// Show only this pane, full size.
    pub zoomed: Option<u64>,
    /// Pane to focus when switching back to this tab.
    pub last_focus: Option<u64>,
    pub name: Option<String>,
}

impl Tab {
    pub fn new(layout: Layout, zoomed: Option<u64>) -> Tab {
        Tab { layout, zoomed, last_focus: None, name: None }
    }
}

/// The tabs of one location (base, slot or folder). Derefs to the active tab, so
/// layout operations act on what's on screen.
pub struct View {
    pub tabs: Vec<Tab>,
    pub active: usize,
    /// Name given with rename_location (shown instead of base / slot id).
    pub name: Option<String>,
}

impl View {
    pub fn single(tab: Tab) -> View {
        View { tabs: vec![tab], active: 0, name: None }
    }

    pub fn tab_of(&self, pane: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.layout.contains(pane))
    }
}

impl std::ops::Deref for View {
    type Target = Tab;
    fn deref(&self) -> &Tab {
        &self.tabs[self.active]
    }
}

impl std::ops::DerefMut for View {
    fn deref_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }
}

pub struct App {
    pub projects: Vec<Project>,
    pub panes: Vec<Pane>,
    pub views: HashMap<Loc, View>,
    focus: Option<u64>,
    current: Option<Loc>,
    pub mode: Mode,
    pub prefix: Prefix,
    pub keymap: Keymap,
    pub sidebar_hidden: bool,
    pub sidebar_view: ui::SidebarView,
    /// Shown once a client attaches (e.g. an invalid `prefix` in config).
    startup_warning: Option<String>,
    pub jobs: Vec<String>,
    status: Option<(String, Instant)>,
    size: (u16, u16),
    events: Sender<Event>,
    client: Option<Client>,
    next_conn: u64,
    next_pane: u64,
    prev: Buffer,
    pub copy: Option<Copy>,
    /// Raw bytes for the client terminal itself (e.g. OSC 52 clipboard).
    pending_out: Vec<u8>,
    mouse: bool,
    /// Restart in place when the binary changes.
    auto_restart: bool,
    /// The binary this server runs, and its mtime when we last looked.
    exe: Option<(PathBuf, Option<SystemTime>)>,
    /// A newer binary seen once; restart when it's unchanged on the next look.
    exe_pending: Option<SystemTime>,
    /// Claude Code's config dir, for its per-process session records.
    claude_dir: PathBuf,
    /// The editor pane opened by `settings`; config reloads when it closes.
    settings_pane: Option<u64>,
    /// Unfinished mouse sequence from the previous read.
    input_carry: Vec<u8>,
    drag: Option<Drag>,
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
        for mut stream in listener.incoming().flatten() {
            // Read the first frame off the accept thread so control requests never
            // disturb the attached client.
            let tx = accept_tx.clone();
            std::thread::spawn(move || {
                let Ok(first) = ClientMsg::read(&mut stream) else { return };
                let ev = match first {
                    ClientMsg::ImportHerdr { .. } | ClientMsg::Restart => Event::Control(stream, first),
                    _ => Event::Client(stream, first),
                };
                let _ = tx.send(ev);
            });
        }
    });

    let (settings, mut warning) = match Env::from_process().and_then(|env| tui_settings(&env)) {
        Ok(s) => (s, None),
        Err(e) => (
            TuiSettings { prefix: None, mouse: true, auto_restart: true, keys: Default::default() },
            Some(format!("config: {e:#}")),
        ),
    };
    let prefix = match settings.prefix.as_deref().map(Prefix::parse) {
        None => Prefix::default(),
        Some(Ok(p)) => p,
        Some(Err(e)) => {
            warning = Some(format!("config: {e:#}; using ctrl+b"));
            Prefix::default()
        }
    };
    if let Some(w) = &warning {
        eprintln!("velmux: {w}");
    }
    let (keymap, key_warnings) = Keymap::build(prefix.byte, &settings.keys);
    if !key_warnings.is_empty() {
        let w = key_warnings.join("; ");
        eprintln!("velmux: {w}");
        warning = Some(match warning {
            Some(prev) => format!("{prev}; {w}"),
            None => w,
        });
    }
    let mut app = App::new(tx, prefix, keymap, settings.mouse, warning);
    app.auto_restart = settings.auto_restart;
    match std::env::var_os("VELMUX_HANDOFF") {
        Some(path) => {
            // Don't pass it on to panes we start later.
            std::env::remove_var("VELMUX_HANDOFF");
            app.adopt(Path::new(&path));
        }
        None => app.restore(),
    }
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
            app.check_binary();
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
    // Let each pane's SIGKILL fallback fire before the process (and its threads) exit.
    std::thread::sleep(Duration::from_millis(1700));
    Ok(())
}

impl App {
    fn new(events: Sender<Event>, prefix: Prefix, keymap: Keymap, mouse: bool, startup_warning: Option<String>) -> App {
        App {
            projects: Vec::new(),
            panes: Vec::new(),
            views: HashMap::new(),
            focus: None,
            current: None,
            mode: Mode::Normal,
            prefix,
            keymap,
            sidebar_hidden: false,
            sidebar_view: ui::SidebarView::Spaces,
            startup_warning,
            jobs: Vec::new(),
            status: None,
            size: (80, 24),
            events,
            client: None,
            next_conn: 0,
            next_pane: 1,
            prev: Buffer::empty(Rect::new(0, 0, 80, 24)),
            copy: None,
            pending_out: Vec::new(),
            mouse,
            auto_restart: true,
            exe: std::env::current_exe().ok().and_then(|p| p.canonicalize().ok()).map(|p| {
                let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
                (p, mtime)
            }),
            exe_pending: None,
            claude_dir: Env::from_process().map(|e| e.claude_dir).unwrap_or_default(),
            settings_pane: None,
            input_carry: Vec::new(),
            drag: None,
            full_redraw: true,
            dirty: true,
            quit: false,
        }
    }

    // ---- queries used by the UI ----

    pub fn focused(&self) -> Option<&Pane> {
        self.focus.and_then(|id| self.panes.iter().find(|p| p.id == id))
    }

    pub fn pane(&self, id: u64) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    /// Outer rect of every pane in `loc`'s view, plus dividers (none when zoomed).
    pub fn view_rects(&self, loc: &Loc) -> (Vec<(u64, Rect)>, Vec<layout::Divider>) {
        let Some(view) = self.views.get(loc) else { return (Vec::new(), Vec::new()) };
        let body = ui::pane_area(self.size.0, self.size.1, self.sidebar_hidden);
        match view.zoomed {
            Some(id) => (vec![(id, body)], Vec::new()),
            None => view.layout.rects(body),
        }
    }

    /// Whether panes in `loc` get a title row (only when more than one is visible).
    pub fn titled(&self, loc: &Loc) -> bool {
        self.views
            .get(loc)
            .is_some_and(|v| v.zoomed.is_none() && v.layout.leaves().len() > 1)
    }

    pub fn current_loc(&self) -> Option<Loc> {
        self.focused()
            .map(|p| p.loc.clone())
            .or_else(|| self.current.clone())
            .or_else(|| {
                let project = self.projects.iter().position(|p| !p.is_unclassified())?;
                Some(Loc { project, slot: None })
            })
    }

    /// The unclassified group, created on first use.
    fn unclassified(&mut self) -> usize {
        if let Some(i) = self.projects.iter().position(Project::is_unclassified) {
            return i;
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        // Global settings (agent, editor) apply; a broken config falls back to defaults.
        let cfg = Env::from_process().and_then(|env| Config::load(&env, &home)).unwrap_or_else(|_| Config::defaults());
        self.projects.push(Project { pool: None, cfg, slots: Vec::new(), folders: Vec::new() });
        self.projects.len() - 1
    }

    /// Add (or find) folder `path` in the unclassified group; returns its location.
    fn folder_loc(&mut self, wanted_id: &str, path: &Path) -> Loc {
        let project = self.unclassified();
        let folders = &mut self.projects[project].folders;
        if let Some(f) = folders.iter().find(|f| f.id == wanted_id && f.path == path) {
            return Loc { project, slot: Some(f.id.clone()) };
        }
        let mut id = wanted_id.to_string();
        let mut n = 2;
        while folders.iter().any(|f| f.id == id) {
            id = format!("{wanted_id}-{n}");
            n += 1;
        }
        folders.push(Folder { id: id.clone(), path: path.to_path_buf() });
        Loc { project, slot: Some(id) }
    }

    pub fn loc_name(&self, loc: &Loc) -> String {
        let project = &self.projects[loc.project];
        let name = self.views.get(loc).and_then(|v| v.name.as_deref());
        match (&loc.slot, name) {
            (None, Some(n)) => format!("{} › {n} (base)", project.name()),
            (None, None) => format!("{} › base", project.name()),
            (Some(id), _) if project.is_unclassified() => format!("unclassified › {}", name.unwrap_or(id)),
            (Some(id), name) => {
                let branch = project
                    .slots
                    .iter()
                    .find(|s| &s.id == id)
                    .and_then(|s| s.branch.clone())
                    .map(|b| format!(" · {b}"))
                    .unwrap_or_default();
                match name {
                    Some(n) => format!("{} › {n} ({id}{branch})", project.name()),
                    None => format!("{} › {id}{branch}", project.name()),
                }
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
            Event::Client(stream, first) => self.attach(stream, first),
            Event::Control(mut stream, msg) => {
                let reply = match msg {
                    ClientMsg::ImportHerdr { path, with_latest_claude } => self.import_herdr(&path, with_latest_claude),
                    ClientMsg::Restart => {
                        let _ = ServerMsg::Reply("restarting — panes keep running".into()).write(&mut stream);
                        drop(stream);
                        if let Err(e) = self.restart() {
                            self.say(format!("restart failed: {e:#}"));
                        }
                        return;
                    }
                    _ => "unsupported request".into(),
                };
                let _ = ServerMsg::Reply(reply).write(&mut stream);
            }
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
            Event::PtyExit(id) => {
                self.remove_pane(id);
                if self.settings_pane == Some(id) {
                    self.settings_pane = None;
                    self.reload_config();
                }
            }
            Event::Claimed { project, result } => {
                self.jobs.retain(|j| j != "claiming");
                self.refresh_slots();
                match result {
                    Ok(slot) => {
                        let loc = Loc { project, slot: Some(slot.id.clone()) };
                        self.say(format!("claimed {}", slot.id));
                        self.spawn(loc, Kind::Agent, None);
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

    fn attach(&mut self, stream: UnixStream, first: ClientMsg) {
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
        self.on_client(first);
    }

    fn say_bye(&mut self, reason: &str) {
        if let Some(mut old) = self.client.take() {
            let _ = ServerMsg::Bye(reason.into()).write(&mut old.stream);
        }
    }

    fn on_client(&mut self, msg: ClientMsg) {
        match msg {
            ClientMsg::Hello { cols, rows, cwd } => {
                if self.mouse {
                    self.pending_out.extend_from_slice(mouse::ENABLE);
                }
                self.resize(cols, rows);
                self.open_cwd(&cwd);
                if let Some(w) = self.startup_warning.take() {
                    self.say(w);
                }
            }
            ClientMsg::Resize { cols, rows } => self.resize(cols, rows),
            ClientMsg::Input(bytes) => self.input(&bytes),
            // Only valid as the first message of a control connection.
            ClientMsg::ImportHerdr { .. } | ClientMsg::Restart => {}
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.size = (cols.max(20), rows.max(5));
        self.relayout();
    }

    /// Size every pane to its rect in its location's layout.
    fn relayout(&mut self) {
        self.reveal_focus();
        let body = ui::pane_area(self.size.0, self.size.1, self.sidebar_hidden);
        // Every tab, not just the visible one, so switching tabs never shows a stale size.
        let mut sizes = Vec::new();
        for view in self.views.values() {
            for tab in &view.tabs {
                let titled = tab.zoomed.is_none() && tab.layout.leaves().len() > 1;
                let rects = match tab.zoomed {
                    Some(id) => vec![(id, body)],
                    None => tab.layout.rects(body).0,
                };
                sizes.extend(rects.into_iter().map(|(id, r)| (id, ui::content_rect(r, titled))));
            }
        }
        for (id, inner) in sizes {
            if let Some(pane) = self.panes.iter_mut().find(|p| p.id == id) {
                pane.resize(inner.width, inner.height);
            }
        }
        self.full_redraw = true;
        self.dirty = true;
        // Every structural change ends in a relayout, so this keeps the save current.
        self.save();
    }

    /// Make the focused pane's tab the visible one (focus can move by sidebar,
    /// attention jump, cycling…), and remember it as that tab's focus.
    fn reveal_focus(&mut self) {
        let Some(id) = self.focus else { return };
        let Some(loc) = self.pane(id).map(|p| p.loc.clone()) else { return };
        if let Some(view) = self.views.get_mut(&loc) {
            if let Some(t) = view.tab_of(id) {
                view.active = t;
                view.tabs[t].last_focus = Some(id);
            }
        }
    }

    fn save(&self) {
        if let Err(e) = session::save(&self.snapshot()) {
            eprintln!("velmux: saving session: {e:#}");
        }
    }

    fn snapshot(&self) -> Saved {
        let secs = |t: SystemTime| t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let views = self
            .views
            .iter()
            .map(|(loc, view)| {
                let tabs: Vec<SavedTab> = view
                    .tabs
                    .iter()
                    .map(|tab| SavedTab {
                        name: tab.name.clone(),
                        layout: tab.layout.clone(),
                        panes: tab
                            .layout
                            .leaves()
                            .into_iter()
                            .filter_map(|id| self.pane(id))
                            .map(|p| SavedPane {
                                id: p.id,
                                // A shell where you started Claude comes back as that Claude.
                                kind: if p.kind == Kind::Shell && p.session.is_some() { Kind::Agent } else { p.kind },
                                claude_session: p.session.clone(),
                                started_at: secs(p.started_at),
                                cwd: p.cwd_override.clone(),
                                command: p.command_override.clone(),
                            })
                            .collect(),
                    })
                    .collect();
                SavedView {
                    project: self.projects[loc.project].base().map(Path::to_path_buf).unwrap_or_default(),
                    slot: loc.slot.clone(),
                    folder: self.projects[loc.project].is_unclassified(),
                    layout: tabs[0].layout.clone(),
                    panes: tabs[0].panes.clone(),
                    active_tab: view.active,
                    name: view.name.clone(),
                    tabs,
                }
            })
            .collect();
        Saved {
            version: 1,
            projects: self.projects.iter().filter_map(|p| p.base().map(Path::to_path_buf)).collect(),
            folders: self
                .projects
                .iter()
                .flat_map(|p| &p.folders)
                .map(|f| SavedFolder { id: f.id.clone(), path: f.path.clone() })
                .collect(),
            views,
        }
    }

    /// Restart in place: exec the (possibly rebuilt) binary, handing it every
    /// pane's open pty so shells and agents keep running and screens survive.
    /// The attached client reconnects by itself.
    fn restart(&mut self) -> Result<()> {
        let exe = match &self.exe {
            Some((p, _)) => p.clone(),
            None => std::env::current_exe()?,
        };
        // Never exec a binary that can't even start: the panes would die with us.
        let ok = std::process::Command::new(&exe).arg("--version").output().is_ok_and(|o| o.status.success());
        if !ok {
            anyhow::bail!("{} doesn't run (still building?)", exe.display());
        }
        if self.copy.is_some() {
            self.exit_copy();
        }
        let saved = self.snapshot();
        let live: Vec<session::LivePane> = self.panes.iter_mut().filter_map(Pane::handoff).collect();
        let handoff = session::Handoff { saved, live, focus: self.focus };
        let path = session::handoff_file()?;
        std::fs::write(&path, serde_json::to_vec(&handoff)?)?;
        // Also save normally, so a failed hand-off still restores.
        self.save();
        self.say_bye("restarting");
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&exe).arg("__server").env("VELMUX_HANDOFF", &path).exec();
        // Only reached if exec failed: carry on as before.
        let _ = std::fs::remove_file(&path);
        Err(err.into())
    }

    /// Live-restart when the binary on disk changes (e.g. `cargo build`), once
    /// its mtime has held still between two looks so a half-written file is skipped.
    fn check_binary(&mut self) {
        if !self.auto_restart {
            return;
        }
        let Some((path, seen)) = &self.exe else { return };
        let now = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if now == *seen || now.is_none() {
            self.exe_pending = None;
            return;
        }
        if self.exe_pending != now {
            self.exe_pending = now;
            return;
        }
        // Remember it either way, so a binary that fails the check isn't retried forever.
        if let Some((_, seen)) = &mut self.exe {
            *seen = now;
        }
        self.exe_pending = None;
        if let Err(e) = self.restart() {
            self.say(format!("new velmux binary, but restart failed: {e:#}"));
        }
    }

    /// Pick up from a live restart: re-create projects and layouts from the
    /// hand-off, adopting each pane's still-open pty instead of starting anew.
    fn adopt(&mut self, path: &Path) {
        let handoff: Option<session::Handoff> =
            std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let _ = std::fs::remove_file(path);
        let Some(handoff) = handoff else { return self.restore() };
        self.restore_projects(&handoff.saved);
        let mut live: HashMap<u64, session::LivePane> = handoff.live.into_iter().map(|l| (l.id, l)).collect();
        for mut view in handoff.saved.views {
            let project = if view.folder {
                self.projects.iter().position(Project::is_unclassified)
            } else {
                self.projects.iter().position(|p| p.base() == Some(view.project.as_path()))
            };
            let Some(project) = project else { continue };
            let loc = Loc { project, slot: view.slot.clone() };
            let (active, name) = (view.active_tab, view.name.clone());
            let mut tabs = Vec::new();
            for saved_tab in view.take_tabs() {
                let mut layout = Some(saved_tab.layout);
                for p in saved_tab.panes {
                    let adopted = live.remove(&p.id).and_then(|l| {
                        Pane::adopt(p.id, loc.clone(), p.kind, &l, self.events.clone()).ok()
                    });
                    match adopted {
                        Some(mut pane) => {
                            pane.session = p.claude_session;
                            pane.started_at = UNIX_EPOCH + Duration::from_secs(p.started_at);
                            pane.cwd_override = p.cwd;
                            pane.command_override = p.command;
                            self.next_pane = self.next_pane.max(p.id + 1);
                            self.panes.push(pane);
                        }
                        None => layout = layout.and_then(|l| l.remove(p.id)),
                    }
                }
                if let Some(layout) = layout {
                    let mut tab = Tab::new(layout, None);
                    tab.name = saved_tab.name;
                    tabs.push(tab);
                }
            }
            if !tabs.is_empty() {
                let active = active.min(tabs.len() - 1);
                self.views.insert(loc, View { tabs, active, name });
            }
        }
        // Anything left over has nowhere to live: let it go.
        for l in live.into_values() {
            unsafe {
                libc::kill(l.pid as libc::pid_t, libc::SIGHUP);
                libc::close(l.fd);
            }
        }
        self.focus = handoff.focus.filter(|f| self.pane(*f).is_some());
        self.current = self.focused().map(|p| p.loc.clone());
        self.startup_warning = Some(format!("restarted in place · {} pane(s) kept running", self.panes.len()));
        self.relayout();
    }

    /// Bring back the projects, layouts and panes of the last server. Claude panes
    /// resume their conversation in the slot they ran in; slots released since are skipped.
    fn restore_projects(&mut self, saved: &Saved) {
        let env = Env::from_process().ok();
        for base in &saved.projects {
            let Some(pool) = env.clone().and_then(|e| Pool::discover(e, base).ok()) else { continue };
            if !self.projects.iter().any(|p| p.base() == Some(pool.base.as_path())) {
                self.projects.push(Project::repo(pool));
            }
        }
        for f in &saved.folders {
            if f.path.is_dir() {
                self.folder_loc(&f.id, &f.path);
            }
        }
        self.refresh_slots();
    }

    fn restore(&mut self) {
        let Some(saved) = session::load() else { return };
        let env = Env::from_process().ok();
        self.restore_projects(&saved);

        let mut taken: Vec<String> = Vec::new();
        let (mut panes, mut resumed) = (0, 0);
        for mut view in saved.views {
            let project = if view.folder {
                self.projects.iter().position(Project::is_unclassified)
            } else {
                self.projects.iter().position(|p| p.base() == Some(view.project.as_path()))
            };
            let Some(project) = project else { continue };
            let loc = Loc { project, slot: view.slot.clone() };
            if let (false, Some(id)) = (view.folder, &loc.slot) {
                let in_use = self.projects[project].slots.iter().any(|s| &s.id == id && s.status == Status::InUse);
                if !in_use {
                    continue;
                }
            }
            if self.loc_cwd(&loc).is_none() {
                continue;
            }
            let active = view.active_tab;
            let view_name = view.name.clone();
            let mut tabs = Vec::new();
            for saved_tab in view.take_tabs() {
            let mut layout = Some(saved_tab.layout);
            let mut ids = HashMap::new();
            for p in saved_tab.panes {
                // Exactly the recorded session (Claude resumes by id from any folder).
                // Never guess from "newest transcript here": other live Claudes share folders.
                let resume = match (&env, p.kind) {
                    (Some(env), Kind::Agent) if p.command.is_none() => p
                        .claude_session
                        .clone()
                        .filter(|id| claude::session_exists(&env.claude_dir, id) && !taken.contains(id)),
                    _ => None,
                };
                if let Some(id) = &resume {
                    taken.push(id.clone());
                }
                let resuming = resume.is_some();
                let opts = LaunchOpts { resume, cwd: p.cwd, command: p.command };
                match self.launch(&loc, p.kind, opts) {
                    Ok(new) => {
                        ids.insert(p.id, new);
                        panes += 1;
                        resumed += resuming as usize;
                    }
                    Err(_) => layout = layout.and_then(|l| l.remove(p.id)),
                }
            }
            if let Some(layout) = layout {
                let mut tab = Tab::new(layout.remap(&ids), None);
                tab.name = saved_tab.name;
                tabs.push(tab);
            }
            }
            if !tabs.is_empty() {
                let active = active.min(tabs.len() - 1);
                self.views.insert(loc, View { tabs, active, name: view_name });
            }
        }
        if panes > 0 && self.startup_warning.is_none() {
            self.startup_warning = Some(format!("restored {panes} pane(s), {resumed} Claude session(s) resumed"));
        }
        self.relayout();
    }

    /// Register the repo containing `cwd` and make it current.
    fn open_cwd(&mut self, cwd: &Path) {
        // A client reconnecting after a restart sends no cwd: keep where you were.
        if cwd.as_os_str().is_empty() {
            return;
        }
        let pool = match Env::from_process().and_then(|env| Pool::discover(env, cwd)) {
            Ok(pool) => pool,
            Err(_) => return,
        };
        let index = match self.projects.iter().position(|p| p.base() == Some(pool.base.as_path())) {
            Some(i) => i,
            None => {
                self.projects.push(Project::repo(pool));
                self.projects.len() - 1
            }
        };
        self.refresh_slots();
        let slot = self.projects[index].pool.as_ref().and_then(|p| p.slot_of(cwd).ok().flatten());
        let loc = Loc { project: index, slot };
        if self.focused().is_none_or(|p| p.loc != loc) {
            self.focus = self.views.get(&loc).and_then(|v| v.layout.leaves().first().copied());
        }
        self.current = Some(loc);
        self.save();
    }

    pub fn refresh_slots(&mut self) {
        for project in &mut self.projects {
            if let Some(Ok(slots)) = project.pool.as_ref().map(|p| p.status()) {
                project.slots = slots;
            }
        }
        self.dirty = true;
    }

    fn detect_all(&mut self) {
        let focus = self.focus;
        let mut changed = false;
        let mut sessions_changed = false;
        let claude_dir = self.claude_dir.clone();
        for pane in &mut self.panes {
            let before = pane.session.clone();
            changed |= pane.detect(focus == Some(pane.id), &claude_dir);
            sessions_changed |= pane.session != before;
        }
        if sessions_changed {
            self.save();
        }
        self.dirty |= changed || self.status.is_some();
    }

    // ---- input ----

    fn input(&mut self, bytes: &[u8]) {
        let mut data = std::mem::take(&mut self.input_carry);
        data.extend_from_slice(bytes);
        let (chunks, carry) = mouse::split(&data);
        let chunks: Vec<Result<MouseEvent, Vec<u8>>> = chunks
            .into_iter()
            .map(|c| match c {
                Chunk::Mouse(ev) => Ok(ev),
                Chunk::Bytes(b) => Err(b.to_vec()),
            })
            .collect();
        self.input_carry = carry;
        for chunk in chunks {
            match chunk {
                Ok(ev) => self.mouse_event(ev),
                Err(bytes) => self.keys(&bytes),
            }
        }
    }

    // ---- mouse ----

    fn mouse_event(&mut self, ev: MouseEvent) {
        self.dirty = true;
        if let Some(drag) = self.drag.take() {
            return self.continue_drag(drag, ev);
        }
        let (cols, rows) = self.size;
        let at = Position::new(ev.x, ev.y);
        if let Mode::Menu { .. } = self.mode {
            if ev.is_press() && ev.is_left() {
                let area = ui::menu_area(self, cols, rows);
                let item = (area.contains(at) && ev.y > area.y).then(|| (ev.y - area.y - 1) as usize);
                self.mode = Mode::Normal;
                if let Some(&(_, action)) = item.and_then(|i| ui::MENU.get(i)) {
                    self.run(action);
                }
            }
            return;
        }
        if !matches!(self.mode, Mode::Normal | Mode::Copy) {
            return;
        }
        if ev.x < ui::sidebar_width(cols, self.sidebar_hidden) {
            if ev.is_press() && ev.is_left() {
                self.sidebar_click(ev.x, ev.y);
            }
            return;
        }
        if ev.y == 0 || ev.y + 1 >= rows {
            return;
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
        if !inner.contains(at) {
            return; // title bar: focusing was all
        }
        let (lx, ly) = (ev.x - inner.x, ev.y - inner.y);
        let copying = self.copy.as_ref().is_some_and(|c| c.pane == id);
        let Some(pane) = self.panes.iter_mut().find(|p| p.id == id) else { return };
        let screen = pane.parser.screen();
        let app_mode = screen.mouse_protocol_mode();
        if !copying && app_mode != MouseProtocolMode::None {
            // The app handles the mouse itself (vim with mouse=a, fullscreen TUIs).
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
            self.drag = Some(Drag::Select { pane: id, origin: (lx, ly), moved: false });
        }
    }

    fn continue_drag(&mut self, drag: Drag, ev: MouseEvent) {
        match drag {
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
            Drag::Select { pane, origin, mut moved } => {
                if ev.release {
                    if moved {
                        self.yank_selection();
                    }
                    return;
                }
                if ev.is_motion() {
                    let Some(inner) = self.pane_inner(pane) else { return };
                    // Dragging past the top or bottom edge scrolls.
                    let edge = if ev.y < inner.y { -1 } else if ev.y >= inner.bottom() { 1 } else { 0 };
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
                }
                self.drag = Some(Drag::Select { pane, origin, moved });
            }
        }
    }

    fn pane_inner(&self, id: u64) -> Option<Rect> {
        let loc = self.pane(id)?.loc.clone();
        let titled = self.titled(&loc);
        let (rects, _) = self.view_rects(&loc);
        rects.into_iter().find(|(p, _)| *p == id).map(|(_, r)| ui::content_rect(r, titled))
    }

    fn sidebar_click(&mut self, x: u16, y: u16) {
        let Some((_, row)) = ui::sidebar_rows(self, self.size.1).into_iter().find(|(ry, _)| *ry == y) else {
            return;
        };
        let side = ui::sidebar_width(self.size.0, self.sidebar_hidden);
        match row {
            ui::Row::TopBar => {
                return self.run(if x < side / 2 { Action::ClaimSlot } else { Action::Menu });
            }
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
            ui::Row::Heading(_) | ui::Row::Divider | ui::Row::Blank => return,
            ui::Row::TopBar | ui::Row::ViewTabs | ui::Row::Agent { .. } => unreachable!("handled above"),
            ui::Row::Location(loc) => loc,
            ui::Row::Project(p) => {
                // Open the project at its first location with panes, else its root.
                let live = self.live_locations().into_iter().find(|l| l.project == p);
                let first = self.projects[p].folders.first().map(|f| f.id.clone());
                live.unwrap_or(Loc { project: p, slot: if self.projects[p].is_unclassified() { first } else { None } })
            }
        };
        self.focus = self.views.get(&loc).and_then(|v| v.layout.leaves().first().copied());
        self.current = Some(loc);
        self.full_redraw = true;
    }

    fn with_copy(&mut self, f: impl FnOnce(&mut Copy, &mut vt100::Parser<Term>)) {
        let Some(id) = self.copy.as_ref().map(|c| c.pane) else { return };
        if let (Some(c), Some(pane)) = (self.copy.as_mut(), self.panes.iter_mut().find(|p| p.id == id)) {
            f(c, &mut pane.parser);
        }
    }

    fn copy_scroll(&mut self, lines: isize) {
        self.with_copy(|c, p| c.scroll(lines, p));
        let back_at_bottom = self.copy.as_ref().is_some_and(|c| c.from_mouse && c.offset == 0 && c.anchor.is_none());
        if back_at_bottom {
            self.exit_copy();
        }
    }

    fn yank_selection(&mut self) {
        let mut text = None;
        self.with_copy(|c, p| text = c.selected_text(p));
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            let lines = text.lines().count().max(1);
            self.set_clipboard(&text);
            self.say(format!("copied {lines} line(s)"));
        }
        self.exit_copy();
    }

    fn keys(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let consumed = match self.mode {
                Mode::Normal => {
                    // Whichever comes first: the prefix key or a root (prefix-less) binding.
                    let prefix_at = bytes.iter().position(|&b| b == self.prefix.byte);
                    let root = self.keymap.find_root(bytes).filter(|(at, ..)| prefix_at.is_none_or(|p| *at < p));
                    match (root, prefix_at) {
                        (Some((at, action, len)), _) => {
                            self.send_to_pane(&bytes[..at]);
                            self.run(action);
                            at + len
                        }
                        (None, Some(at)) => {
                            self.send_to_pane(&bytes[..at]);
                            self.mode = Mode::Prefix;
                            at + 1
                        }
                        (None, None) => {
                            self.send_to_pane(bytes);
                            bytes.len()
                        }
                    }
                }
                Mode::Prefix => {
                    self.mode = Mode::Normal;
                    match self.keymap.after_prefix(bytes) {
                        Some((action, len)) => {
                            self.run(action);
                            len
                        }
                        None => unbound_len(bytes),
                    }
                }
                Mode::Prompt { .. } => self.prompt_input(bytes),
                Mode::Picker(_) => self.picker_input(bytes),
                Mode::Menu { .. } => self.menu_input(bytes),
                Mode::Copy => self.copy_input(bytes),
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
        } else if let Some(answer) = submit {
            let Mode::Prompt { purpose, .. } = std::mem::replace(&mut self.mode, Mode::Normal) else { unreachable!() };
            match purpose {
                PromptFor::ClaimBranch => self.claim((!answer.is_empty()).then_some(answer)),
                PromptFor::RenameTab => {
                    if let Some(view) = self.current_loc().and_then(|l| self.views.get_mut(&l)) {
                        // `View` derefs to its active tab: this names the tab.
                        let tab: &mut Tab = view;
                        tab.name = (!answer.is_empty()).then_some(answer);
                    }
                    self.relayout();
                }
                PromptFor::RenameLocation => {
                    if let Some(view) = self.current_loc().and_then(|l| self.views.get_mut(&l)) {
                        view.name = (!answer.is_empty()).then_some(answer);
                    }
                    self.relayout();
                }
            }
        }
        bytes.len()
    }

    /// Every place to go, in sidebar order: repo bases, claimed or open slots, folders.
    pub fn picker_items(&self) -> Vec<picker::Item> {
        ui::rows(self)
            .into_iter()
            .filter_map(|r| match r {
                ui::Row::Location(loc) => Some(loc),
                _ => None,
            })
            .filter(|loc| {
                let project = &self.projects[loc.project];
                let free = loc.slot.as_ref().is_some_and(|id| {
                    !project.is_unclassified()
                        && project.slots.iter().any(|s| &s.id == id && s.status == Status::Free)
                });
                !free || self.views.contains_key(loc)
            })
            .map(|loc| picker::Item { label: self.loc_name(&loc), loc })
            .collect()
    }

    fn menu_input(&mut self, bytes: &[u8]) -> usize {
        let Mode::Menu { selected } = &mut self.mode else { return bytes.len() };
        let n = ui::MENU.len();
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
                let action = ui::MENU[*selected].1;
                self.mode = Mode::Normal;
                self.run(action);
            }
            0x1b | 0x03 | b'q' => self.mode = Mode::Normal,
            _ => {}
        }
        used
    }

    fn picker_input(&mut self, bytes: &[u8]) -> usize {
        let items = self.picker_items();
        let Mode::Picker(p) = &mut self.mode else { return bytes.len() };
        let count = picker::filter(&items, &p.query).len();
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
                // Printable text up to the next control byte.
                let end = bytes.iter().position(|b| b.is_ascii_control()).unwrap_or(bytes.len());
                p.query.push_str(&String::from_utf8_lossy(&bytes[..end]));
                p.selected = 0;
                return end;
            }
        };
        match key {
            Some(0x10) => p.selected = p.selected.checked_sub(1).unwrap_or(count.saturating_sub(1)),
            Some(0x0e) => p.selected = if count == 0 { 0 } else { (p.selected + 1) % count },
            Some(0x7f | 0x08) => {
                p.query.pop();
                p.selected = 0;
            }
            Some(0x1b | 0x03) => self.mode = Mode::Normal,
            Some(b'\r' | b'\n') => {
                let chosen = picker::filter(&items, &p.query).get(p.selected).map(|i| i.loc.clone());
                self.mode = Mode::Normal;
                if let Some(loc) = chosen {
                    self.show_location(loc);
                }
            }
            _ => {}
        }
        used
    }

    fn run(&mut self, action: Action) {
        match action {
            Action::SendPrefix => {
                let b = self.prefix.byte;
                self.send_to_pane(&[b]);
            }
            Action::ClaimSlot => {
                if self.current_loc().is_none() {
                    self.say("not in a git repo — run velmux from inside one");
                } else {
                    self.mode = Mode::Prompt {
                        label: "branch for new slot (empty = detached)".into(),
                        input: String::new(),
                        purpose: PromptFor::ClaimBranch,
                    };
                }
            }
            Action::AgentBase => {
                if let Some(loc) = self.current_loc() {
                    // Claude in the base checkout; unclassified folders have no base.
                    let loc = if self.projects[loc.project].is_unclassified() { loc } else { Loc { slot: None, ..loc } };
                    self.spawn(loc, Kind::Agent, None);
                }
            }
            Action::NewShell => self.spawn_here(Kind::Shell, None),
            Action::NewEditor => self.spawn_here(Kind::Editor, None),
            Action::SplitRight => self.spawn_here(Kind::Shell, Some(Dir::Right)),
            Action::SplitDown => self.spawn_here(Kind::Shell, Some(Dir::Down)),
            Action::NextPane => self.cycle(1),
            Action::PrevPane => self.cycle(-1),
            Action::FocusUp => self.move_focus(Toward::Up),
            Action::FocusDown => self.move_focus(Toward::Down),
            Action::FocusLeft => self.move_focus(Toward::Left),
            Action::FocusRight => self.move_focus(Toward::Right),
            Action::ResizeLeft => self.nudge(Dir::Right, -0.05),
            Action::ResizeRight => self.nudge(Dir::Right, 0.05),
            Action::ResizeUp => self.nudge(Dir::Down, -0.05),
            Action::ResizeDown => self.nudge(Dir::Down, 0.05),
            Action::Zoom => self.toggle_zoom(),
            Action::CopyMode => self.enter_copy(false),
            Action::CopyModePageUp => self.enter_copy(true),
            Action::ClosePane => {
                if let Some(pane) = self.focus.and_then(|id| self.panes.iter_mut().find(|p| p.id == id)) {
                    pane.kill();
                }
            }
            Action::ReleaseSlot => match self.current_loc() {
                Some(loc) if loc.slot.is_some() && !self.projects[loc.project].is_unclassified() => {
                    let label = format!(
                        "release {}? its panes close; uncommitted work is stashed",
                        self.loc_name(&loc)
                    );
                    self.mode = Mode::Confirm { label, action: Confirm::Release(loc) };
                }
                Some(loc) if self.projects[loc.project].is_unclassified() => {
                    self.say("unclassified folders have no slots to release")
                }
                _ => self.say("focus a slot to release it (the base checkout is never released)"),
            },
            Action::Detach => self.say_bye("detached — run `velmux` to reattach"),
            Action::Quit => {
                self.mode = Mode::Confirm { label: "stop the server? panes and Claude sessions are restored next time you run velmux".into(), action: Confirm::Quit }
            }
            Action::Help => {
                let help = self.keymap.help(&self.prefix.short);
                self.say(help);
            }
            Action::Slot(n) => self.jump_in_space(n as usize),
            Action::Base => match self.current_loc() {
                Some(loc) if !self.projects[loc.project].is_unclassified() => {
                    self.show_location(Loc { project: loc.project, slot: None })
                }
                Some(_) => self.say("unclassified folders have no base — ⌥1–9 picks a folder"),
                None => {}
            },
            Action::Location(n) => self.goto_location(n as usize - 1),
            Action::PrevLocation => self.step_location(-1),
            Action::NextLocation => self.step_location(1),
            Action::ToggleSidebar => {
                self.sidebar_hidden = !self.sidebar_hidden;
                self.relayout();
            }
            Action::Attention => self.next_attention(),
            Action::LocationPicker => {
                if self.copy.is_some() {
                    self.exit_copy();
                }
                // Start on the current location so Enter alone is a no-op.
                let items = self.picker_items();
                let at = self.current_loc().and_then(|c| items.iter().position(|i| i.loc == c)).unwrap_or(0);
                self.mode = Mode::Picker(Picker { query: String::new(), selected: at });
            }
            Action::RenameTab => match self.current_loc().and_then(|l| self.views.get(&l)) {
                Some(view) => {
                    let tab: &Tab = view;
                    self.mode = Mode::Prompt {
                        label: format!("rename tab {} (empty clears)", view.active + 1),
                        input: tab.name.clone().unwrap_or_default(),
                        purpose: PromptFor::RenameTab,
                    }
                }
                None => self.say("no tab here yet — open a pane first"),
            },
            Action::RenameLocation => match self.current_loc() {
                Some(loc) if self.views.contains_key(&loc) => {
                    let current = self.views[&loc].name.clone().unwrap_or_default();
                    self.mode = Mode::Prompt {
                        label: format!("name {} (empty clears)", self.loc_name(&loc)),
                        input: current,
                        purpose: PromptFor::RenameLocation,
                    }
                }
                _ => self.say("open a pane here first, then name it"),
            },
            Action::Settings => self.open_settings(),
            Action::ReloadConfig => self.reload_config(),
            Action::Menu => self.mode = Mode::Menu { selected: 0 },
            Action::SidebarView => {
                self.sidebar_view = match self.sidebar_view {
                    ui::SidebarView::Spaces => ui::SidebarView::Agents,
                    ui::SidebarView::Agents => ui::SidebarView::Spaces,
                };
                self.full_redraw = true;
            }
            Action::RestartServer => {
                if let Err(e) = self.restart() {
                    self.say(format!("restart failed: {e:#}"));
                }
            }
            Action::NewTab => self.new_tab(),
            Action::NextTab => self.step_tab(1),
            Action::PrevTab => self.step_tab(-1),
            Action::Tab(n) => self.goto_tab(n as usize - 1),
        }
    }

    /// Open the global config in the editor, in a new "settings" tab here
    /// (creating it from a template first). Closing the editor reloads it.
    fn open_settings(&mut self) {
        let Some(loc) = self.current_loc() else {
            return self.say("run velmux from inside a repo to open settings");
        };
        let path = match Env::from_process() {
            Ok(env) => env.global_config,
            Err(e) => return self.say(format!("settings: {e:#}")),
        };
        if !path.exists() {
            let created = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(&path, CONFIG_TEMPLATE));
            if let Err(e) = created {
                return self.say(format!("settings: can't create {}: {e}", path.display()));
            }
        }
        let editor = self.projects[loc.project]
            .cfg
            .editor
            .clone()
            .or_else(|| std::env::var("VISUAL").ok())
            .or_else(|| std::env::var("EDITOR").ok())
            .unwrap_or_else(|| "vi".into());
        let quoted = format!("'{}'", path.display().to_string().replace('\'', r"'\''"));
        // Plain exec: the pane closes with the editor, which triggers the reload.
        let opts = LaunchOpts { command: Some(format!("exec {editor} {quoted}")), ..LaunchOpts::default() };
        match self.launch(&loc, Kind::Editor, opts) {
            Ok(id) => {
                let mut tab = Tab::new(Layout::Leaf(id), None);
                tab.name = Some("settings".into());
                match self.views.get_mut(&loc) {
                    Some(view) => {
                        view.tabs.push(tab);
                        view.active = view.tabs.len() - 1;
                    }
                    None => {
                        self.views.insert(loc.clone(), View::single(tab));
                    }
                }
                self.settings_pane = Some(id);
                self.focus = Some(id);
                self.current = Some(loc);
                self.relayout();
                self.say(format!("editing {} — save and quit to apply", path.display()));
            }
            Err(e) => self.say(e),
        }
    }

    /// Re-read the global and per-repo config: prefix, keys, mouse, agent,
    /// editor, slots. No restart needed.
    fn reload_config(&mut self) {
        let env = match Env::from_process() {
            Ok(env) => env,
            Err(e) => return self.say(format!("reload failed: {e:#}")),
        };
        let settings = match tui_settings(&env) {
            Ok(s) => s,
            Err(e) => return self.say(format!("config error, kept the old settings: {e:#}")),
        };
        let mut warnings = Vec::new();
        self.prefix = match settings.prefix.as_deref().map(Prefix::parse) {
            None => Prefix::default(),
            Some(Ok(p)) => p,
            Some(Err(e)) => {
                warnings.push(format!("{e:#}; using ctrl+b"));
                Prefix::default()
            }
        };
        let (keymap, key_warnings) = Keymap::build(self.prefix.byte, &settings.keys);
        self.keymap = keymap;
        warnings.extend(key_warnings);
        if settings.mouse != self.mouse {
            self.mouse = settings.mouse;
            self.pending_out.extend_from_slice(if self.mouse { mouse::ENABLE } else { mouse::DISABLE });
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        for project in &mut self.projects {
            match &project.pool {
                Some(pool) => match Pool::discover(env.clone(), &pool.base) {
                    Ok(fresh) => {
                        project.cfg = fresh.cfg.clone();
                        project.pool = Some(Arc::new(fresh));
                    }
                    Err(e) => warnings.push(format!("{}: {e:#}", project.name())),
                },
                None => project.cfg = Config::load(&env, &home).unwrap_or_else(|_| Config::defaults()),
            }
        }
        self.refresh_slots();
        self.full_redraw = true;
        if warnings.is_empty() {
            self.say(format!("config reloaded · prefix {}", self.prefix.label));
        } else {
            self.say(format!("config reloaded with warnings: {}", warnings.join("; ")));
        }
    }

    /// A new tab in the current location, starting with a shell.
    fn new_tab(&mut self) {
        let Some(loc) = self.current_loc() else {
            return self.say("not in a git repo — run velmux from inside one");
        };
        match self.launch(&loc, Kind::Shell, LaunchOpts::default()) {
            Ok(id) => {
                let tab = Tab::new(Layout::Leaf(id), None);
                match self.views.get_mut(&loc) {
                    Some(view) => {
                        view.tabs.push(tab);
                        view.active = view.tabs.len() - 1;
                    }
                    None => {
                        self.views.insert(loc.clone(), View::single(tab));
                    }
                }
                self.focus = Some(id);
                self.current = Some(loc);
                self.relayout();
            }
            Err(e) => self.say(e),
        }
    }

    fn goto_tab(&mut self, index: usize) {
        let Some(loc) = self.current_loc() else { return };
        let Some(view) = self.views.get_mut(&loc) else { return };
        if index >= view.tabs.len() {
            let n = view.tabs.len();
            return self.say(format!("no tab {} ({n} open)", index + 1));
        }
        view.active = index;
        let tab = &view.tabs[index];
        self.focus = tab.last_focus.filter(|f| tab.layout.contains(*f)).or_else(|| tab.layout.leaves().first().copied());
        if self.copy.is_some() {
            self.exit_copy();
        }
        self.full_redraw = true;
    }

    fn step_tab(&mut self, step: isize) {
        let Some(loc) = self.current_loc() else { return };
        let Some(view) = self.views.get(&loc) else { return };
        let n = view.tabs.len() as isize;
        let next = (view.active as isize + step).rem_euclid(n) as usize;
        self.goto_tab(next);
    }

    /// Locations that have panes, in sidebar order (what "location N" counts).
    fn live_locations(&self) -> Vec<Loc> {
        ui::rows(self)
            .into_iter()
            .filter_map(|r| match r {
                ui::Row::Location(loc) if self.views.contains_key(&loc) => Some(loc),
                _ => None,
            })
            .collect()
    }

    fn show_location(&mut self, loc: Loc) {
        if self.copy.is_some() {
            self.exit_copy();
        }
        self.focus = self.views.get(&loc).and_then(|v| v.last_focus.or_else(|| v.layout.leaves().first().copied()));
        self.current = Some(loc);
        self.full_redraw = true;
    }

    fn goto_location(&mut self, index: usize) {
        match self.live_locations().into_iter().nth(index) {
            Some(loc) => self.show_location(loc),
            None => self.say(format!("no location {} (only locations with panes count)", index + 1)),
        }
    }

    fn step_location(&mut self, step: isize) {
        let locs = self.live_locations();
        if locs.is_empty() {
            return;
        }
        let at = self.current_loc().and_then(|c| locs.iter().position(|l| *l == c));
        let next = match at {
            Some(i) => (i as isize + step).rem_euclid(locs.len() as isize) as usize,
            None => 0,
        };
        self.show_location(locs[next].clone());
    }

    /// Focus the next agent (after the current pane, wrapping) that's blocked on you
    /// or finished while you weren't looking.
    fn next_attention(&mut self) {
        let order: Vec<usize> = ui::rows(self)
            .into_iter()
            .filter_map(|r| match r {
                ui::Row::Pane(i) => Some(i),
                _ => None,
            })
            .collect();
        let start = self.focus.and_then(|f| order.iter().position(|&i| self.panes[i].id == f)).map_or(0, |i| i + 1);
        let needs = |p: &Pane| p.unseen_done || p.state == Some(crate::detect::AgentState::Blocked);
        let hit = (0..order.len()).map(|k| order[(start + k) % order.len()]).find(|&i| needs(&self.panes[i]));
        match hit {
            Some(i) => {
                let (id, loc) = (self.panes[i].id, self.panes[i].loc.clone());
                if self.copy.is_some() {
                    self.exit_copy();
                }
                self.focus = Some(id);
                self.current = Some(loc);
                self.full_redraw = true;
            }
            None => self.say("nothing needs you right now"),
        }
    }

    fn confirm(&mut self, action: Confirm) {
        match action {
            Confirm::Quit => self.quit = true,
            Confirm::Release(loc) => {
                let Some(slot) = loc.slot.clone() else { return };
                let Some(pool) = self.projects[loc.project].pool.clone() else { return };
                for pane in self.panes.iter_mut().filter(|p| p.loc == loc) {
                    pane.kill();
                }
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
        let Some(pool) = self.projects[loc.project].pool.clone() else {
            return self.say("unclassified folders aren't git repos, so they have no slots");
        };
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

    fn spawn_here(&mut self, kind: Kind, dir: Option<Dir>) {
        if let Some(loc) = self.current_loc() {
            self.spawn(loc, kind, dir);
        } else {
            self.say("not in a git repo — run velmux from inside one");
        }
    }

    /// Open a pane in `loc`, splitting the focused pane there (`dir`, or along its longer side).
    fn spawn(&mut self, loc: Loc, kind: Kind, dir: Option<Dir>) {
        match self.launch(&loc, kind, LaunchOpts::default()) {
            Ok(id) => {
                self.insert_into_view(&loc, id, dir);
                self.focus = Some(id);
                self.current = Some(loc);
                self.relayout();
            }
            Err(e) => self.say(e),
        }
        self.dirty = true;
    }

    /// The folder a location opens in (base checkout, slot or plain folder).
    pub fn loc_cwd(&self, loc: &Loc) -> Option<PathBuf> {
        let project = self.projects.get(loc.project)?;
        match (&project.pool, &loc.slot) {
            (Some(pool), None) => Some(pool.base.clone()),
            (Some(_), Some(id)) => project.slots.iter().find(|s| &s.id == id).map(|s| s.path.clone()),
            (None, Some(id)) => project.folders.iter().find(|f| &f.id == id).map(|f| f.path.clone()),
            (None, None) => None,
        }
    }

    /// Start a pane process in `loc` (not yet placed in a view). Claude agents get a
    /// known session id, or resume `resume`.
    fn launch(&mut self, loc: &Loc, kind: Kind, opts: LaunchOpts) -> Result<u64, String> {
        let root = self.loc_cwd(loc).ok_or_else(|| format!("{} not found", self.loc_name(loc)))?;
        let cwd_override = opts.cwd.filter(|c| c.is_dir() && *c != root);
        let cwd = cwd_override.clone().unwrap_or(root);
        let resume = opts.resume;
        let project = &self.projects[loc.project];
        let cfg = &project.cfg;
        let mut session = None;
        let command = match kind {
            _ if opts.command.is_some() => opts.command.clone(),
            Kind::Agent if session::is_claude(&cfg.agent) => {
                let agent = &cfg.agent;
                Some(match resume {
                    Some(id) => {
                        session = Some(id.clone());
                        format!("{agent} --resume {id}")
                    }
                    None => {
                        let id = session::new_session_id();
                        session = Some(id.clone());
                        format!("{agent} --session-id {id}")
                    }
                })
            }
            Kind::Agent => Some(cfg.agent.clone()),
            Kind::Shell => None,
            Kind::Editor => {
                let editor = cfg
                    .editor
                    .clone()
                    .or_else(|| std::env::var("VISUAL").ok())
                    .or_else(|| std::env::var("EDITOR").ok())
                    .unwrap_or_else(|| "vi".into());
                Some(format!("{editor} ."))
            }
        };
        let path_var = std::env::var_os("PATH");
        let command = command.map(|c| session::resolve_program(&c, path_var.as_deref()));
        let mut env = Vec::new();
        if let Some(base) = project.base() {
            env.push(("VELMUX_BASE", base.to_string_lossy().into_owned()));
            if let Some(id) = &loc.slot {
                env.push(("VELMUX_SLOT", id.clone()));
            }
        }
        let body = ui::pane_area(self.size.0, self.size.1, self.sidebar_hidden);
        let id = self.next_pane;
        self.next_pane += 1;
        let launch = Launch { cwd: &cwd, command: command.as_deref(), env: &env };
        let mut pane = Pane::spawn(id, loc.clone(), kind, launch, (body.width, body.height), self.events.clone())
            .map_err(|e| format!("spawn failed: {e:#}"))?;
        pane.session = session;
        pane.cwd_override = cwd_override;
        pane.command_override = opts.command;
        self.panes.push(pane);
        Ok(id)
    }

    /// Import a herdr session: one view per mapped workspace, skipping locations
    /// velmux already has panes in. Returns a summary for the caller.
    fn import_herdr(&mut self, path: &Path, with_latest_claude: bool) -> String {
        let plan = match Env::from_process().and_then(|env| herdr::plan(path, &env, with_latest_claude)) {
            Ok(plan) => plan,
            Err(e) => return format!("import failed: {e:#}"),
        };
        let Plan { views, mut skipped } = plan;
        let (mut imported, mut panes, mut resumed) = (0, 0, 0);
        let mut first = None;
        for v in views {
            let loc = match &v.target {
                Target::Folder { id } => self.folder_loc(id, &v.root),
                Target::Repo { base, slot } => {
                    let project = match self.projects.iter().position(|p| p.base() == Some(base.as_path())) {
                        Some(i) => i,
                        None => match Env::from_process().and_then(|env| Pool::discover(env, base)) {
                            Ok(pool) => {
                                self.projects.push(Project::repo(pool));
                                self.refresh_slots();
                                self.projects.len() - 1
                            }
                            Err(e) => {
                                skipped.push((v.label, format!("{e:#}")));
                                continue;
                            }
                        },
                    };
                    Loc { project, slot: slot.clone() }
                }
            };
            if self.views.contains_key(&loc) {
                skipped.push((v.label, "velmux already has panes there".into()));
                continue;
            }
            let mut tabs = Vec::new();
            for plan_tab in v.tabs {
            let mut layout = Some(plan_tab.layout);
            let mut ids = HashMap::new();
            let resumes = plan_tab.panes.iter().filter(|p| p.resume.is_some()).count();
            for p in plan_tab.panes {
                let opts = LaunchOpts { resume: p.resume, cwd: p.cwd, command: p.command };
                match self.launch(&loc, p.kind, opts) {
                    Ok(new) => {
                        ids.insert(p.id, new);
                    }
                    Err(_) => layout = layout.and_then(|l| l.remove(p.id)),
                }
            }
            let Some(layout) = layout else { continue };
            let zoomed = plan_tab.zoomed.and_then(|z| ids.get(&z).copied());
            panes += ids.len();
            resumed += resumes;
            first.get_or_insert((loc.clone(), layout.leaves().first().and_then(|l| ids.get(l).copied())));
            let mut tab = Tab::new(layout.remap(&ids), zoomed);
            tab.name = plan_tab.name;
            tabs.push(tab);
            }
            if tabs.is_empty() {
                continue;
            }
            imported += 1;
            let active = v.active_tab.min(tabs.len() - 1);
            self.views.insert(loc, View { tabs, active, name: v.name });
        }
        if let (None, Some((loc, pane))) = (self.focus, first) {
            self.focus = pane;
            self.current = Some(loc);
        }
        self.relayout();
        let mut out = format!(
            "imported {imported} workspace(s), {panes} pane(s), {resumed} Claude resume(s) from {}\n",
            path.display()
        );
        for (label, why) in skipped {
            out.push_str(&format!("  - skipped {label}: {why}\n"));
        }
        out
    }

    fn insert_into_view(&mut self, loc: &Loc, id: u64, dir: Option<Dir>) {
        let (rects, _) = self.view_rects(loc);
        let Some(view) = self.views.get_mut(loc) else {
            self.views.insert(loc.clone(), View::single(Tab::new(Layout::Leaf(id), None)));
            return;
        };
        view.zoomed = None;
        let leaves = view.layout.leaves();
        let target = self
            .focus
            .filter(|f| leaves.contains(f))
            .unwrap_or_else(|| *leaves.last().unwrap());
        // Terminal cells are ~2:1, so compare width against twice the height.
        let dir = dir.unwrap_or_else(|| match rects.iter().find(|(p, _)| *p == target) {
            Some((_, r)) if r.width < r.height * 2 => Dir::Down,
            _ => Dir::Right,
        });
        view.layout.split(target, id, dir);
    }

    fn focused_view(&mut self) -> Option<(u64, &mut View)> {
        let id = self.focus?;
        let loc = self.pane(id)?.loc.clone();
        Some((id, self.views.get_mut(&loc)?))
    }

    fn move_focus(&mut self, toward: Toward) {
        let Some(loc) = self.focused().map(|p| p.loc.clone()) else { return };
        let (rects, _) = self.view_rects(&loc);
        if let Some(next) = self.focus.and_then(|f| layout::neighbor(&rects, f, toward)) {
            self.focus = Some(next);
        }
    }

    fn nudge(&mut self, dir: Dir, delta: f32) {
        if let Some((id, view)) = self.focused_view() {
            if view.layout.nudge(id, dir, delta) {
                self.relayout();
            }
        }
    }

    fn toggle_zoom(&mut self) {
        if let Some((id, view)) = self.focused_view() {
            view.zoomed = match view.zoomed {
                Some(_) => None,
                None if view.layout.leaves().len() > 1 => Some(id),
                None => None,
            };
            self.relayout();
        }
    }

    fn enter_copy(&mut self, page_up: bool) {
        let Some(id) = self.focus else { return };
        let Some(pane) = self.panes.iter_mut().find(|p| p.id == id) else { return };
        pane.freeze();
        let mut c = Copy::enter(id, &pane.parser);
        if page_up {
            c.key(&[0x02], &mut pane.parser);
        }
        c.sync(&mut pane.parser);
        self.copy = Some(c);
        self.mode = Mode::Copy;
    }

    fn copy_input(&mut self, bytes: &[u8]) -> usize {
        let Some(id) = self.copy.as_ref().map(|c| c.pane) else {
            self.mode = Mode::Normal;
            return bytes.len();
        };
        let (Some(c), Some(pane)) = (self.copy.as_mut(), self.panes.iter_mut().find(|p| p.id == id)) else {
            self.exit_copy();
            return bytes.len();
        };
        let (used, action) = c.key(bytes, &mut pane.parser);
        match action {
            CopyAction::Stay => {}
            CopyAction::Exit => self.exit_copy(),
            CopyAction::Yank(text) => {
                let lines = text.lines().count().max(1);
                self.set_clipboard(&text);
                self.exit_copy();
                self.say(format!("copied {lines} line(s)"));
            }
        }
        used.max(1)
    }

    fn exit_copy(&mut self) {
        if let Some(c) = self.copy.take() {
            if let Some(pane) = self.panes.iter_mut().find(|p| p.id == c.pane) {
                pane.thaw();
            }
        }
        self.mode = Mode::Normal;
        self.dirty = true;
    }

    /// OSC 52 to the client terminal (works over SSH), plus the local clipboard
    /// tool when there is one, since the server runs on the user's machine.
    fn set_clipboard(&mut self, text: &str) {
        self.pending_out.extend_from_slice(&copy::osc52(text));
        let text = text.to_string();
        std::thread::spawn(move || {
            let tools: &[&[&str]] = if cfg!(target_os = "macos") {
                &[&["pbcopy"]]
            } else {
                &[&["wl-copy"], &["xclip", "-selection", "clipboard"]]
            };
            for tool in tools {
                let child = std::process::Command::new(tool[0])
                    .args(&tool[1..])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                if let Ok(mut child) = child {
                    if let Some(mut stdin) = child.stdin.take() {
                        use std::io::Write;
                        let _ = stdin.write_all(text.as_bytes());
                    }
                    if child.wait().is_ok_and(|s| s.success()) {
                        return;
                    }
                }
            }
        });
    }

    fn remove_pane(&mut self, id: u64) {
        if self.copy.as_ref().is_some_and(|c| c.pane == id) {
            self.copy = None;
            self.mode = Mode::Normal;
        }
        let Some(i) = self.panes.iter().position(|p| p.id == id) else { return };
        let mut pane = self.panes.remove(i);
        pane.reap();
        if let Some(mut view) = self.views.remove(&pane.loc) {
            if let Some(t) = view.tab_of(id) {
                let tab = view.tabs.remove(t);
                if let Some(layout) = tab.layout.remove(id) {
                    let zoomed = tab.zoomed.filter(|z| *z != id);
                    let last_focus = tab.last_focus.filter(|f| *f != id);
                    view.tabs.insert(t, Tab { layout, zoomed, last_focus, name: tab.name });
                } else if view.active >= t && view.active > 0 {
                    view.active -= 1; // the tab closed; stay on its neighbour
                }
            }
            if !view.tabs.is_empty() {
                view.active = view.active.min(view.tabs.len() - 1);
                self.views.insert(pane.loc.clone(), view);
            }
        }
        if self.focus == Some(id) {
            // Prefer the tab's last focus, another pane in the same place, else nothing.
            self.focus = self.views.get(&pane.loc).and_then(|v| v.last_focus.or_else(|| v.layout.leaves().first().copied()));
            self.current = Some(pane.loc);
        }
        self.relayout();
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

    /// Swap within the current space: slot N of this repo, or folder N in the
    /// unclassified group.
    fn jump_in_space(&mut self, n: usize) {
        let Some(loc) = self.current_loc() else { return };
        let project = &self.projects[loc.project];
        if project.is_unclassified() {
            return match project.folders.get(n.wrapping_sub(1)).map(|f| f.id.clone()) {
                Some(id) => self.show_location(Loc { project: loc.project, slot: Some(id) }),
                None => self.say(format!("no folder {n} here")),
            };
        }
        let slot = format!("slot-{n}");
        let target = Loc { project: loc.project, slot: Some(slot.clone()) };
        let Some(status) = project.slots.iter().find(|s| s.id == slot).map(|s| s.status) else {
            return self.say(format!("no {slot} (this repo has {} slots)", project.slots.len()));
        };
        if status == Status::Free && !self.views.contains_key(&target) {
            return self.say(format!("{slot} is free — claim one with {} n (or ⌘N)", self.prefix.short));
        }
        self.show_location(target);
        if self.focus.is_none() {
            self.say(format!("{slot}: no panes — {} a/c/e to open one", self.prefix.short));
        }
    }

    // ---- output ----

    fn render(&mut self) {
        self.dirty = false;
        self.reveal_focus();
        if self.client.is_none() {
            return;
        }
        let area = Rect::new(0, 0, self.size.0, self.size.1);
        let mut buf = Buffer::empty(area);
        let cursor = ui::draw(self, &mut buf);
        let mut out: Vec<u8> = std::mem::take(&mut self.pending_out);
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
