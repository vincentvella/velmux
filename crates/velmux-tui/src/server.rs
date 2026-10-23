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

mod conversations;
mod menus;
mod pointer;
mod prs;
mod slots;
mod workstream;

/// One project's slots, parked work and checkout, as a background refresh found them.
pub struct PoolHealth {
    project: usize,
    slots: Option<Vec<SlotView>>,
    parked: Option<Vec<velmux_pool::ParkedView>>,
    /// Uncommitted changes in the checkout.
    base_changes: usize,
    /// What fast-forwarding the checkout did, after a fetch.
    base_update: Option<velmux_pool::BaseUpdate>,
}

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
    /// An update check came back (`asked`: you asked, so say "up to date" too).
    UpdateChecked { result: Result<crate::update::Latest, String>, asked: bool },
    /// Swapping in a downloaded update finished.
    Updated { job: String, result: Result<String, String> },
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
    /// Uncommitted changes in the checkout (work that belongs in a slot).
    pub base_changes: usize,
    /// The last thing said about fast-forwarding the checkout, so it isn't repeated.
    base_said: Option<velmux_pool::BaseUpdate>,
    /// Unclassified only: its folders, addressed by id in `Loc::slot`.
    pub folders: Vec<Folder>,
    /// Removed from the sidebar. Kept in place (locations refer to projects by
    /// index), skipped everywhere and not saved; opening it again brings it back.
    pub removed: bool,
}

pub struct Folder {
    pub id: String,
    pub path: PathBuf,
    /// A workstream: slots from one or more repos, worked on together from
    /// this place (its folder links to each). `None`: a scratch session.
    pub members: Option<Vec<session::Member>>,
}

impl Project {
    fn repo(pool: Pool) -> Project {
        // Worktrees another tool laid out beside the repo become its slots.
        let _ = pool.adopt(&|_| {});
        Project { cfg: pool.cfg.clone(), pool: Some(Arc::new(pool)), slots: Vec::new(), parked: Vec::new(), prs: HashMap::new(), base_changes: 0, base_said: None, folders: Vec::new(), removed: false }
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

/// Let go of the terminal the server was started from (the first client's).
/// Kept, a program a pane or an install runs that takes its terminal for job
/// control (an interactive shell) could take yours, and stop the client
/// you're using (`suspended (tty input)`).
fn detach_terminal() {
    if let Ok(tty) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty") {
        use std::os::fd::AsRawFd;
        unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCNOTTY) };
    }
}

/// How long after a resize its keys work again without the prefix.
const REPEAT_FOR: Duration = Duration::from_millis(800);

/// Keys you press several times in a row: resizing splits and the sidebar.
fn repeatable(action: Action) -> bool {
    matches!(action, Action::ResizeLeft | Action::ResizeRight | Action::ResizeUp | Action::ResizeDown | Action::SidebarWider | Action::SidebarNarrower)
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
    /// Choosing sounds: a column each for finished / blocked (`col` 0 / 1),
    /// the highlighted one played as you land on it; nothing changes until enter.
    SoundPicker { col: usize, done: usize, blocked: usize },
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
    /// The pane it's for (a right-click on a pane): focused before its action runs.
    pub pane: Option<u64>,
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
    /// Resizing the sidebar by its edge.
    SidebarEdge,
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
    /// A repo's slot: dropped on a workstream it joins it; on the repos, it leaves its own.
    Slot(Loc),
}

/// Overrides for a new pane (restore and import).
#[derive(Default)]
struct LaunchOpts {
    /// A session to resume (a reference: it names its agent).
    resume: Option<String>,
    cwd: Option<PathBuf>,
    command: Option<String>,
    /// For an agent pane: which agent (else the configured one).
    agent: Option<crate::agents::Agent>,
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
    /// A branch for the checkout's changes, in this project.
    MoveToSlot(usize),
    RenameTab,
    RenameLocation,
    /// A name for a new workstream, and the slot to put in it (if any).
    NewWorkstream(Option<Loc>),
    /// A branch for a slot in this repo, claimed into this workstream.
    ClaimInto { ws: Loc, project: usize },
}

