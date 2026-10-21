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


/// One project's slots and parked work, as a background refresh found them.
pub type PoolHealth = (usize, Option<Vec<SlotView>>, Option<Vec<velmux_pool::ParkedView>>);

pub enum Event {
    /// A new connection and its first message (`Hello` attaches).
    Client(UnixStream, ClientMsg),
    /// A one-shot control connection.
    Control(UnixStream, ClientMsg),
    FromClient(u64, ClientMsg),
    ClientGone(u64),
    PtyOutput(u64, Vec<u8>),
    PtyExit(u64),
    /// `for_pane`: claimed by `velmux claim` inside that pane, which moves to
    /// the slot; the stream gets the slot's path.
    /// `resume`: Claude sessions that worked on a parked branch, to bring back in the slot.
    Claimed { project: usize, job: String, result: Result<SlotView, String>, for_pane: Option<(u64, UnixStream)>, resume: Vec<String> },
    Released { project: usize, slot: String, result: Result<String, String> },
    /// A step of a background job (`job` key); a leading `slot-N:` names the slot.
    Progress { project: usize, job: String, text: String },
    /// Fresh slot health and parked work, per project, from a background refresh.
    /// `gen`: [`App::slots_gen`] when it started.
    Pools { gen: u64, fresh: Vec<PoolHealth> },
    Synced { job: String, result: Result<String, String> },
    /// The conversation index, rebuilt in the background.
    Indexed(Vec<crate::search::Doc>),
    /// Pull requests per project, from a background `gh` call.
    Prs(Vec<(usize, HashMap<String, velmux_pool::gh::Pr>)>),
    /// A PR was opened (or failed to): refresh PRs soon.
    PrOpened { job: String, result: Result<String, String> },
}

/// A git repo with its pool, or the "unclassified" group of plain folders.
pub struct Project {
    /// `None` for the unclassified group.
    pub pool: Option<Arc<Pool>>,
    /// The repo's config, or global defaults for unclassified folders.
    pub cfg: Config,
    pub slots: Vec<SlotView>,
    /// Branches released with work on them, waiting to be resumed.
    pub parked: Vec<velmux_pool::ParkedView>,
    /// Pull requests by head branch (GitHub repos, with `gh` installed).
    pub prs: HashMap<String, velmux_pool::gh::Pr>,
    /// Unclassified only: its folders, addressed by id in `Loc::slot`.
    pub folders: Vec<Folder>,
    /// Removed from the sidebar. Kept in place (locations refer to projects by
    /// index), skipped everywhere and not saved; opening it again brings it back.
    pub removed: bool,
}

pub struct Folder {
    pub id: String,
    pub path: PathBuf,
}

impl Project {
    fn repo(pool: Pool) -> Project {
        Project { cfg: pool.cfg.clone(), pool: Some(Arc::new(pool)), slots: Vec::new(), parked: Vec::new(), prs: HashMap::new(), folders: Vec::new(), removed: false }
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
    /// Searching Claude conversations (the index is `App::sessions`).
    Sessions { query: String, selected: usize },
    /// Picking a repo to add as a space: the git repos in the scratch dir.
    NewSpace { query: String, selected: usize, repos: Vec<PathBuf> },
    /// The short menu, with the highlighted row; `context` when it was opened
    /// by right-clicking a place in the sidebar.
    Menu { selected: usize, context: Option<MenuContext> },
    /// Choosing a theme: the highlighted one is previewed live; esc restores `original`.
    ThemePicker { selected: usize, original: crate::theme::Theme },
    Confirm { label: String, action: Confirm },
    /// Scrolling/selecting in the focused pane's history (state in `App::copy`).
    Copy,
}

/// How many conversations the search shows.
pub const SESSION_HITS: usize = 200;

/// A right-click menu: actions for one place, run there.
#[derive(Debug, Clone)]
pub struct MenuContext {
    pub target: Loc,
    /// The parked branch it's for, when opened on one.
    pub parked: Option<String>,
    pub title: String,
    pub items: Vec<(&'static str, Action)>,
    /// Where it was opened (the click).
    pub at: (u16, u16),
}

/// A mouse drag in progress.
enum Drag {
    /// Pressing on a repo heading or a folder in the sidebar: a click on release,
    /// or a reorder once it moves.
    Sidebar { grab: Grab, start: (u16, u16), moved: bool },
    /// Resizing the split behind a divider.
    Divider { loc: Loc, path: Vec<bool>, span: Rect, dir: Dir },
    /// Selecting text; copy mode opens on the first motion, copies on release.
    /// `click`: the app asked for the mouse, so a press that never moves is
    /// replayed to it on release (a click); a drag selects instead.
    Select { pane: u64, origin: (u16, u16), moved: bool, click: bool },
}

/// What's being dragged in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grab {
    Repo(usize),
    Folder(String),
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

const TOAST_FOR: Duration = Duration::from_millis(1600);

/// What a Claude in a repo's base checkout is told, so new work starts as a
/// conversation and Claude claims its own worktree slot when it's ready.
const BASE_PROMPT: &str = "You are running inside velmux, in the base checkout of the {repo} repo. \
Read, explore and plan here, but don't edit files in this checkout. When you're ready to change code, \
run `velmux claim <branch>` with a short branch name for the task (e.g. feat/login-redirect): it sets up a \
dedicated git worktree on that branch and prints its path. Do all edits, builds and commits in that path \
(cd there in your shell; use its absolute paths for file edits). When the work is finished and committed, \
you can run `velmux release` to hand the worktree back.";

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
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
    GhosttyKeys,
    DiscardParked { project: usize, branch: String },
    RemoveFolder(Loc),
    RemoveRepo(usize),
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
    /// The tab shown before the active one, for flipping back (not saved).
    pub previous: Option<usize>,
}

impl View {
    pub fn single(tab: Tab) -> View {
        View { tabs: vec![tab], active: 0, name: None, previous: None }
    }

    /// Show tab `t`, remembering the one it replaces.
    pub fn set_active(&mut self, t: usize) {
        if t != self.active {
            self.previous = Some(self.active);
            self.active = t;
        }
    }