pub enum Confirm {
    Release(Loc),
    GhosttyKeys,
    DiscardParked { project: usize, branch: String },
    RemoveFolder(Loc),
    RemoveRepo(usize),
    ReleaseWorkstream(Loc),
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
    /// The names velmux gave conversations (session → name), so one you've
    /// renamed yourself is left alone. Kept in `state/conversation-names.json`.
    named: HashMap<String, String>,
    /// Until when a repeatable key (resizing) works again without the prefix.
    repeat_until: Option<Instant>,
    /// Claims (by job) whose slot goes into a workstream instead of moving the pane.
    claim_into: HashMap<String, Loc>,
    /// Menu labels made at runtime (workstream names), kept for the menus' `&'static str`.
    interned: HashMap<String, &'static str>,
    /// Where the menu that's running an item was, for a menu it opens.
    menu_at: Option<(u16, u16)>,
    /// A selection being dragged at a pane's edge: which way it scrolls, the
    /// pointer's cell, and when it last scrolled.
    select_edge: Option<(isize, u16, u16, Instant)>,
    pub keymap: Keymap,
    pub theme: crate::theme::Theme,
    /// Per-role overrides from config, re-applied when the theme changes.
    theme_colors: std::collections::BTreeMap<String, String>,
    pub sidebar_hidden: bool,
    /// The width you've set the sidebar to (dragging its edge, `<prefix> <` / `>`).
    pub sidebar_cols: Option<u16>,
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
    /// Check for a newer velmux; the newest one found, if it's newer.
    update_check: bool,
    pub update: Option<crate::update::Latest>,
    update_checked: Option<Instant>,
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
    /// The client's terminal window has focus (it reports focus in / out).
    terminal_focused: bool,
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
    /// Focus the pane under the pointer as it moves.
    focus_follows_mouse: bool,
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
    detach_terminal();
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
                    | ClientMsg::ReleaseFor { .. }
                    | ClientMsg::ReportSession { .. }
                    | ClientMsg::ClaimInto { .. } => Event::Control(stream, first),
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
                focus_follows_mouse: false,
                auto_restart: true,
                keys: Default::default(),
                notify: Default::default(),
                theme: None,
                theme_colors: Default::default(),
                scratch_dir: std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default(),
                update_check: true,
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
    app.focus_follows_mouse = settings.focus_follows_mouse;
    app.auto_restart = settings.auto_restart;
    app.notify = settings.notify.clone();
    app.scratch_dir = settings.scratch_dir.clone();
    app.update_check = settings.update_check;
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
            app.check_update_bg();
            app.check_binary();
            last_slots = Instant::now();
        }
        app.finish_nudge();
        app.autoscroll_selection();
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
            repeat_until: None,
            claim_into: HashMap::new(),
            interned: HashMap::new(),
            menu_at: None,
            select_edge: None,
            named: crate::paths::state_dir().ok().and_then(|d| std::fs::read_to_string(d.join("conversation-names.json")).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default(),
            keymap,
            theme: crate::theme::builtin("default").unwrap(),
            theme_colors: Default::default(),
            sidebar_hidden: false,
            sidebar_cols: None,
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
            update_check: true,
            update: None,
            update_checked: None,
            checking_prs: false,
            prs_checked: None,
            status: None,
            size: (80, 24),
            size_known: false,
            nudge: None,
            terminal_focused: true,
            adopted: false,
            events,
            client: None,
            next_conn: 0,
            next_pane: 1,
            prev: Buffer::empty(Rect::new(0, 0, 80, 24)),
            copy: None,
            pending_out: Vec::new(),
            mouse,
            focus_follows_mouse: false,
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
        let body = ui::pane_area(self.size.0, self.size.1, self.sidebar_hidden, self.sidebar_cols);
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
        self.projects.push(Project { pool: None, cfg, slots: Vec::new(), parked: Vec::new(), prs: HashMap::new(), base_changes: 0, base_said: None, folders: Vec::new(), removed: false });
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
        folders.push(Folder { id: id.clone(), path: dir, members: None });
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
        folders.push(Folder { id: id.clone(), path: path.to_path_buf(), members: None });
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
                    ClientMsg::ClaimInto { pane, branch, repo, workstream } => {
                        return self.claim_into_for(pane, branch, repo, workstream, stream);
                    }
                    ClientMsg::ReleaseFor { pane } => self.release_for(pane),
                    ClientMsg::ReportSession { pane, session } => match self.panes.iter_mut().find(|p| p.id == pane) {
                        Some(p) => {
                            p.session = Some(session);
                            self.save();
                            "ok".into()
                        }
                        None => "error: that pane isn't in velmux".into(),
                    },
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
                    // Claimed from a workstream's menu: the slot joins it.
                    (Ok(slot), None) if self.claim_into.contains_key(&job) => {
                        let ws = self.claim_into.remove(&job).unwrap();
                        let loc = Loc { project, slot: Some(slot.id.clone()) };
                        self.add_to_workstream(&ws, &loc);
                        let branch = slot.branch.clone().map(|b| format!(" on {b}")).unwrap_or_default();
                        self.say(format!("claimed {}{branch} into {}", slot.id, ws.slot.clone().unwrap_or_default()));
                    }
                    (Ok(slot), None) if !resume.is_empty() => {
                        let loc = Loc { project, slot: Some(slot.id.clone()) };
                        let branch = slot.branch.clone().unwrap_or_default();
                        self.say(format!("resumed {branch} in {}", slot.id));
                        self.resume_sessions(loc, &resume);
                    }
                    (Ok(slot), None) => {
                        // Claimed by hand: a shell to work in (new Claudes start
                        // in the checkout and claim their own slot).
                        let loc = Loc { project, slot: Some(slot.id.clone()) };
                        self.say(format!("claimed {}", slot.id));
                        self.spawn(loc, Kind::Shell, None);
                    }
                    (Ok(slot), Some((_, mut stream))) if self.claim_into.contains_key(&job) => {
                        // For a workstream: the slot joins it; the pane stays put.
                        let ws = self.claim_into.remove(&job).unwrap();
                        let loc = Loc { project, slot: Some(slot.id.clone()) };
                        self.add_to_workstream(&ws, &loc);
                        let branch = slot.branch.clone().map(|b| format!(" on {b}")).unwrap_or_default();
                        let name = ws.slot.clone().unwrap_or_default();
                        self.say(format!("claimed {}{branch} into {name}", slot.id));
                        let _ = ServerMsg::Reply(format!("{}\n{name}", slot.path.display())).write(&mut stream);
                    }
                    (Ok(slot), Some((pane, mut stream))) => {
                        let loc = Loc { project, slot: Some(slot.id.clone()) };
                        self.move_pane(pane, loc);
                        let branch = slot.branch.clone().map(|b| format!(" on {b}")).unwrap_or_default();
                        self.say(format!("claimed {}{branch}", slot.id));
                        let _ = ServerMsg::Reply(slot.path.display().to_string()).write(&mut stream);
                    }
                    (Err(e), None) => {
                        self.claim_into.remove(&job);
                        self.say(format!("claim failed: {e}"));
                    }
                    (Err(e), Some((_, mut stream))) => {
                        self.claim_into.remove(&job);
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
                for h in fresh {
                    let Some(project) = self.projects.get_mut(h.project) else { continue };
                    if let Some(slots) = h.slots {
                        project.slots = slots;
                    }
                    if let Some(parked) = h.parked {
                        project.parked = parked;
                    }
                    project.base_changes = h.base_changes;
                    if let Some(update) = h.base_update {
                        self.base_updated(h.project, update);
                    }
                }
                self.name_conversations();
                self.prune_workstreams();
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
            Event::UpdateChecked { result, asked } => match result {
                Ok(latest) if crate::update::newer(&latest.version, crate::update::CURRENT) => {
                    let fresh = crate::update::announced().as_deref() != Some(latest.version.as_str());
                    if fresh || asked {
                        let msg = format!("velmux {} is out (you have {}): menu › update velmux", latest.version, crate::update::CURRENT);
                        self.toast(&msg);
                        self.say(msg.clone());
                        if fresh && !asked {
                            self.notify_text(&msg, true);
                        }
                        crate::update::mark_announced(&latest.version);
                    }
                    self.update = Some(latest);
                    self.dirty = true;
                }
                Ok(_) => {
                    self.update = None;
                    if asked {
                        self.toast(&format!("velmux {} is the newest", crate::update::CURRENT));
                    }
                }
                Err(e) if asked => self.say(format!("couldn't check for updates: {e}")),
                Err(_) => {}
            },
            Event::Updated { job, result } => {
                self.jobs.retain(|(k, _)| k != &job);
                match result {
                    Ok(version) if self.auto_restart => self.toast(&format!("updated to {version}: restarting into it")),
                    Ok(version) => self.say(format!("updated to {version}: restart velmux (<prefix> ctrl+r) to use it")),
                    Err(e) => self.say(format!("update failed: {e}")),
                }
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
                    self.pending_out.extend_from_slice(self.mouse_mode());
                }
                // Focus reporting: whether you're looking at this terminal at all.
                self.pending_out.extend_from_slice(b"\x1b[?1004h");
                self.terminal_focused = true;
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
            ClientMsg::ImportHerdr { .. } | ClientMsg::Restart | ClientMsg::Stop | ClientMsg::Debug | ClientMsg::ClaimFor { .. } | ClientMsg::ReleaseFor { .. } | ClientMsg::ReportSession { .. } | ClientMsg::ClaimInto { .. } => {}
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.size = (cols.max(20), rows.max(5));
        self.size_known = true;
        self.relayout();
    }

    /// Size every pane to its rect in its location's layout.
    /// Set the sidebar to `cols` wide (kept within bounds) and fit the panes.
    fn set_sidebar_width(&mut self, cols: u16) {
        let width = ui::sidebar_width(self.size.0, false, Some(cols));
        if self.sidebar_cols != Some(width) {
            self.sidebar_cols = Some(width);
            self.full_redraw = true;
            self.relayout();
        }
    }

    fn relayout(&mut self) {
        self.reveal_focus();
        let body = ui::pane_area(self.size.0, self.size.1, self.sidebar_hidden, self.sidebar_cols);
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
            sidebar_width: self.sidebar_cols,
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
                .map(|f| SavedFolder { id: f.id.clone(), path: f.path.clone(), members: f.members.clone() })
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
        self.sidebar_cols = saved.sidebar_width;
        let env = Env::from_process().ok();
        for base in &saved.projects {
            let Some(pool) = env.clone().and_then(|e| Pool::discover(e, base).ok()) else { continue };
            if !self.projects.iter().any(|p| p.base() == Some(pool.base.as_path())) {
                self.projects.push(Project::repo(pool));
            }
        }
        for f in &saved.folders {
            if f.path.is_dir() {
                let loc = self.folder_loc(&f.id, &f.path);
                if let Some(folder) = self.folder_mut(&loc) {
                    folder.members = f.members.clone();
                }
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
                        .filter(|id| crate::agents::exists(id, &env.claude_dir) && !taken.contains(id)),
                    _ => None,
                };
                if let Some(id) = &resume {
                    taken.push(id.clone());
                }
                let resuming = resume.is_some();
                let opts = LaunchOpts { resume, cwd: p.cwd, command: p.command, agent: None };
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
                    let fetched = fetch && pool.fetch().is_ok();
                    let base_update = (fetched && pool.cfg.fast_forward).then(|| pool.update_base());
                    PoolHealth { project: i, slots: pool.status().ok(), parked: pool.parked().ok(), base_changes: pool.base_changes().len(), base_update }
                })
                .collect();
            let _ = tx.send(Event::Pools { gen, fresh });
        });
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
        let body = ui::pane_area(self.size.0, self.size.1, self.sidebar_hidden, self.sidebar_cols);
        let Some(pane) = self.focus.and_then(|id| self.panes.iter_mut().find(|p| p.id == id)) else {
            return "no focused pane".into();
        };
        let (rows, cols) = pane.parser.screen().size();
        let pty = pane.pty_size();
        let mut out = format!(
            "pane {} · client {}x{} · pane area {}x{} at col {} · parser {}x{} · pty {} · alt screen {} · mouse {:?} · scrollback {}\n",
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
            pane.parser.screen().mouse_protocol_mode(),
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
        let watching = self.watching(id);
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

    /// You're looking at pane `id`: it's focused, a client is attached, and
    /// its terminal window has focus.
    fn watching(&self, id: u64) -> bool {
        self.client.is_some() && self.terminal_focused && self.focus == Some(id)
    }

    fn detect_all(&mut self) {
        let focus = self.focus.filter(|_| self.client.is_some() && self.terminal_focused);
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
        // Focus reports (ESC [ I / ESC [ O) from the terminal: noted, not passed on.
        let mut i = 0;
        while i + 2 < data.len() {
            if data[i] == 0x1b && data[i + 1] == b'[' && matches!(data[i + 2], b'I' | b'O') {
                self.terminal_focused = data[i + 2] == b'I';
                data.drain(i..i + 3);
                self.dirty = true;
            } else {
                i += 1;
            }
        }
        if data.is_empty() {
            return;
        }
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

    /// Where a new agent starts for `loc`: a repo's checkout, even from one of
    /// its slots (it plans there and claims a slot of its own); a folder, there.
    fn agent_loc(&self, loc: Loc) -> Loc {
        if self.projects[loc.project].is_unclassified() {
            loc
        } else {
            Loc { slot: None, ..loc }
        }
    }

    /// Show a place: its active tab, focused where you left it.
    fn go_to(&mut self, loc: Loc) {
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
            // Just after a resize, its keys repeat without the prefix
            // (`<prefix> > > >`), like tmux; any other key ends that.
            if let Some(until) = self.repeat_until.take() {
                if let (Mode::Normal, true) = (&self.mode, Instant::now() < until) {
                    if let Some((action, len)) = self.keymap.after_prefix(bytes).filter(|(a, _)| repeatable(*a)) {
                        self.run(action);
                        self.repeat_until = Some(Instant::now() + REPEAT_FOR);
                        bytes = &bytes[len.min(bytes.len())..];
                        self.dirty = true;
                        continue;
                    }
                }
            }
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
                            if repeatable(action) {
                                self.repeat_until = Some(Instant::now() + REPEAT_FOR);
                            }
                            len
                        }
                        None => unbound_len(bytes),
                    }
                }
                Mode::Prompt { .. } => self.prompt_input(bytes),
                Mode::Picker(_) => self.picker_input(bytes),
                Mode::Sessions { .. } => self.sessions_input(bytes),
                Mode::SoundPicker { .. } => self.sound_input(bytes),
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
                PromptFor::MoveToSlot(project) => {
                    let Some(pool) = self.projects[project].pool.clone() else { return bytes.len() };
                    if answer.is_empty() {
                        self.say("a branch name is needed to move the changes to");
                        return bytes.len();
                    }
                    match pool.stash_base_for(&answer) {
                        Ok(()) => {
                            self.projects[project].base_changes = 0;
                            self.say(format!("moving the checkout's changes to {answer}…"));
                            self.claim_in(project, Some(answer), None);
                        }
                        Err(e) => self.say(format!("couldn't move them: {e:#}")),
                    }
                }
                PromptFor::ClaimInto { ws, project } => {
                    if let Some(job) = self.claim_in(project, (!answer.is_empty()).then_some(answer), None) {
                        self.claim_into.insert(job, ws);
                    }
                }
                PromptFor::NewWorkstream(slot) => match self.new_workstream(&answer) {
                    Some(ws) => {
                        if let Some(slot) = slot {
                            self.add_to_workstream(&ws, &slot);
                        }
                        self.go_to(ws);
                    }
                    None => self.say("a workstream needs a name"),
                },
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
                    self.name_conversations();
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

    fn sound_input(&mut self, bytes: &[u8]) -> usize {
        use crate::notify::{play, set_notify_key, BLOCKED_SOUNDS, DONE_SOUNDS};
        let Mode::SoundPicker { col, done, blocked } = &mut self.mode else { return bytes.len() };
        let (used, key) = match bytes {
            [0x1b, b'[', b'A', ..] => (3, b'k'),
            [0x1b, b'[', b'B', ..] => (3, b'j'),
            [0x1b, b'[', b'C', ..] => (3, b'l'),
            [0x1b, b'[', b'D', ..] => (3, b'h'),
            [0x1b, b'[', rest @ ..] => {
                let len = rest.iter().position(|b| (0x40..=0x7e).contains(b)).map_or(bytes.len(), |i| i + 3);
                (len, 0)
            }
            [b, ..] => (1, *b),
            [] => return 0,
        };
        let (list, at) = if *col == 0 { (DONE_SOUNDS, &mut *done) } else { (BLOCKED_SOUNDS, &mut *blocked) };
        let n = list.len();
        match key {
            b'k' | 0x10 | b'j' | 0x0e => {
                *at = if matches!(key, b'k' | 0x10) { (*at + n - 1) % n } else { (*at + 1) % n };
                play(list[*at]);
            }
            b'h' | b'l' | b'\t' => {
                *col = 1 - *col;
                let (list, at) = if *col == 0 { (DONE_SOUNDS, *done) } else { (BLOCKED_SOUNDS, *blocked) };
                play(list[at]);
            }
            b' ' => play(list[*at]),
            b'\r' | b'\n' => {
                let (d, b) = (DONE_SOUNDS[*done], BLOCKED_SOUNDS[*blocked]);
                self.mode = Mode::Normal;
                self.notify.done_sound = d.to_string();
                self.notify.blocked_sound = b.to_string();
                let saved = Env::from_process().and_then(|env| {
                    let text = std::fs::read_to_string(&env.global_config).unwrap_or_default();
                    let text = set_notify_key(&set_notify_key(&text, "done_sound", d), "blocked_sound", b);
                    if let Some(dir) = env.global_config.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    std::fs::write(&env.global_config, text)?;
                    Ok(())
                });
                match saved {
                    Ok(()) => self.toast(&format!("sounds: {d} · {b}")),
                    Err(e) => self.say(format!("sounds set, but saving them failed: {e:#}")),
                }
            }
            0x1b | 0x03 | b'q' => self.mode = Mode::Normal,
            _ => {}
        }
        self.dirty = true;
        used
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
                    let loc = self.agent_loc(loc);
                    self.spawn(loc, Kind::Agent, None);
                }
            }
            Action::AgentPicker => {
                let Some(target) = self.current_loc() else { return self.say("go to a repo or session first") };
                let items: Vec<(&'static str, Action)> = crate::agents::ALL
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| a.installed())
                    .map(|(i, a)| (a.label(), Action::AgentWith(i as u8)))
                    .collect();
                if items.is_empty() {
                    return self.say("no coding agent found on PATH (claude, codex, gemini, opencode)");
                }
                let side = ui::sidebar_width(self.size.0, self.sidebar_hidden, self.sidebar_cols);
                self.mode = Mode::Menu {
                    selected: 0,
                    context: Some(MenuContext { pane: None, target, parked: None, title: "new conversation with…".into(), items, at: self.menu_at.take().unwrap_or((side + 2, 2)) }),
                };
            }
            Action::AgentWith(n) => {
                let Some(agent) = crate::agents::ALL.get(n as usize).copied() else { return };
                if let Some(loc) = self.current_loc() {
                    let loc = self.agent_loc(loc);
                    self.spawn_with(loc, Kind::Agent, None, LaunchOpts { agent: Some(agent), ..LaunchOpts::default() });
                }
            }
            // `u8::MAX`: pick a workstream for the slot here (a menu of them).
            Action::AddToWorkstream(u8::MAX) => {
                let Some(target) = self.current_loc() else { return };
                let mut items: Vec<(&'static str, Action)> = Vec::new();
                for (i, ws) in self.workstreams().iter().enumerate().take(200) {
                    let name = self.intern(ws.slot.as_deref().unwrap_or_default());
                    items.push((name, Action::AddToWorkstream(i as u8)));
                }
                items.push(("new workstream…", Action::NewWorkstream));
                let side = ui::sidebar_width(self.size.0, self.sidebar_hidden, self.sidebar_cols);
                self.mode = Mode::Menu {
                    selected: 0,
                    context: Some(MenuContext { pane: None, target, parked: None, title: "add to workstream…".into(), items, at: self.menu_at.take().unwrap_or((side + 2, 2)) }),
                };
            }
            Action::AddToWorkstream(n) => {
                let (Some(slot), Some(ws)) = (self.current_loc(), self.workstreams().get(n as usize).cloned()) else { return };
                if slot.slot.is_some() && !self.projects[slot.project].is_unclassified() {
                    self.add_to_workstream(&ws, &slot);
                    self.toast(&format!("added to {}", ws.slot.unwrap_or_default()));
                }
            }
            Action::NewWorkstream => {
                // From a repo's slot, it starts the workstream with it.
                let slot = self.current_loc().filter(|l| l.slot.is_some() && !self.projects[l.project].is_unclassified());
                self.mode = Mode::Prompt { label: "name for the workstream".into(), input: String::new(), purpose: PromptFor::NewWorkstream(slot) };
            }
            // Which repo: the only one, or a menu of them.
            Action::ClaimIntoWorkstream(u8::MAX) => {
                let Some(ws) = self.current_loc().filter(|l| self.is_workstream(l)) else { return self.say("go to a workstream first") };
                let repos: Vec<usize> = self.ordered_projects().into_iter().filter(|&p| !self.projects[p].is_unclassified()).collect();
                match repos.len() {
                    0 => self.say("add a repo first (⊕ new space)"),
                    1 => self.run(Action::ClaimIntoWorkstream(0)),
                    _ => {
                        let mut items: Vec<(&'static str, Action)> = Vec::new();
                        for (i, &p) in repos.iter().enumerate().take(200) {
                            let name = self.projects[p].name().to_string();
                            items.push((self.intern(&name), Action::ClaimIntoWorkstream(i as u8)));
                        }
                        let side = ui::sidebar_width(self.size.0, self.sidebar_hidden, self.sidebar_cols);
                        self.mode = Mode::Menu {
                            selected: 0,
                            context: Some(MenuContext { pane: None, target: ws, parked: None, title: "a slot in…".into(), items, at: self.menu_at.take().unwrap_or((side + 2, 2)) }),
                        };
                    }
                }
            }
            Action::ClaimIntoWorkstream(n) => {
                let Some(ws) = self.current_loc().filter(|l| self.is_workstream(l)) else { return };
                let repos: Vec<usize> = self.ordered_projects().into_iter().filter(|&p| !self.projects[p].is_unclassified()).collect();
                let Some(&project) = repos.get(n as usize) else { return };
                let label = format!("branch for a slot in {} (into {})", self.projects[project].name(), ws.slot.clone().unwrap_or_default());
                self.mode = Mode::Prompt { label, input: String::new(), purpose: PromptFor::ClaimInto { ws, project } };
            }
            Action::OpenWorkstreamPrs => {
                let Some(ws) = self.current_loc().filter(|l| self.is_workstream(l)) else { return };
                let urls: Vec<String> = self.workstream_prs(&ws).iter().map(|p| p.url.clone()).filter(|u| !u.is_empty() && !u.chars().any(char::is_control)).collect();
                for url in &urls {
                    self.open_url(url);
                }
                self.toast(&format!("opened {} PR(s)", urls.len()));
            }
            Action::LeaveWorkstream => {
                if let Some(slot) = self.current_loc() {
                    self.remove_from_workstream(&slot);
                }
            }
            Action::ReleaseWorkstream => match self.current_loc().filter(|l| self.is_workstream(l)) {
                Some(ws) => {
                    let n = self.members(&ws).len();
                    let label = format!("release {} and its {n} slot(s)? unfinished work is parked", ws.slot.clone().unwrap_or_default());
                    self.mode = Mode::Confirm { label, action: Confirm::ReleaseWorkstream(ws) };
                }
                None => self.say("go to a workstream to release it"),
            },
            Action::AgentHere => {
                if let Some(loc) = self.current_loc() {
                    let loc = self.agent_loc(loc);
                    self.spawn(loc, Kind::Agent, None);
                }
            }
            Action::SyncSlot => self.sync_slot(),
            Action::Update => self.update_velmux(),
            Action::SyncAll => self.sync_all(),
            Action::MoveToSlot => match self.current_loc() {
                Some(loc) if self.projects[loc.project].pool.is_some() => {
                    let n = self.projects[loc.project].base_changes;
                    self.mode = Mode::Prompt {
                        label: format!("branch for the checkout's {n} changed file(s)"),
                        input: String::new(),
                        purpose: PromptFor::MoveToSlot(loc.project),
                    };
                }
                _ => self.say("go to a repo to move its checkout's changes"),
            },
            Action::SoundPicker => {
                use crate::notify::{BLOCKED_SOUNDS, DONE_SOUNDS};
                let (d, b) = (self.notify.done_sound.clone(), self.notify.blocked_sound.clone());
                let at = |list: &[&str], s: &str| list.iter().position(|n| *n == s || (s.is_empty() && *n == "none")).unwrap_or(0);
                self.mode = Mode::SoundPicker { col: 0, done: at(DONE_SOUNDS, &d), blocked: at(BLOCKED_SOUNDS, &b) };
            }
            Action::TestSounds => {
                let (done, blocked) = (self.notify.done_sound.clone(), self.notify.blocked_sound.clone());
                self.toast("finished… then blocked on you");
                std::thread::spawn(move || {
                    crate::notify::play(&done);
                    std::thread::sleep(Duration::from_millis(1100));
                    crate::notify::play(&blocked);
                });
            }
            Action::InstallSkill => match crate::skill::install(&crate::skill::user_path(&self.claude_dir)) {
                Ok(_) => {
                    let (_, problems) = crate::skill::install_others();
                    match problems.first() {
                        Some(p) => self.say(format!("skill installed, but {p}")),
                        None => self.toast("Claude skill installed: new Claude sessions know velmux"),
                    }
                }
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
                        "release {}? its panes move back to the checkout; uncommitted work is stashed",
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
            Action::SidebarWider | Action::SidebarNarrower => {
                let now = ui::sidebar_width(self.size.0, false, self.sidebar_cols);
                let to = if action == Action::SidebarWider { now + 4 } else { now.saturating_sub(4) };
                self.sidebar_hidden = false;
                self.set_sidebar_width(to);
                self.save();
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

    /// What velmux adds when starting `agent` in `loc`: in a repo's checkout,
    /// its instructions and the slots' folder; in a repo or its slots, the
    /// checkout guard, for the agents that have one.
    fn agent_context(&self, loc: &Loc, agent: crate::agents::Agent) -> crate::agents::Context {
        use crate::agents::Agent;
        let project = &self.projects[loc.project];
        let mut ctx = crate::agents::Context { claude_dir: self.claude_dir.clone(), ..Default::default() };
        // A workstream: every slot in it in reach, and how to work there.
        if self.is_workstream(loc) {
            let name = loc.slot.clone().unwrap_or_default();
            let prompt = workstream::PROMPT.replace("{name}", &name);
            ctx.pool_dirs = self.members(loc).iter().filter_map(|m| self.loc_cwd(m)).collect();
            if agent == Agent::Opencode {
                ctx.prompt_file = crate::paths::state_dir().ok().map(|d| d.join("instructions").join(format!("workstream-{name}.md"))).filter(|f| {
                    std::fs::create_dir_all(f.parent().unwrap()).is_ok() && std::fs::write(f, &prompt).is_ok()
                });
            }
            ctx.prompt = Some(prompt);
            return ctx;
        }
        let Some(pool) = project.pool.as_ref() else { return ctx };
        if loc.slot.is_none() {
            let _ = std::fs::create_dir_all(&pool.dir);
            let prompt = BASE_PROMPT.replace("{repo}", &pool.name);
            if agent == Agent::Opencode {
                ctx.prompt_file = crate::paths::state_dir().ok().map(|d| d.join("instructions").join(format!("{}.md", velmux_pool::claude::sanitize(&pool.base)))).filter(|f| {
                    std::fs::create_dir_all(f.parent().unwrap()).is_ok() && std::fs::write(f, &prompt).is_ok()
                });
            }
            ctx.pool_dirs = pool.slot_dirs();
            ctx.prompt = Some(prompt);
        }
        if project.cfg.guard {
            match agent {
                Agent::Claude => ctx.claude_settings = self.guard_settings(pool),
                Agent::Gemini => ctx.gemini_policy = self.gemini_policy(pool),
                _ => {}
            }
        }
        ctx
    }

    /// Gemini CLI's version of the checkout guard: a policy denying its file
    /// writes under the checkout (unless the pool lives inside it).
    fn gemini_policy(&self, pool: &Pool) -> Option<PathBuf> {
        if pool.dir.starts_with(&pool.base) {
            return None;
        }
        let base = pool.base.display().to_string();
        let escaped: String = base.chars().flat_map(|c| if r"\.^$|?*+()[]{}".contains(c) { vec!['\\', c] } else { vec![c] }).collect();
        let text = format!(
            "# velmux: keep edits out of {base}; agents work in slots (velmux claim <branch>).\n[[rule]]\ntoolName = [\"write_file\", \"replace\"]\nargsPattern = '\"file_path\":\"{escaped}/'\ndecision = \"deny\"\npriority = 900\ndenyMessage = \"This is the repo's checkout, which velmux keeps clean: run `velmux claim <branch>` and make the edit in the slot it prints.\"\n"
        );
        let path = crate::paths::state_dir().ok()?.join("guard").join(format!("{}.gemini.toml", velmux_pool::claude::sanitize(&pool.base)));
        if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            std::fs::create_dir_all(path.parent()?).ok()?;
            std::fs::write(&path, text).ok()?;
        }
        Some(path)
    }

    /// The `--settings` file keeping Claudes in `pool`'s repo out of its
    /// checkout (see [`crate::guard`]): one per repo, in the state dir.
    fn guard_settings(&self, pool: &Pool) -> Option<PathBuf> {
        let exe = self.exe.as_ref().map(|(p, _)| p.clone()).or_else(|| std::env::current_exe().ok())?;
        let text = serde_json::to_string_pretty(&crate::guard::settings(&exe, &pool.base, &pool.dir)).ok()?;
        let path = crate::paths::state_dir().ok()?.join("guard").join(format!("{}.json", velmux_pool::claude::sanitize(&pool.base)));
        if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            std::fs::create_dir_all(path.parent()?).ok()?;
            std::fs::write(&path, text).ok()?;
        }
        Some(path)
    }

    /// `velmux doctor` in a new tab here (or in a new scratch session).
    /// At startup, then twice a day: is there a newer velmux? (Not for builds
    /// from source, which update with git.)
    fn check_update_bg(&mut self) {
        if !self.update_check || self.update_checked.is_some_and(|t| t.elapsed() < Duration::from_secs(12 * 3600)) {
            return;
        }
        self.update_checked = Some(Instant::now());
        if crate::update::Install::detect() == crate::update::Install::Dev {
            return;
        }
        self.check_update(false);
    }

    fn check_update(&mut self, asked: bool) {
        let tx = self.events.clone();
        std::thread::spawn(move || {
            let result = crate::update::fetch().map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::UpdateChecked { result, asked });
        });
    }

    /// "update velmux": check if nothing's known yet; else update the way it
    /// was installed.
    fn update_velmux(&mut self) {
        use crate::update::Install;
        let Some(latest) = self.update.clone() else {
            self.toast("checking for updates…");
            return self.check_update(true);
        };
        match Install::detect() {
            Install::Dev => self.say("this velmux is built from source: update it with git pull and cargo build --release"),
            Install::Binary(path) => {
                self.job_seq += 1;
                let job = format!("update {}", self.job_seq);
                self.jobs.push((job.clone(), format!("updating to {}", latest.version)));
                let tx = self.events.clone();
                std::thread::spawn(move || {
                    let result = crate::update::apply(&latest, &path).map(|_| latest.version.clone()).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Event::Updated { job, result });
                });
            }
            install => {
                if let Some(cmd) = install.command() {
                    self.open_command_tab("update", cmd);
                }
            }
        }
    }

    fn open_doctor(&mut self) {
        let exe = self.exe.as_ref().map(|(p, _)| p.display().to_string()).unwrap_or_else(|| "velmux".into());
        self.open_command_tab("doctor", &format!("{} doctor", shell_quote(&exe)));
    }

    /// `command` in a new tab named `name`, here (or in a new scratch session).
    fn open_command_tab(&mut self, name: &str, command: &str) {
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
        let opts = LaunchOpts { command: Some(command.to_string()), ..LaunchOpts::default() };
        match self.launch(&loc, Kind::Shell, opts) {
            Ok(id) => {
                let mut tab = Tab::new(Layout::Leaf(id), None);
                tab.name = Some(name.into());
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
        self.update_check = settings.update_check;
        let (theme, theme_warnings) = crate::theme::resolve(settings.theme.as_deref(), &settings.theme_colors);
        self.theme = theme;
        self.theme_colors = settings.theme_colors.clone();
        warnings.extend(theme_warnings);
        if settings.mouse != self.mouse || settings.focus_follows_mouse != self.focus_follows_mouse {
            self.mouse = settings.mouse;
            self.focus_follows_mouse = settings.focus_follows_mouse;
            self.pending_out.extend_from_slice(mouse::DISABLE);
            if self.mouse {
                self.pending_out.extend_from_slice(self.mouse_mode());
            }
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

    /// The previous / next place in the sidebar, as it lists them (bases,
    /// claimed slots, folders), whether or not it has panes open.
    fn step_location(&mut self, step: isize) {
        let locs: Vec<Loc> = ui::display_rows(self)
            .into_iter()
            .filter_map(|r| match r {
                ui::Row::Location(loc) => Some(loc),
                _ => None,
            })
            .collect();
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
            Confirm::Release(loc) => self.release_moving_panes(loc),
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
            Confirm::ReleaseWorkstream(loc) => self.release_workstream(loc),
            Confirm::RemoveFolder(loc) if self.is_workstream(&loc) => {
                let name = loc.slot.clone().unwrap_or_default();
                self.remove_workstream(loc);
                self.say(format!("removed {name}; its slots are back with their repos"));
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

    fn spawn_here(&mut self, kind: Kind, dir: Option<Dir>) {
        if let Some(loc) = self.current_loc() {
            self.spawn(loc, kind, dir);
        } else {
            self.say("not in a git repo — run velmux from inside one");
        }
    }

    /// Open a pane in `loc`, splitting the focused pane there (`dir`, or along its longer side).
    fn spawn(&mut self, loc: Loc, kind: Kind, dir: Option<Dir>) {
        self.spawn_with(loc, kind, dir, LaunchOpts::default())
    }

    fn spawn_with(&mut self, loc: Loc, kind: Kind, dir: Option<Dir>, opts: LaunchOpts) {
        match self.launch(&loc, kind, opts) {
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
        let mut agent_env = Vec::new();
        let command = match kind {
            _ if opts.command.is_some() => opts.command.clone(),
            Kind::Agent => {
                // The agent: the one a resumed session belongs to, the one
                // asked for, else the configured one.
                let resume = resume.as_deref().map(crate::agents::parse_ref);
                let configured = crate::agents::Agent::of_command(&cfg.agent);
                match resume.map(|(a, _)| a).or(opts.agent).or(configured) {
                    Some(agent) => {
                        // Your configured command (and its flags) when it's this agent.
                        let base = if configured == Some(agent) { cfg.agent.clone() } else { agent.name().to_string() };
                        let ctx = self.agent_context(loc, agent);
                        let l = crate::agents::launch(agent, &base, resume.map(|(_, id)| id), &ctx);
                        session = l.session;
                        agent_env = l.env;
                        Some(l.command)
                    }
                    None => Some(cfg.agent.clone()),
                }
            }
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
        let mut env = match (&project.pool, &loc.slot) {
            (Some(pool), Some(id)) => pool.slot_env(id),
            (Some(pool), None) => vec![
                ("VELMUX_BASE".to_string(), pool.base.to_string_lossy().into_owned()),
                ("VELMUX_POOL_DIR".to_string(), pool.dir.to_string_lossy().into_owned()),
            ],
            (None, Some(id)) if self.is_workstream(loc) => vec![("VELMUX_WORKSTREAM".to_string(), id.clone())],
            (None, _) => Vec::new(),
        };
        env.extend(agent_env);
        let body = ui::pane_area(self.size.0, self.size.1, self.sidebar_hidden, self.sidebar_cols);
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
                let opts = LaunchOpts { resume: p.resume, cwd: p.cwd, command: p.command, agent: None };
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
        c.rewrap = pane.agent.is_some();
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
            // Links (OSC 8), redrawn every frame: a terminal ties the link to
            // the cells printed while it's open, and a diff may reprint them.
            for (y, x, w, url) in ui::sidebar_links(self, self.size.0, self.size.1) {
                use std::io::Write;
                let _ = write!(backend, "\x1b]8;;{url}\x1b\\");
                let _ = backend.draw((x..x + w).filter_map(|cx| buf.cell((cx, y)).map(|c| (cx, y, c))));
                let _ = write!(backend, "\x1b]8;;\x1b\\");
            }
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