    /// Tab `t` was removed: keep `previous` pointing at the same tab.
    pub fn forget_tab(&mut self, t: usize) {
        self.previous = match self.previous {
            Some(p) if p == t => None,
            Some(p) if p > t => Some(p - 1),
            p => p,
        };
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
    pub theme: crate::theme::Theme,
    /// Per-role overrides from config, re-applied when the theme changes.
    theme_colors: std::collections::BTreeMap<String, String>,
    pub sidebar_hidden: bool,
    pub sidebar_view: ui::SidebarView,
    /// Display order of projects (indices into `projects`, which never move,
    /// since locations refer to projects by index).
    pub project_order: Vec<usize>,
    /// A sidebar reorder in progress: what's dragged, and the screen row whose
    /// bottom edge is the insertion line (for drawing).
    pub sidebar_drag: Option<(Grab, u16)>,
    /// Shown once a client attaches (e.g. an invalid `prefix` in config).
    startup_warning: Option<String>,
    /// Background work in the status line: (key, what it's doing now).
    pub jobs: Vec<(String, String)>,
    /// Slots being claimed: (job key, current step: checking out, installing…).
    pub preparing: HashMap<Loc, (String, String)>,
    /// A background slot/parked refresh is running.
    refreshing: bool,
    /// Bumped by every direct refresh: a background one that started earlier
    /// is stale and its results are dropped.
    slots_gen: u64,
    /// When each repo was last fetched in the background.
    fetched: HashMap<usize, Instant>,
    /// For unique job keys.
    job_seq: u64,
    /// Every Claude conversation in velmux's repos and folders, for search.
    pub sessions: Vec<crate::search::Doc>,
    /// The index is being rebuilt.
    pub indexing: bool,
    /// Where new scratch sessions start; `new space` lists the repos in it.
    pub scratch_dir: PathBuf,
    /// A background PR check is running; when the last one started.
    checking_prs: bool,
    prs_checked: Option<Instant>,
    status: Option<(String, Instant)>,
    size: (u16, u16),
    /// `size` came from a client (or a hand-off), not the startup default:
    /// until it does, panes aren't resized (a running app would be squeezed
    /// to the default and might miss the resize back).
    size_known: bool,
    /// A redraw squeezed every pane by a column; restore them at this time.
    nudge: Option<Instant>,
    /// Panes were adopted from a live restart: nudge them once the client is
    /// back (heals apps an older server squeezed to the default size).
    adopted: bool,
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
    notify: velmux_pool::NotifySettings,
    /// A floating "copied to clipboard"-style message and when it appeared.
    toast: Option<(String, Instant)>,
    /// The last left press: when, which pane, which cell, how many in a row.
    last_click: Option<(Instant, u64, (u16, u16), u8)>,
    /// When each pane last notified, so a flapping agent doesn't spam.
    notified: HashMap<u64, Instant>,
    /// The binary this server runs, and its mtime when we last looked.
    exe: Option<(PathBuf, Option<SystemTime>)>,
    /// A newer binary seen once; restart when it's unchanged on the next look.
    exe_pending: Option<SystemTime>,
    /// Claude Code's config dir, for its per-process session records.
    pub claude_dir: PathBuf,
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
                    ClientMsg::ImportHerdr { .. }
                    | ClientMsg::Restart
                    | ClientMsg::Stop
                    | ClientMsg::Debug
                    | ClientMsg::ClaimFor { .. }
                    | ClientMsg::ReleaseFor { .. } => Event::Control(stream, first),
                    _ => Event::Client(stream, first),
                };
                let _ = tx.send(ev);
            });
        }
    });

    let (settings, mut warning) = match Env::from_process().and_then(|env| tui_settings(&env)) {
        Ok(s) => (s, None),
        Err(e) => (
            TuiSettings {
                prefix: None,
                mouse: true,
                auto_restart: true,
                keys: Default::default(),
                notify: Default::default(),
                theme: None,
                theme_colors: Default::default(),
                scratch_dir: std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default(),
            },
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
    app.notify = settings.notify.clone();
    app.scratch_dir = settings.scratch_dir.clone();
    let (theme, theme_warnings) = crate::theme::resolve(settings.theme.as_deref(), &settings.theme_colors);
    app.theme = theme;
    app.theme_colors = settings.theme_colors.clone();
    if !theme_warnings.is_empty() {
        let w = theme_warnings.join("; ");
        app.startup_warning = Some(match app.startup_warning.take() {
            Some(prev) => format!("{prev}; {w}"),
            None => w,
        });
    }
    match std::env::var_os("VELMUX_HANDOFF") {
        Some(path) => {
            // Don't pass it on to panes we start later.
            std::env::remove_var("VELMUX_HANDOFF");
            app.adopt(Path::new(&path));
        }
        None => app.restore(),
    }
    let (mut last_detect, mut last_slots, mut last_render, mut last_history) =
        (Instant::now(), Instant::now(), Instant::now(), Instant::now());
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
            app.refresh_slots_bg();
            app.refresh_prs_bg();
            app.check_binary();
            last_slots = Instant::now();
        }
        app.finish_nudge();
        if last_history.elapsed() >= Duration::from_secs(60) {
            app.save_histories(false);
            last_history = Instant::now();
        }
        if app.quit {
            app.save_histories(true);
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
            theme: crate::theme::builtin("default").unwrap(),
            theme_colors: Default::default(),
            sidebar_hidden: false,
            sidebar_view: ui::SidebarView::Spaces,
            project_order: Vec::new(),
            sidebar_drag: None,
            startup_warning,
            jobs: Vec::new(),
            preparing: HashMap::new(),
            refreshing: false,
            slots_gen: 0,
            fetched: HashMap::new(),
            job_seq: 0,
            sessions: Vec::new(),
            indexing: false,
            scratch_dir: PathBuf::new(),
            checking_prs: false,
            prs_checked: None,
            status: None,
            size: (80, 24),
            size_known: false,
            nudge: None,
            adopted: false,
            events,
            client: None,
            next_conn: 0,
            next_pane: 1,
            prev: Buffer::empty(Rect::new(0, 0, 80, 24)),
            copy: None,
            pending_out: Vec::new(),
            mouse,
            auto_restart: true,
            notify: Default::default(),
            notified: HashMap::new(),
            toast: None,
            last_click: None,
            // How it was launched (kept across restarts: exec keeps the env),
            // else the binary itself.
            exe: std::env::var_os("VELMUX_EXE")
                .map(PathBuf::from)
                .filter(|p| p.exists())
                .or_else(|| std::env::current_exe().ok().and_then(|p| p.canonicalize().ok()))
                .map(|p| {
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

    /// Projects in display order: the saved / dragged order, then any added since.
    pub fn ordered_projects(&self) -> Vec<usize> {
        let mut order: Vec<usize> = self.project_order.iter().copied().filter(|&p| p < self.projects.len()).collect();
        let missing: Vec<usize> = (0..self.projects.len()).filter(|p| !order.contains(p)).collect();
        order.extend(missing);
        order.retain(|&p| !self.projects[p].removed);
        order
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
                let project = self.projects.iter().position(|p| !p.is_unclassified() && !p.removed)?;
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
        self.projects.push(Project { pool: None, cfg, slots: Vec::new(), parked: Vec::new(), prs: HashMap::new(), folders: Vec::new(), removed: false });
        self.projects.len() - 1
    }

    /// A new scratch session: a shell in the scratch dir, as its own place
    /// below the divider (`scratch`, `scratch-2`, …).
    fn new_session(&mut self) {
        let dir = self.scratch_dir.clone();
        if !dir.is_dir() {
            return self.say(format!("{} isn't a folder (set scratch_dir in the config)", dir.display()));
        }
        let project = self.unclassified();
        self.projects[project].removed = false;
        let folders = &mut self.projects[project].folders;
        let mut id = "scratch".to_string();
        let mut n = 2;
        while folders.iter().any(|f| f.id == id) {
            id = format!("scratch-{n}");
            n += 1;
        }
        folders.push(Folder { id: id.clone(), path: dir });
        self.spawn(Loc { project, slot: Some(id) }, Kind::Shell, None);
        self.save();
    }

    /// Git repos directly in the scratch dir that aren't spaces yet, newest first.
    fn space_candidates(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.scratch_dir) else { return Vec::new() };
        let open: Vec<&Path> = self.projects.iter().filter(|p| !p.removed).filter_map(|p| p.base()).collect();
        let mut repos: Vec<(SystemTime, PathBuf)> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join(".git").exists() && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
            .filter(|p| !open.iter().any(|o| o == p))
            .map(|p| (std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH), p))
            .collect();
        repos.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        repos.into_iter().map(|(_, p)| p).collect()
    }

    /// The chooser's rows for `query`: matching repos, or a typed path.
    pub fn space_matches(repos: &[PathBuf], query: &str) -> Vec<PathBuf> {
        let q = query.trim();
        if q.starts_with('/') || q.starts_with('~') {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            let path = match q.strip_prefix('~') {
                Some(rest) => home.join(rest.trim_start_matches('/')),
                None => PathBuf::from(q),
            };
            return vec![path];
        }
        let mut hits: Vec<(i32, usize, &PathBuf)> = repos
            .iter()
            .enumerate()
            .filter_map(|(i, p)| picker::score(q, &p.file_name()?.to_string_lossy()).map(|s| (s, i, p)))
            .collect();
        hits.sort_by_key(|(s, i, _)| (std::cmp::Reverse(*s), *i));
        hits.into_iter().map(|(_, _, p)| p.clone()).collect()
    }

    fn new_space_input(&mut self, bytes: &[u8]) -> usize {
        let Mode::NewSpace { query, selected, repos } = &mut self.mode else { return bytes.len() };
        let count = App::space_matches(repos, query).len();
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
            Some(0x1b | 0x03) => self.mode = Mode::Normal,
            Some(b'\r' | b'\n') => {
                let chosen = App::space_matches(repos, query).get(*selected).cloned();
                self.mode = Mode::Normal;
                if let Some(path) = chosen {
                    self.add_space(&path);
                }
            }
            _ => {}
        }
        used
    }

    /// Add the repo at `path` as a space and go to its base.
    fn add_space(&mut self, path: &Path) {
        match Env::from_process().and_then(|env| Pool::discover(env, path)) {
            Ok(_) => {
                self.open_cwd(path);
                self.full_redraw = true;
                self.toast(&format!("added {}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
            }
            Err(_) => self.say(format!("{} isn't a git repo", path.display())),
        }
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
            (Some(id), _) if project.is_unclassified() => name.unwrap_or(id).to_string(),
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
                    ClientMsg::ClaimFor { pane, branch } => {
                        let Some(project) = self.pane(pane).map(|p| p.loc.project) else {
                            let _ = ServerMsg::Reply("error: that pane isn't in velmux".into()).write(&mut stream);
                            return;
                        };
                        self.claim_in(project, branch, Some((pane, stream)));
                        return;
                    }
                    ClientMsg::ReleaseFor { pane } => self.release_for(pane),
                    ClientMsg::Debug => self.debug_dump(),
                    ClientMsg::Stop => {
                        self.quit = true;
                        "stopping — panes and Claude sessions are restored next time you run velmux".into()
                    }
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
            Event::Claimed { project, job, result, for_pane, resume } => {
                self.jobs.retain(|(k, _)| k != &job);
                self.preparing.retain(|_, (k, _)| k != &job);
                self.refresh_slots();
                match (result, for_pane) {
                    (Ok(slot), None) if !resume.is_empty() => {
                        let loc = Loc { project, slot: Some(slot.id.clone()) };
                        let branch = slot.branch.clone().unwrap_or_default();
                        self.say(format!("resumed {branch} in {}", slot.id));
                        self.resume_sessions(loc, &resume);
                    }
                    (Ok(slot), None) => {
                        let loc = Loc { project, slot: Some(slot.id.clone()) };
                        self.say(format!("claimed {}", slot.id));
                        self.spawn(loc, Kind::Agent, None);
                    }
                    (Ok(slot), Some((pane, mut stream))) => {
                        let loc = Loc { project, slot: Some(slot.id.clone()) };
                        self.move_pane(pane, loc);
                        let branch = slot.branch.clone().map(|b| format!(" on {b}")).unwrap_or_default();
                        self.say(format!("claimed {}{branch}", slot.id));
                        let _ = ServerMsg::Reply(slot.path.display().to_string()).write(&mut stream);
                    }
                    (Err(e), None) => self.say(format!("claim failed: {e}")),
                    (Err(e), Some((_, mut stream))) => {
                        let _ = ServerMsg::Reply(format!("error: {e}")).write(&mut stream);
                    }
                }
            }
            Event::Progress { project, job, text } => {
                // `slot-2: installing: bun install` → that slot is busy with that step.
                let (slot, step) = match text.split_once(": ") {
                    Some((s, rest)) if s.starts_with("slot-") => (Some(s.to_string()), rest.to_string()),
                    _ => (None, text.clone()),
                };
                if let Some(slot) = slot {
                    self.preparing.insert(Loc { project, slot: Some(slot) }, (job.clone(), step.clone()));
                }
                let name = self.projects.get(project).map(|p| p.name().to_string()).unwrap_or_default();
                if let Some(entry) = self.jobs.iter_mut().find(|(k, _)| k == &job) {
                    entry.1 = format!("{name}: {step}");
                }
                self.dirty = true;
            }
            Event::Pools { gen, fresh } => {
                self.refreshing = false;
                if gen != self.slots_gen {
                    return; // a claim / release refreshed since this one started
                }
                for (p, slots, parked) in fresh {
                    let Some(project) = self.projects.get_mut(p) else { continue };
                    if let Some(slots) = slots {
                        project.slots = slots;
                    }
                    if let Some(parked) = parked {
                        project.parked = parked;
                    }
                }
                self.dirty = true;
            }
            Event::Prs(fresh) => {
                self.checking_prs = false;
                for (p, prs) in fresh {
                    self.pr_changes(p, &prs);
                    if let Some(project) = self.projects.get_mut(p) {
                        project.prs = prs;
                    }
                }
                self.dirty = true;
            }
            Event::PrOpened { job, result } => {
                self.jobs.retain(|(k, _)| k != &job);
                match result {
                    Ok(msg) => self.toast(&msg),
                    Err(e) => self.say(e),
                }
                self.prs_checked = None;
            }
            Event::Indexed(docs) => {
                self.sessions = docs;
                self.indexing = false;
                if let Mode::Sessions { selected, .. } = &mut self.mode {
                    *selected = 0;
                }
                self.dirty = true;
            }
            Event::Synced { job, result } => {
                self.jobs.retain(|(k, _)| k != &job);
                match result {
                    Ok(msg) => self.toast(&msg),
                    Err(e) => self.say(format!("sync: {e}")),
                }
                self.refresh_slots();
            }
            Event::Released { project, slot, result } => {
                self.jobs.retain(|(k, _)| k != &format!("release {slot}"));
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
                if std::mem::take(&mut self.adopted) {
                    self.redraw();
                }
                self.open_cwd(&cwd);
                if let Some(w) = self.startup_warning.take() {
                    self.say(w);
                }
            }
            ClientMsg::Resize { cols, rows } => self.resize(cols, rows),
            ClientMsg::Input(bytes) => self.input(&bytes),
            // Only valid as the first message of a control connection.
            ClientMsg::ImportHerdr { .. } | ClientMsg::Restart | ClientMsg::Stop | ClientMsg::Debug | ClientMsg::ClaimFor { .. } | ClientMsg::ReleaseFor { .. } => {}
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.size = (cols.max(20), rows.max(5));
        self.size_known = true;
        self.relayout();
    }

    /// Size every pane to its rect in its location's layout.
    fn relayout(&mut self) {
        self.reveal_focus();
        let body = ui::pane_area(self.size.0, self.size.1, self.sidebar_hidden);
        // Every tab, not just the visible one, so switching tabs never shows a stale size.
        if !self.size_known {
            self.full_redraw = true;
            self.dirty = true;
            return;
        }
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
                view.set_active(t);
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
            .filter(|(loc, _)| !self.projects[loc.project].removed)
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
                                history: (p.dumped_seq > 0).then(|| p.history_key.clone()),
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
            // Saved in display order, so restore rebuilds that order.
            projects: self
                .ordered_projects()
                .into_iter()
                .filter_map(|p| self.projects[p].base().map(Path::to_path_buf))
                .collect(),
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
        let size = self.size_known.then_some(self.size);
        let handoff = session::Handoff { saved, live, focus: self.focus, size };
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
        if let Some(size) = handoff.size {
            self.size = size;
            self.size_known = true;
        }
        self.adopted = true;
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
                self.views.insert(loc, View { tabs, active, name, previous: None });
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
                        self.restore_history(new, p.history.as_deref());
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
                self.views.insert(loc, View { tabs, active, name: view_name, previous: None });
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
            Some(i) => {
                self.projects[i].removed = false;
                i
            }
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

    /// [`refresh_slots`](Self::refresh_slots) off the main thread (git status
    /// and friends can be slow in big repos), fetching repos that are due.
    pub fn refresh_slots_bg(&mut self) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        let mut work = Vec::new();
        for (i, project) in self.projects.iter().enumerate() {
            let Some(pool) = project.pool.clone() else { continue };
            if project.removed {
                continue;
            }
            let every = project.cfg.fetch_every;
            let due = every > 0 && self.fetched.get(&i).is_none_or(|t| t.elapsed() >= Duration::from_secs(every * 60));
            if due {
                self.fetched.insert(i, Instant::now());
            }
            work.push((i, pool, due));
        }
        let (tx, gen) = (self.events.clone(), self.slots_gen);
        std::thread::spawn(move || {
            let fresh = work
                .into_iter()
                .map(|(i, pool, fetch)| {
                    if fetch {
                        let _ = pool.fetch();
                    }
                    (i, pool.status().ok(), pool.parked().ok())
                })
                .collect();
            let _ = tx.send(Event::Pools { gen, fresh });
        });
    }

    /// Claude's transcript folders for every repo (base and slots) and folder.
    fn transcript_dirs(&self) -> Vec<PathBuf> {
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
    fn reindex_sessions(&mut self) {
        if self.indexing {
            return;
        }
        self.indexing = true;
        let (dirs, old, tx) = (self.transcript_dirs(), self.sessions.clone(), self.events.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Event::Indexed(crate::search::scan(&dirs, old)));
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
    fn open_session(&mut self, doc: &crate::search::Doc) {
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
                return self.claim_resuming(loc.project, Some(branch), None, vec![doc.id.clone()]);
            }
            (false, false, None) => Loc { project: loc.project, slot: None },
        };
        self.say(format!("resuming “{}” in {}", doc.title, self.loc_name(&target)));
        self.resume_sessions(target, std::slice::from_ref(&doc.id));
    }

    fn sessions_input(&mut self, bytes: &[u8]) -> usize {
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

    /// Repaint everything, and squeeze every pane a column narrower for a
    /// moment: each app re-reads its size, fixing one stuck at a stale width.
    fn redraw(&mut self) {
        for pane in &mut self.panes {
            let (rows, cols) = pane.parser.screen().size();
            if cols.get() > 2 {
                pane.resize(cols.get() - 1, rows.get());
            }
        }
        self.nudge = Some(Instant::now() + Duration::from_millis(200));
        self.full_redraw = true;
        self.dirty = true;
    }

    /// The other half of [`redraw`](Self::redraw): back to the real sizes.
    pub fn finish_nudge(&mut self) {
        if self.nudge.is_some_and(|t| Instant::now() >= t) {
            self.nudge = None;
            self.relayout();
        }
    }

    /// `velmux debug`: the focused pane as velmux holds it (pty and parser
    /// size, the rect it's drawn in, each row), to compare with the screen.
    fn debug_dump(&mut self) -> String {
        let body = ui::pane_area(self.size.0, self.size.1, self.sidebar_hidden);
        let Some(pane) = self.focus.and_then(|id| self.panes.iter_mut().find(|p| p.id == id)) else {
            return "no focused pane".into();
        };
        let (rows, cols) = pane.parser.screen().size();
        let pty = pane.pty_size();
        let mut out = format!(
            "pane {} · client {}x{} · pane area {}x{} at col {} · parser {}x{} · pty {} · alt screen {} · scrollback {}\n",
            pane.id,
            self.size.0,
            self.size.1,
            body.width,
            body.height,
            body.x,
            cols.get(),
            rows.get(),
            pty.map_or("?".into(), |(c, r)| format!("{c}x{r}")),
            pane.parser.screen().alternate_screen(),
            pane.parser.screen().scrollback(),
        );
        for (i, row) in pane.parser.screen().rows(0, cols.get()).enumerate() {
            out.push_str(&format!("{i:>3}│{}\n", row.trim_end()));
        }
        out
    }

    /// The current slot's port, if it has one.
    pub fn loc_port(&self, loc: &Loc) -> Option<u16> {
        let pool = self.projects.get(loc.project)?.pool.as_ref()?;
        pool.slot_port(loc.slot.as_ref()?)
    }

    /// `http://localhost:<port>` for the current slot, in the default browser.
    fn open_browser(&mut self) {
        let Some(port) = self.current_loc().and_then(|l| self.loc_port(&l)) else {
            return self.say("only slots have their own port (see [ports] in the config)");
        };
        self.open_url(&format!("http://localhost:{port}"));
    }

    /// Check PRs for every GitHub repo with claimed or parked branches, about
    /// once a minute (sooner after opening one), in the background.
    fn refresh_prs_bg(&mut self) {
        if self.checking_prs || self.prs_checked.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) || !velmux_pool::gh::available() {
            return;
        }
        let work: Vec<(usize, Arc<Pool>)> = self
            .projects
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.removed && (p.slots.iter().any(|s| s.status == Status::InUse) || !p.parked.is_empty()))
            .filter_map(|(i, p)| p.pool.clone().map(|pool| (i, pool)))
            .collect();
        if work.is_empty() {
            return;
        }
        self.checking_prs = true;
        self.prs_checked = Some(Instant::now());
        let tx = self.events.clone();
        std::thread::spawn(move || {
            let fresh = work
                .into_iter()
                .filter(|(_, pool)| velmux_pool::gh::on_github(&pool.base))
                .filter_map(|(i, pool)| velmux_pool::gh::prs(&pool.base).ok().map(|prs| (i, prs)))
                .collect();
            let _ = tx.send(Event::Prs(fresh));
        });
    }

    /// Tell you when a claimed branch's PR merges or its checks fail.
    fn pr_changes(&mut self, project: usize, fresh: &HashMap<String, velmux_pool::gh::Pr>) {
        use velmux_pool::gh::{Checks, PrState};
        let Some(p) = self.projects.get(project) else { return };
        let mut news = Vec::new();
        for slot in p.slots.iter().filter(|s| s.status == Status::InUse) {
            let Some(branch) = &slot.branch else { continue };
            let (Some(old), Some(new)) = (p.prs.get(branch), fresh.get(branch)) else { continue };
            if old.state != PrState::Merged && new.state == PrState::Merged {
                news.push((format!("#{} merged ({branch}) — release {} from its menu", new.number, slot.id), true));
            } else if old.checks != Checks::Failing && new.checks == Checks::Failing {
                news.push((format!("checks failed on #{} ({branch}): {}", new.number, new.failed.join(", ")), false));
            }
        }
        for (body, good) in news {
            self.toast(&body);
            self.notify_text(&body, good);
        }
    }

    /// A desktop notification and sound (good news: the done sound).
    fn notify_text(&mut self, body: &str, good: bool) {
        use crate::notify;
        if self.notify.desktop {
            if self.client.is_some() {
                self.pending_out.extend_from_slice(&notify::osc777("velmux", body));
                self.dirty = true;
            } else {
                notify::system("velmux", body);
            }
        }
        if self.notify.sound {
            notify::play(if good { &self.notify.done_sound } else { &self.notify.blocked_sound });
        }
    }

    /// The current slot's branch and its PR, if it has one.
    fn current_pr(&self) -> (Option<Loc>, Option<String>, Option<velmux_pool::gh::Pr>) {
        let Some(loc) = self.current_loc() else { return (None, None, None) };
        let project = &self.projects[loc.project];
        let branch = loc.slot.as_ref().and_then(|id| project.slots.iter().find(|s| &s.id == id)).and_then(|s| s.branch.clone());
        let pr = branch.as_ref().and_then(|b| project.prs.get(b)).cloned();
        (Some(loc), branch, pr)
    }

    /// Open the slot's PR, or push the branch and open GitHub's new-PR page.
    fn open_pr(&mut self) {
        let (loc, branch, pr) = self.current_pr();
        if let Some(pr) = pr {
            return self.open_url(&pr.url);
        }
        let (Some(loc), Some(branch)) = (loc, branch) else { return self.say("focus a slot on a branch to open its PR") };
        if !velmux_pool::gh::available() {
            return self.say("install the GitHub CLI (gh) for PRs");
        }
        let Some(dir) = self.loc_cwd(&loc) else { return };
        self.job_seq += 1;
        let job = format!("pr {}", self.job_seq);
        self.jobs.push((job.clone(), format!("pushing {branch}")));
        let tx = self.events.clone();
        std::thread::spawn(move || {
            let result = velmux_pool::gh::push_and_open(&dir, &branch).map(|_| format!("pushed {branch}; finish the PR in your browser")).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::PrOpened { job, result });
        });
    }

    fn pr_action(&mut self, action: Action) {
        let (loc, _, pr) = self.current_pr();
        let Some(pr) = pr else { return self.say("this slot's branch has no PR yet") };
        match action {
            Action::OpenChecks => self.open_url(&format!("{}/checks", pr.url)),
            Action::CopyPrLink => {
                self.set_clipboard(&pr.url);
                self.toast("PR link copied");
            }
            _ => {
                // Hand the failures to the slot's Claude, as if you'd typed it.
                let Some(loc) = loc else { return };
                let Some(pane) = self.panes.iter_mut().find(|p| p.loc == loc && (p.agent.is_some() || p.session.is_some())) else {
                    return self.say("no Claude in this slot to send it to");
                };
                let failed = if pr.failed.is_empty() { "some checks".to_string() } else { pr.failed.join(", ") };
                let text = format!(
                    "CI failed on PR #{} ({failed}). Run `gh pr checks {}` and `gh run view --log-failed` to see why, then fix it.",
                    pr.number, pr.number
                );
                let id = pane.id;
                pane.write_input(format!("\x1b[200~{text}\x1b[201~").as_bytes());
                pane.write_input(b"\r");
                self.focus = Some(id);
                self.toast("sent the failures to Claude");
            }
        }
    }

    /// Open a URL in the default browser.
    fn open_url(&mut self, url: &str) {
        let url = url.to_string();
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        match std::process::Command::new(opener).arg(&url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
                self.toast(&format!("opened {url}"));
            }
            Err(e) => self.say(format!("can't open {url}: {e}")),
        }
    }

    /// Rebase (or merge) a slot's branch onto the base ref, in the background.
    fn sync_slot(&mut self) {
        let Some(loc) = self.current_loc() else { return };
        let (Some(slot), Some(pool)) = (loc.slot.clone(), self.projects[loc.project].pool.clone()) else {
            return self.say("focus a slot to sync it with the base");
        };
        self.job_seq += 1;
        let job = format!("sync {}", self.job_seq);
        self.jobs.push((job.clone(), format!("syncing {slot}")));
        let tx = self.events.clone();
        std::thread::spawn(move || {
            let result = pool.sync(&slot, &|_| {}).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Synced { job, result });
        });
    }

    pub fn refresh_slots(&mut self) {
        self.slots_gen += 1;
        for project in &mut self.projects {
            if let Some(Ok(slots)) = project.pool.as_ref().map(|p| p.status()) {
                project.slots = slots;
            }
            if let Some(Ok(parked)) = project.pool.as_ref().map(|p| p.parked()) {
                project.parked = parked;
            }
        }
        self.dirty = true;
    }

    /// Tell you an agent needs you: a desktop notification and a sound. Skips
    /// agents that just started (restores, trust prompts), a blocked agent
    /// you're looking at, and repeats within a few seconds.
    fn alert(&mut self, id: u64, kind: crate::notify::Kind) {
        use crate::notify::{self, Kind};
        let Some(pane) = self.pane(id) else { return };
        if pane.started_at.elapsed().unwrap_or_default() < Duration::from_secs(6) {
            return;
        }
        let agent = pane.agent.unwrap_or("agent");
        let place = ui::short_loc(self, &pane.loc);
        let task = ui::useful_title(pane).map(|t| format!(" · {t}")).unwrap_or_default();
        let watching = self.client.is_some() && self.focus == Some(id);
        if kind == Kind::Blocked && watching {
            return;
        }
        if self.notified.get(&id).is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        self.notified.insert(id, Instant::now());
        let body = match kind {
            Kind::Blocked => format!("{place}: {agent} needs you{task}"),
            Kind::Done => format!("{place}: {agent} finished{task}"),
        };
        if self.notify.desktop {
            if self.client.is_some() {
                self.pending_out.extend_from_slice(&notify::osc777("velmux", &body));
                self.dirty = true;
            } else {
                notify::system("velmux", &body);
            }
        }
        if self.notify.sound {
            notify::play(match kind {
                Kind::Blocked => &self.notify.blocked_sound,
                Kind::Done => &self.notify.done_sound,
            });
        }
    }

    fn detect_all(&mut self) {
        let focus = self.focus;
        let mut changed = false;
        let mut sessions_changed = false;
        let claude_dir = self.claude_dir.clone();
        let mut alerts = Vec::new();
        for pane in &mut self.panes {
            let before = pane.session.clone();
            let was = (pane.state, pane.unseen_done);
            changed |= pane.detect(focus == Some(pane.id), &claude_dir);
            sessions_changed |= pane.session != before;
            if let Some(kind) = crate::notify::transition(was, (pane.state, pane.unseen_done)) {
                alerts.push((pane.id, kind));
            }
        }
        for (id, kind) in alerts {
            self.alert(id, kind);
        }
        if sessions_changed {
            self.save();
        }
        self.dirty |= changed || self.status.is_some() || self.toast.is_some();
        if self.toast.as_ref().is_some_and(|(_, at)| at.elapsed() >= TOAST_FOR) {
            self.toast = None;
        }
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
            if ev.is_press() && (ev.is_left() || ev.is_right()) {
                let area = ui::menu_area(self, cols, rows);
                let item = (ev.is_left() && area.contains(at) && ev.y > area.y).then(|| (ev.y - area.y - 1) as usize);
                let Mode::Menu { context, .. } = std::mem::replace(&mut self.mode, Mode::Normal) else { unreachable!() };
                match item {
                    Some(i) => self.run_menu_item(context, i),
                    // Right-clicking another place opens its menu instead.
                    None if ev.is_right() && !area.contains(at) && ev.x < ui::sidebar_width(cols, self.sidebar_hidden) => {
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
        if ev.x < ui::sidebar_width(cols, self.sidebar_hidden) {
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
                let body = ui::pane_area(cols, rows, self.sidebar_hidden);
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
            let body = ui::pane_area(cols, rows, self.sidebar_hidden);
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
    fn count_click(&mut self, pane: u64, cell: (u16, u16), ev: &MouseEvent) -> u8 {
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
    fn click_select(&mut self, pane: u64, lx: u16, ly: u16, clicks: u8) {
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

    fn continue_drag(&mut self, drag: Drag, ev: MouseEvent) {
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
                    let gap = if moved { self.drop_gap(&grab, ev.y) } else { None };
                    self.sidebar_drag = gap.map(|(_, line)| (grab.clone(), line));
                }
                self.drag = Some(Drag::Sidebar { grab, start, moved });
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
                self.drag = Some(Drag::Select { pane, origin, moved, click });
            }
        }
    }

    fn pane_inner(&self, id: u64) -> Option<Rect> {
        let loc = self.pane(id)?.loc.clone();
        let titled = self.titled(&loc);
        let (rects, _) = self.view_rects(&loc);
        rects.into_iter().find(|(p, _)| *p == id).map(|(_, r)| ui::content_rect(r, titled))
    }

    /// What a press at sidebar line `y` would drag, if anything.
    fn grab_at(&self, y: u16) -> Option<Grab> {
        if self.sidebar_view != ui::SidebarView::Spaces {
            return None;
        }
        match ui::sidebar_rows(self, self.size.1).into_iter().find(|(ry, _)| *ry == y)?.1 {
            ui::Row::Project(p) => Some(Grab::Repo(p)),
            ui::Row::Location(loc) if self.projects[loc.project].is_unclassified() => loc.slot.map(Grab::Folder),
            _ => None,
        }
    }

    /// The draggable items of `grab`'s kind (repos or folders) in sidebar order,
    /// each with the screen rows it covers.
    fn drag_items(&self, grab: &Grab) -> Vec<(Grab, u16, u16)> {
        let mut items: Vec<(Grab, u16, u16)> = Vec::new();
        for (y, row) in ui::sidebar_rows(self, self.size.1) {
            match (grab, row) {
                (Grab::Repo(_), ui::Row::Project(p)) => items.push((Grab::Repo(p), y, y)),
                (Grab::Repo(_), ui::Row::Location(loc)) if !self.projects[loc.project].is_unclassified() => {
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
    fn drop_gap(&self, grab: &Grab, y: u16) -> Option<(usize, u16)> {
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

    /// Move `grab` into the gap under the pointer.
    fn drop_on(&mut self, grab: &Grab, y: u16) {
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
        }
        self.save();
        self.dirty = true;
    }

    fn sidebar_click(&mut self, x: u16, y: u16) {
        let Some((_, row)) = ui::sidebar_rows(self, self.size.1).into_iter().find(|(ry, _)| *ry == y) else {
            return;
        };
        let side = ui::sidebar_width(self.size.0, self.sidebar_hidden);
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
            // The `+` at the right of the sessions divider.
            ui::Row::SessionsDivider => {
                if x + 4 >= side {
                    self.new_session();
                }
                return;
            }
            ui::Row::Heading(_) | ui::Row::Divider | ui::Row::Blank => return,
            ui::Row::TopBar | ui::Row::ViewTabs | ui::Row::Agent { .. } | ui::Row::Parked(..) => unreachable!("handled above"),
            ui::Row::Location(loc) => loc,
            ui::Row::Project(p) => {
                // Open the project at its first location with panes, else its root.
                let live = self.live_locations().into_iter().find(|l| l.project == p);
                let first = self.projects[p].folders.first().map(|f| f.id.clone());
                live.unwrap_or(Loc { project: p, slot: if self.projects[p].is_unclassified() { first } else { None } })
            }
        };
        self.go_to(loc);
    }

    /// Show a place: its active tab, focused where you left it.
    fn go_to(&mut self, loc: Loc) {
        self.focus = self.views.get(&loc).and_then(|v| v.layout.leaves().first().copied());
        self.current = Some(loc);
        self.full_redraw = true;
    }

    /// Right-click on a sidebar row: the menu of things to do in that place.
    fn open_context_menu(&mut self, x: u16, y: u16) {
        let Some((_, row)) = ui::sidebar_rows(self, self.size.1).into_iter().find(|(ry, _)| *ry == y) else { return };
        if let ui::Row::Parked(p, i) = row {
            let Some(branch) = self.projects[p].parked.get(i).map(|v| v.branch.clone()) else { return };
            let items = vec![("resume in a slot", Action::ResumeParked), ("discard changes…", Action::DiscardParked)];
            let context = MenuContext { target: Loc { project: p, slot: None }, parked: Some(branch.clone()), title: format!("parked · {branch}"), items, at: (x, y) };
            self.mode = Mode::Menu { selected: 0, context: Some(context) };
            return;
        }
        let target = match row {
            ui::Row::Location(loc) => loc,
            ui::Row::Project(p) if !self.projects[p].is_unclassified() => Loc { project: p, slot: None },
            ui::Row::Agent { pane, .. } => self.panes[pane].loc.clone(),
            _ => return,
        };
        let folder = self.projects[target.project].is_unclassified();
        let items: Vec<(&'static str, Action)> = match (folder, target.slot.is_some()) {
            // The space: start work (Claude plans in the base and claims its
            // own slot), or take a slot yourself; manage the repo.
            (false, false) => vec![
                ("new conversation", Action::AgentBase),
                ("claim a slot…", Action::ClaimSlot),
                ("shell in base", Action::NewShell),
                ("open base in editor", Action::NewEditor),
                ("rename space", Action::RenameLocation),
                ("remove repo", Action::RemovePlace),
            ],
            // A slot: work on what's already there, get it reviewed, hand it back.
            (false, true) => {
                use velmux_pool::gh::{Checks, PrState};
                let project = &self.projects[target.project];
                let branch = project.slots.iter().find(|s| Some(&s.id) == target.slot.as_ref()).and_then(|s| s.branch.clone());
                let pr = branch.as_ref().and_then(|b| project.prs.get(b));
                let mut items = Vec::new();
                if pr.is_some_and(|p| p.state == PrState::Merged) {
                    items.push(("release (PR merged)", Action::RemovePlace));
                }
                items.extend([("claude in this slot", Action::AgentHere), ("sync with base", Action::SyncSlot)]);
                match pr {
                    Some(p) => {
                        items.push(("open PR", Action::OpenPr));
                        if p.checks == Checks::Failing {
                            items.push(("send failures to Claude", Action::SendFailures));
                        }
                        items.extend([("open checks", Action::OpenChecks), ("copy PR link", Action::CopyPrLink)]);
                    }
                    None if branch.is_some() => items.push(("push & open PR", Action::OpenPr)),
                    None => {}
                }
                items.extend([
                    ("open in browser", Action::OpenBrowser),
                    ("shell here", Action::NewShell),
                    ("new tab", Action::NewTab),
                    ("open in editor", Action::NewEditor),
                    ("copy path", Action::CopyPath),
                    ("rename slot", Action::RenameLocation),
                ]);
                if !pr.is_some_and(|p| p.state == PrState::Merged) {
                    items.push(("release slot", Action::RemovePlace));
                }
                items
            }
            (true, _) => vec![
                ("claude here", Action::AgentHere),
                ("shell here", Action::NewShell),
                ("new tab", Action::NewTab),
                ("open in editor", Action::NewEditor),
                ("copy path", Action::CopyPath),
                ("rename session", Action::RenameLocation),
                ("remove session", Action::RemovePlace),
            ],
        };
        let title = self.loc_name(&target);
        self.mode = Mode::Menu { selected: 0, context: Some(MenuContext { target, parked: None, title, items, at: (x, y) }) };
    }

    /// Run menu row `i`: the global menu's action here, or a context menu's
    /// action in its place (going there first).
    fn run_menu_item(&mut self, context: Option<MenuContext>, i: usize) {
        match context {
            Some(c) => {
                let Some(&(_, action)) = c.items.get(i) else { return };
                if let Some(branch) = c.parked {
                    let project = c.target.project;
                    return match action {
                        Action::ResumeParked => self.claim_in(project, Some(branch), None),
                        _ => {
                            self.mode = Mode::Confirm {
                                label: format!("discard the parked changes on {branch}? commits on the branch are kept"),
                                action: Confirm::DiscardParked { project, branch },
                            }
                        }
                    };
                }
                if self.current_loc().as_ref() != Some(&c.target) {
                    if self.copy.is_some() {
                        self.exit_copy();
                    }
                    self.go_to(c.target);
                }
                self.run(action);
            }
            None => {
                if let Some(&(_, action)) = ui::MENU.get(i) {
                    self.run(action);
                }
            }
        }
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

    /// Send a held-back click (press + release) to the pane's app.
    fn replay_click(&mut self, pane: u64, (lx, ly): (u16, u16), release: &MouseEvent) {
        let Some(p) = self.panes.iter_mut().find(|p| p.id == pane) else { return };
        let screen = p.parser.screen();
        let (mode, enc) = (screen.mouse_protocol_mode(), screen.mouse_protocol_encoding());
        let press = MouseEvent { release: false, ..*release };
        for ev in [press, *release] {
            if let Some(bytes) = mouse::encode(&ev, lx, ly, mode, enc) {
                p.write_raw(&bytes);
            }
        }
    }

    fn yank_selection(&mut self) {
        let mut text = None;
        self.with_copy(|c, p| text = c.selected_text(p));
        if let Some(text) = text.filter(|t| !t.trim().is_empty()) {
            self.set_clipboard(&text);
            self.toast("copied to clipboard");
        }
        self.exit_copy();
    }

    /// A brief floating message over the pane area.
    pub fn toast(&mut self, text: &str) {
        self.toast = Some((text.to_string(), Instant::now()));
        self.dirty = true;
    }

    pub fn toast_text(&self) -> Option<&str> {
        self.toast.as_ref().filter(|(_, at)| at.elapsed() < TOAST_FOR).map(|(t, _)| t.as_str())
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
                Mode::Sessions { .. } => self.sessions_input(bytes),
                Mode::NewSpace { .. } => self.new_space_input(bytes),
                Mode::Menu { .. } => self.menu_input(bytes),
                Mode::ThemePicker { .. } => self.theme_input(bytes),
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
        // No pane to take them: an empty screen's single-key actions.
        if self.focused().is_none() {
            let loc = self.current_loc();
            let (_, _, items, setup) = ui::empty_screen(self, loc.as_ref());
            let chosen = bytes.iter().find_map(|b| items.iter().chain(&setup).find(|it| it.key as u32 == *b as u32).map(|it| it.action));
            if let Some(action) = chosen {
                if action == Action::Help {
                    self.mode = Mode::Prefix;
                } else {
                    self.run(action);
                }
                self.dirty = true;
            }
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

    fn theme_input(&mut self, bytes: &[u8]) -> usize {
        use crate::theme::{resolve, set_theme_line, NAMES};
        let Mode::ThemePicker { selected, .. } = &mut self.mode else { return bytes.len() };
        let n = NAMES.len();
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
            b'k' | 0x10 | b'j' | 0x0e => {
                *selected = if matches!(key, b'k' | 0x10) { (*selected + n - 1) % n } else { (*selected + 1) % n };
                // Live preview, with your per-role overrides on top.
                self.theme = resolve(Some(NAMES[*selected]), &self.theme_colors).0;
            }
            b'\r' | b'\n' => {
                let name = NAMES[*selected];
                self.mode = Mode::Normal;
                let saved = Env::from_process().and_then(|env| {
                    let text = std::fs::read_to_string(&env.global_config).unwrap_or_default();
                    if let Some(dir) = env.global_config.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    std::fs::write(&env.global_config, set_theme_line(&text, name))?;
                    Ok(())
                });
                match saved {
                    Ok(()) => self.toast(&format!("theme: {name}")),
                    Err(e) => self.say(format!("theme {name} applied, but saving it failed: {e:#}")),
                }
            }
            0x1b | 0x03 | b'q' => {
                if let Mode::ThemePicker { original, .. } = std::mem::replace(&mut self.mode, Mode::Normal) {
                    self.theme = original;
                }
            }
            _ => {}
        }
        self.full_redraw = true;
        used
    }

    fn menu_input(&mut self, bytes: &[u8]) -> usize {
        let Mode::Menu { selected, context } = &mut self.mode else { return bytes.len() };
        let n = context.as_ref().map_or(ui::MENU.len(), |c| c.items.len());
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
                let i = *selected;
                let Mode::Menu { context, .. } = std::mem::replace(&mut self.mode, Mode::Normal) else { unreachable!() };
                self.run_menu_item(context, i);
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
            Action::AgentHere => {
                if let Some(loc) = self.current_loc() {
                    self.spawn(loc, Kind::Agent, None);
                }
            }
            Action::SyncSlot => self.sync_slot(),
            Action::InstallSkill => match crate::skill::install(&crate::skill::user_path(&self.claude_dir)) {
                Ok(_) => self.toast("Claude skill installed: new Claude sessions know velmux"),
                Err(e) => self.say(format!("skill: {e:#}")),
            },
            Action::SetupGhostty => match crate::ghostty::config_path() {
                Some(path) => {
                    let label = format!(
                        "add velmux's keybinds (the ⌘ shortcuts) to {}? its own settings are kept; reload Ghostty after (⌘⇧,)",
                        path.display()
                    );
                    self.mode = Mode::Confirm { label, action: Confirm::GhosttyKeys };
                }
                None => self.say("can't find Ghostty's config (HOME isn't set)"),
            },
            Action::Doctor => self.open_doctor(),
            Action::Redraw => self.redraw(),
            Action::NewSpace => {
                let repos = self.space_candidates();
                self.mode = Mode::NewSpace { query: String::new(), selected: 0, repos };
            }
            Action::NewSession => self.new_session(),
            Action::FindSession => {
                self.mode = Mode::Sessions { query: String::new(), selected: 0 };
                self.reindex_sessions();
            }
            Action::OpenBrowser => self.open_browser(),
            Action::OpenPr => self.open_pr(),
            Action::OpenChecks | Action::CopyPrLink | Action::SendFailures => self.pr_action(action),
            Action::EditScrollback => self.edit_scrollback(),
            Action::ResumeParked | Action::DiscardParked => self.say("right-click a parked branch in the sidebar"),
            Action::CopyPath => match self.current_loc().and_then(|l| self.loc_cwd(&l)) {
                Some(path) => {
                    self.set_clipboard(&path.to_string_lossy());
                    self.toast("path copied");
                }
                None => self.say("no folder here"),
            },
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
                    self.say("scratch sessions have no slots to release")
                }
                _ => self.say("focus a slot to release it (the base checkout is never released)"),
            },
            Action::Detach => self.say_bye("detached — run `velmux` to reattach"),
            Action::Quit => {
                self.mode = Mode::Confirm { label: "stop the server? panes and Claude sessions are restored next time you run velmux".into(), action: Confirm::Quit }
            }
            // Keep the prefix popup (with every key) open.
            Action::Help => self.mode = Mode::Prefix,
            Action::Slot(n) => self.jump_in_space(n as usize),
            Action::Base => match self.current_loc() {
                Some(loc) if !self.projects[loc.project].is_unclassified() => {
                    self.show_location(Loc { project: loc.project, slot: None })
                }
                Some(_) => self.say("scratch sessions have no base — ⌥1–9 picks a session"),
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
            Action::Menu => self.mode = Mode::Menu { selected: 0, context: None },
            Action::ThemePicker => {
                let selected = crate::theme::NAMES.iter().position(|n| *n == self.theme.name).unwrap_or(0);
                self.mode = Mode::ThemePicker { selected, original: self.theme.clone() };
            }
            Action::RemovePlace => match self.current_loc() {
                None => self.say("nothing to remove here"),
                Some(loc) if self.projects[loc.project].is_unclassified() => {
                    let n = self.panes.iter().filter(|p| p.loc == loc).count();
                    let label = format!("remove {} from velmux? {n} pane(s) close", self.loc_name(&loc));
                    self.mode = Mode::Confirm { label, action: Confirm::RemoveFolder(loc) };
                }
                Some(loc) if loc.slot.is_some() => self.run(Action::ReleaseSlot),
                Some(loc) => {
                    let n = self.panes.iter().filter(|p| p.loc.project == loc.project).count();
                    let name = self.projects[loc.project].name();
                    let label = format!("remove {name} from velmux? {n} pane(s) close; its slots stay on disk");
                    self.mode = Mode::Confirm { label, action: Confirm::RemoveRepo(loc.project) };
                }
            },
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
            Action::LastTab => self.last_tab(),
        }
    }

    /// Open the global config in the editor, in a new "settings" tab here
    /// (creating it from a template first). Closing the editor reloads it.
    /// Replay a pane's saved scrollback (from before the server stopped) above
    /// its fresh screen.
    fn restore_history(&mut self, id: u64, key: Option<&str>) {
        let Some(key) = key else { return };
        let Ok(dir) = session::history_dir() else { return };
        let Ok(text) = std::fs::read_to_string(dir.join(format!("{key}.ansi"))) else { return };
        if let Some(pane) = self.panes.iter_mut().find(|p| p.id == id) {
            pane.restore_history(&text, Some("restored from the last session"));
            // Dirty, so the next save writes it under the pane's new key.
            pane.output_seq += 1;
        }
    }

    /// Save each pane's scrollback that changed since last time (`all`: every
    /// pane, written before returning, for shutdown), and drop files no pane
    /// uses any more. Writes happen off the main thread otherwise.
    pub fn save_histories(&mut self, all: bool) {
        let Ok(dir) = session::history_dir() else { return };
        let mut writes = Vec::new();
        for pane in self.panes.iter_mut().filter(|p| all || p.output_seq != p.dumped_seq) {
            if pane.output_seq == 0 && pane.dumped_seq == 0 {
                continue;
            }
            writes.push((pane.history_key.clone(), pane.history_formatted(true)));
            pane.dumped_seq = pane.output_seq.max(1);
        }
        let keep: std::collections::HashSet<String> = self.panes.iter().filter(|p| p.dumped_seq > 0).map(|p| format!("{}.ansi", p.history_key)).collect();
        if !writes.is_empty() {
            self.save();
        }
        let job = move || {
            for (key, text) in writes {
                let tmp = dir.join(format!("{key}.tmp"));
                if std::fs::write(&tmp, text).is_ok() {
                    let _ = std::fs::rename(&tmp, dir.join(format!("{key}.ansi")));
                }
            }
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let stale_view = name.starts_with("history-")
                        && e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().unwrap_or_default() > Duration::from_secs(3600));
                    if (name.ends_with(".ansi") && !keep.contains(&name)) || stale_view {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        };
        if all {
            job();
        } else {
            std::thread::spawn(job);
        }
    }

    /// Open the focused pane's whole history (scrollback and screen) as text in
    /// your editor, in a new tab that closes with it.
    fn edit_scrollback(&mut self) {
        let Some(loc) = self.current_loc() else { return };
        let Some(pane) = self.focus.and_then(|id| self.panes.iter_mut().find(|p| p.id == id)) else {
            return self.say("no pane to read");
        };
        let text = crate::pane::history_text(&mut pane.parser);
        let name = format!("history-{}.txt", pane.id);
        let Ok(path) = session::history_dir().map(|d| d.join(name)) else { return };
        if let Err(e) = std::fs::write(&path, text) {
            return self.say(format!("can't write {}: {e}", path.display()));
        }
        // A terminal editor ($VISUAL / $EDITOR) first: it runs in the tab.
        let editor = std::env::var("VISUAL")
            .ok()
            .or_else(|| std::env::var("EDITOR").ok())
            .or_else(|| self.projects[loc.project].cfg.editor.clone())
            .unwrap_or_else(|| "vi".into());
        let program = editor.split_whitespace().next().unwrap_or("").rsplit('/').next().unwrap_or("");
        let at_end = if matches!(program, "vi" | "vim" | "nvim") { " +" } else { "" };
        let opts = LaunchOpts { command: Some(format!("exec {editor}{at_end} {}", shell_quote(&path.display().to_string()))), ..LaunchOpts::default() };
        match self.launch(&loc, Kind::Editor, opts) {
            Ok(id) => {
                let mut tab = Tab::new(Layout::Leaf(id), None);
                tab.name = Some("history".into());
                match self.views.get_mut(&loc) {
                    Some(view) => {
                        view.tabs.push(tab);
                        view.set_active(view.tabs.len() - 1);
                    }
                    None => {
                        self.views.insert(loc.clone(), View::single(tab));
                    }
                }
                self.focus = Some(id);
                self.relayout();
            }
            Err(e) => self.say(e),
        }
    }

    /// `velmux doctor` in a new tab here (or in a new scratch session).
    fn open_doctor(&mut self) {
        let loc = match self.current_loc() {
            Some(loc) => loc,
            None => {
                self.new_session();
                match self.current_loc() {
                    Some(loc) => loc,
                    None => return,
                }
            }
        };
        let exe = self.exe.as_ref().map(|(p, _)| p.display().to_string()).unwrap_or_else(|| "velmux".into());
        let opts = LaunchOpts { command: Some(format!("{} doctor", shell_quote(&exe))), ..LaunchOpts::default() };
        match self.launch(&loc, Kind::Shell, opts) {
            Ok(id) => {
                let mut tab = Tab::new(Layout::Leaf(id), None);
                tab.name = Some("doctor".into());
                match self.views.get_mut(&loc) {
                    Some(view) => {
                        view.tabs.push(tab);
                        view.set_active(view.tabs.len() - 1);
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
                        view.set_active(view.tabs.len() - 1);
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
        self.notify = settings.notify.clone();
        self.scratch_dir = settings.scratch_dir.clone();
        let (theme, theme_warnings) = crate::theme::resolve(settings.theme.as_deref(), &settings.theme_colors);
        self.theme = theme;
        self.theme_colors = settings.theme_colors.clone();
        warnings.extend(theme_warnings);
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
                        view.set_active(view.tabs.len() - 1);
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
        view.set_active(index);
        let tab = &view.tabs[index];
        self.focus = tab.last_focus.filter(|f| tab.layout.contains(*f)).or_else(|| tab.layout.leaves().first().copied());
        if self.copy.is_some() {
            self.exit_copy();
        }
        self.full_redraw = true;
    }

    /// Flip back to the tab shown before this one.
    fn last_tab(&mut self) {
        let Some(loc) = self.current_loc() else { return };
        let Some(view) = self.views.get(&loc) else { return };
        match view.previous.filter(|p| *p < view.tabs.len() && *p != view.active) {
            Some(p) => self.goto_tab(p),
            None => self.say("no other tab to flip to"),
        }
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
            Confirm::Release(loc) => self.release_location(loc, None),
            Confirm::GhosttyKeys => match crate::ghostty::install(&crate::ghostty_lines(&self.keymap)) {
                Ok(path) => {
                    let msg = format!("added velmux's keybinds to {} — reload Ghostty's config (⌘⇧,)", path.display());
                    self.toast(&msg);
                    self.say(msg);
                }
                Err(e) => self.say(format!("ghostty: {e:#}")),
            },
            Confirm::DiscardParked { project, branch } => {
                let Some(pool) = self.projects[project].pool.clone() else { return };
                match pool.discard(&branch) {
                    Ok(_) => self.say(format!("discarded the parked changes on {branch} (the branch is kept)")),
                    Err(e) => self.say(format!("discard failed: {e:#}")),
                }
                self.refresh_slots();
            }
            Confirm::RemoveFolder(loc) => {
                self.close_panes_where(|p| p.loc == loc);
                if let (Some(id), Some(project)) = (&loc.slot, self.projects.get_mut(loc.project)) {
                    project.folders.retain(|f| &f.id != id);
                }
                self.views.remove(&loc);
                self.leave(&loc);
                self.say(format!("removed {}", loc.slot.unwrap_or_default()));
            }
            Confirm::RemoveRepo(project) => {
                self.close_panes_where(|p| p.loc.project == project);
                self.views.retain(|l, _| l.project != project);
                self.projects[project].removed = true;
                let name = self.projects[project].name().to_string();
                self.leave(&Loc { project, slot: None });
                self.say(format!("removed {name} from velmux (its slots are untouched; run velmux there to bring it back)"));
            }
        }
    }

    fn close_panes_where(&mut self, pick: impl Fn(&Pane) -> bool) {
        for pane in self.panes.iter_mut().filter(|p| pick(p)) {
            pane.kill();
        }
    }

    /// After removing a place: if you were there, go to the first remaining one.
    fn leave(&mut self, gone: &Loc) {
        let here = self.current_loc();
        let was_here = here.as_ref().is_some_and(|c| c == gone || (gone.slot.is_none() && c.project == gone.project && self.projects[c.project].removed));
        if was_here || self.focused().is_some_and(|p| p.loc == *gone) {
            self.current = None;
            self.focus = None;
            if let Some(loc) = self.live_locations().into_iter().next() {
                self.show_location(loc);
            }
        }
        self.save();
        self.full_redraw = true;
    }

    /// `velmux release` inside a pane: move it back to its repo's base, then
    /// release the slot it was in (other panes there close, work is stashed).
    fn release_for(&mut self, pane: u64) -> String {
        let Some(loc) = self.pane(pane).map(|p| p.loc.clone()) else { return "error: that pane isn't in velmux".into() };
        let Some(slot) = loc.slot.clone().filter(|_| !self.projects[loc.project].is_unclassified()) else {
            return "error: this pane isn't in a slot (nothing to release)".into();
        };
        let session = self.pane(pane).and_then(|p| p.session.clone());
        self.move_pane(pane, Loc { project: loc.project, slot: None });
        self.release_location(loc, session);
        format!("releasing {slot}; this conversation is back in the base checkout")
    }

    /// Move a running pane to another location (its process is untouched).
    fn move_pane(&mut self, id: u64, to: Loc) {
        let Some(from) = self.pane(id).map(|p| p.loc.clone()) else { return };
        if from == to {
            return;
        }
        if let Some(mut view) = self.views.remove(&from) {
            if let Some(t) = view.tab_of(id) {
                let tab = view.tabs.remove(t);
                if let Some(layout) = tab.layout.remove(id) {
                    let last_focus = tab.last_focus.filter(|f| *f != id);
                    view.tabs.insert(t, Tab { layout, zoomed: tab.zoomed.filter(|z| *z != id), last_focus, name: tab.name });
                } else {
                    view.forget_tab(t);
                }
            }
            if !view.tabs.is_empty() {
                view.active = view.active.min(view.tabs.len() - 1);
                self.views.insert(from.clone(), view);
            }
        }
        if let Some(pane) = self.panes.iter_mut().find(|p| p.id == id) {
            pane.loc = to.clone();
        }
        self.insert_into_view(&to, id, None);
        if self.focus == Some(id) {
            self.current = Some(to);
        }
        self.relayout();
    }

    /// Release a slot: close its panes, then stash / reset it in the background.
    /// The Claude sessions that worked there (`first`, the one that claimed it
    /// and is heading back to the base, then any closed here) are kept with
    /// the parked branch for resuming.
    fn release_location(&mut self, loc: Loc, first: Option<String>) {
        let Some(slot) = loc.slot.clone() else { return };
        let Some(pool) = self.projects[loc.project].pool.clone() else { return };
        let mut sessions: Vec<String> = first.into_iter().collect();
        for pane in self.panes.iter_mut().filter(|p| p.loc == loc) {
            if let Some(s) = pane.session.clone().filter(|s| !sessions.contains(s)) {
                sessions.push(s);
            }
            pane.kill();
        }
        let tx = self.events.clone();
        let project = loc.project;
        // A branch whose PR merged has nothing left to resume (unless there
        // were uncommitted changes): don't park it.
        let branch = self.projects[project].slots.iter().find(|s| s.id == slot).and_then(|s| s.branch.clone());
        let merged = branch.as_ref().and_then(|b| self.projects[project].prs.get(b)).is_some_and(|pr| pr.state == velmux_pool::gh::PrState::Merged);
        self.jobs.push((format!("release {slot}"), format!("releasing {slot}")));
        std::thread::spawn(move || {
            // Give killed processes a moment to exit before git touches the tree.
            std::thread::sleep(Duration::from_millis(300));
            let result = pool
                .release_with(&slot, false, &sessions)
                .map(|r| {
                    if let (true, None, Some(b)) = (merged, &r.stashed, &r.branch) {
                        let _ = pool.discard(b);
                    }
                    let mut msg = format!("released {}", r.id);
                    if let Some(b) = r.branch.as_ref().filter(|_| r.stashed.is_some()) {
                        msg = format!("parked {b}");
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

    fn claim(&mut self, branch: Option<String>) {
        let Some(loc) = self.current_loc() else { return };
        self.claim_in(loc.project, branch, None);
    }

    /// Claim a slot in `project` (in the background). With `for_pane`, that pane
    /// moves into the slot and the stream gets its path, instead of a new pane.
    fn claim_in(&mut self, project: usize, branch: Option<String>, for_pane: Option<(u64, UnixStream)>) {
        // Claiming a parked branch yourself resumes it: its Claude comes back too.
        let resume = match (&branch, &for_pane) {
            (Some(b), None) => self.projects[project].parked.iter().find(|p| &p.branch == b).map(|p| p.sessions.clone()).unwrap_or_default(),
            _ => Vec::new(),
        };
        self.claim_resuming(project, branch, for_pane, resume);
    }

    /// [`claim_in`](Self::claim_in), bringing back the Claude `resume` sessions in the slot.
    fn claim_resuming(&mut self, project: usize, branch: Option<String>, for_pane: Option<(u64, UnixStream)>, resume: Vec<String>) {
        let Some(pool) = self.projects[project].pool.clone() else {
            if let Some((_, mut stream)) = for_pane {
                let _ = ServerMsg::Reply("error: this folder isn't a git repo, so it has no slots".into()).write(&mut stream);
            }
            return self.say("scratch sessions aren't git repos, so they have no slots");
        };
        let tx = self.events.clone();
        self.job_seq += 1;
        let job = format!("claim {}", self.job_seq);
        self.jobs.push((job.clone(), format!("claiming {}", branch.as_deref().unwrap_or("a slot"))));
        std::thread::spawn(move || {
            let progress = {
                let (tx, job) = (tx.clone(), job.clone());
                move |text: &str| {
                    let _ = tx.send(Event::Progress { project, job: job.clone(), text: text.to_string() });
                }
            };
            let result = (|| {
                if pool.status()?.is_empty() {
                    pool.init(&|_| {})?;
                }
                let opts = ClaimOpts { branch, by: Some("velmux".into()), fetch: true, install: true };
                pool.claim(&opts, &progress)
            })()
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Claimed { project, job, result, for_pane, resume });
        });
    }

    /// The pane where conversation `id` is running right now (Claude in the
    /// foreground: a pane whose Claude exited keeps the id, but is a shell).
    pub fn live_session_pane(&self, id: &str, project: Option<usize>) -> Option<u64> {
        self.panes
            .iter()
            .find(|p| p.session.as_deref() == Some(id) && p.agent == Some("claude") && project.is_none_or(|i| p.loc.project == i))
            .map(|p| p.id)
    }

    /// A resumed branch's Claude, in its new slot: the conversation itself if
    /// it's still open (back in the base after `velmux release`), moved over;
    /// else the session resumed from its transcript; else a fresh Claude.
    fn resume_sessions(&mut self, loc: Loc, sessions: &[String]) {
        for id in sessions {
            if let Some(pane) = self.live_session_pane(id, Some(loc.project)) {
                self.move_pane(pane, loc.clone());
                self.focus = Some(pane);
                self.current = Some(loc);
                self.full_redraw = true;
                return;
            }
        }
        let resumable = sessions.iter().find(|id| claude::session_exists(&self.claude_dir, id)).cloned();
        let Some(id) = resumable else { return self.spawn(loc, Kind::Agent, None) };
        match self.launch(&loc, Kind::Agent, LaunchOpts { resume: Some(id), cwd: None, command: None }) {
            Ok(pane) => {
                self.insert_into_view(&loc, pane, None);
                self.focus = Some(pane);
                self.current = Some(loc);
                self.relayout();
            }
            Err(e) => self.say(e),
        }
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
                let mut agent = cfg.agent.clone();
                if let (None, Some(pool)) = (&loc.slot, &project.pool) {
                    let _ = std::fs::create_dir_all(&pool.dir);
                    let prompt = BASE_PROMPT.replace("{repo}", &pool.name);
                    agent = format!("{agent} --add-dir {} --append-system-prompt {}", shell_quote(&pool.dir.display().to_string()), shell_quote(&prompt));
                }
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
        // Slots get their own ports (and `VELMUX_SLOT`), so dev servers don't collide.
        let env = match (&project.pool, &loc.slot) {
            (Some(pool), Some(id)) => pool.slot_env(id),
            (Some(pool), None) => vec![("VELMUX_BASE".to_string(), pool.base.to_string_lossy().into_owned())],
            (None, _) => Vec::new(),
        };
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
                        Some(i) => {
                            self.projects[i].removed = false;
                            i
                        }
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
            self.views.insert(loc, View { tabs, active, name: v.name, previous: None });
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
                self.set_clipboard(&text);
                self.exit_copy();
                self.toast("copied to clipboard");
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
                } else {
                    view.forget_tab(t);
                    if view.active >= t && view.active > 0 {
                        view.active -= 1; // the tab closed; stay on its neighbour
                    }
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
