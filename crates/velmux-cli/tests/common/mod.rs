//! The end-to-end harness: the real `velmux` binary driven through a
//! pseudo-terminal, with a fake Claude (`examples/fake_claude.rs`) and a fake
//! `gh`, each on its own socket, state, config, Claude dir and HOME.
//!
//! Panes run the platform's shell (`sh` here, PowerShell on Windows), so a
//! test waits for [`PROMPT`] and types commands both understand, or picks one
//! per platform; setup commands run in a POSIX `sh` everywhere (on Windows,
//! Git's).
#![allow(dead_code)] // each test binary uses part of it

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

pub const VELMUX: &str = env!("CARGO_BIN_EXE_velmux");
pub const COLS: u16 = 120;
pub const ROWS: u16 = 32;
pub const WAIT: Duration = Duration::from_secs(25);
/// What a pane's shell shows when it's ready for a command (PowerShell's
/// prompt is `PS C:\…> `).
pub const PROMPT: &str = if cfg!(windows) { "PS " } else { "$" };

/// The socket clients reach the server on.
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(windows)]
use uds_windows::UnixStream;

/// A few servers at a time (`VELMUX_E2E_JOBS`; by default 4, or fewer on a
/// smaller machine): each test has its own server, socket, state and HOME, so
/// they don't meet, but they're heavy enough that too many makes timing flaky.
static RUNNING: (Mutex<usize>, std::sync::Condvar) = (Mutex::new(0), std::sync::Condvar::new());

/// A place among the tests running at once, held for the test's life.
pub struct Slot;

impl Slot {
    fn take() -> Slot {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        let jobs = std::env::var("VELMUX_E2E_JOBS").ok().and_then(|j| j.parse().ok()).unwrap_or(cores.min(4)).max(1);
        let (count, freed) = &RUNNING;
        let mut n = count.lock().unwrap_or_else(|e| e.into_inner());
        while *n >= jobs {
            n = freed.wait(n).unwrap_or_else(|e| e.into_inner());
        }
        *n += 1;
        Slot
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let (count, freed) = &RUNNING;
        *count.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        freed.notify_one();
    }
}

pub struct Env {
    pub _tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub socket: PathBuf,
    pub claude_dir: PathBuf,
    pub state: PathBuf,
    pub path_var: String,
    pub _lock: Slot,
}

/// A POSIX `sh` (on Windows, the one Git ships).
fn posix_sh() -> Command {
    Command::new(velmux_pool::sys::posix_shell().expect("a POSIX sh (Git's, on Windows)"))
}

pub fn sh(dir: &Path, cmd: &str) -> String {
    let out = posix_sh().arg("-c").arg(cmd).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "`{cmd}` failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn fake_claude() -> PathBuf {
    // A test target alone doesn't build examples; this is quick when it's current.
    static BUILT: std::sync::Once = std::sync::Once::new();
    BUILT.call_once(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let ok = Command::new(cargo).args(["build", "-q", "-p", "velmux-cli", "--example", "fake_claude"]).status().unwrap();
        assert!(ok.success(), "building the fake claude");
    });
    Path::new(VELMUX).parent().unwrap().join(format!("examples/fake_claude{}", std::env::consts::EXE_SUFFIX))
}

/// The fake, copied to `dir/name` (`name.exe` on Windows) and linked from
/// `bin`, which is on the tests' `PATH`: it plays whoever it's installed as.
fn install(dir: &Path, name: &str, bin: &Path, as_name: &str) {
    let exe = std::env::consts::EXE_SUFFIX;
    std::fs::create_dir_all(dir).unwrap();
    let file = dir.join(format!("{name}{exe}"));
    std::fs::copy(fake_claude(), &file).unwrap();
    velmux_pool::sys::symlink(&file, &bin.join(format!("{as_name}{exe}"))).unwrap();
}

/// Run inside velmux (a pane), the tests would inherit its `VELMUX_*`
/// variables (`VELMUX_EXE`, `VELMUX_PANE`…) and point at the real install:
/// every command a test starts gets only the ones it sets itself.
fn forget_outer_velmux() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("VELMUX") {
                std::env::remove_var(&key);
            }
        }
    });
}

impl Env {
    pub fn new() -> Env {
        forget_outer_velmux();
        let lock = Slot::take();
        let tmp = tempfile::tempdir().unwrap();
        let root = velmux_pool::sys::Canonical::canonical(tmp.path()).unwrap();
        let (home, bin, claude_dir, state) = (root.join("home"), root.join("bin"), root.join("claude"), root.join("state"));
        for d in [&home, &bin, &claude_dir, &state] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(home.join(".gitconfig"), "[user]\n\tname = e2e\n\temail = e2e@example.com\n[commit]\n\tgpgsign = false\n[init]\n\tdefaultBranch = main\n[core]\n\tautocrlf = false\n").unwrap();
        // Installed like Claude (`…/claude/versions/<v>`), so it's detected as Claude.
        install(&root.join("install/claude/versions"), "9.9.9", &bin, "claude");
        // The same fake, installed as Codex (`…/codex/bin/codex`), plays Codex.
        install(&root.join("install/codex/bin"), "codex", &bin, "codex");
        // And as Droid (followed through its session hook).
        install(&root.join("install/droid"), "droid", &bin, "droid");
        // `gh`, answering `--version` and `pr` with `$FAKE_GH_PRS` (or `[]`).
        install(&root.join("install/gh"), "gh", &bin, "gh");
        // A stand-in browser: what velmux opens is written to `opened.txt`.
        install(&root.join("install/browser"), "browser", &bin, "browser");
        std::fs::write(
            root.join("config.toml"),
            format!(
                "prefix = \"C-a\"\nauto_restart = false\nslots = 2\nfetch_every = 0\npool_root = '{}'\nscratch_dir = '{}'\n[notify]\ndesktop = false\nsound = false\n",
                root.join("pools").display(),
                root.join("ws").display()
            ),
        )
        .unwrap();
        // Short (macOS caps socket paths at 104 bytes) and new per test, so a
        // test never reaches the previous one's server while it shuts down.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp_dir = if cfg!(windows) { std::env::temp_dir() } else { PathBuf::from("/tmp") };
        let socket = tmp_dir.join(format!("vmx-e2e-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let velmux_dir = Path::new(VELMUX).parent().unwrap();
        let outer = std::env::var_os("PATH").unwrap_or_default();
        let dirs = [bin.clone(), velmux_dir.to_path_buf()].into_iter().chain(std::env::split_paths(&outer));
        let path_var = std::env::join_paths(dirs).unwrap().to_string_lossy().into_owned();
        Env { _tmp: tmp, root, socket, claude_dir, state, path_var, _lock: lock }
    }

    pub fn vars(&self) -> Vec<(&'static str, String)> {
        vec![
            ("HOME", self.root.join("home").display().to_string()),
            ("PATH", self.path_var.clone()),
            ("SHELL", "/bin/sh".into()),
            ("TERM", "xterm-256color".into()),
            ("VELMUX_SOCKET", self.socket.display().to_string()),
            ("VELMUX_STATE_DIR", self.state.display().to_string()),
            ("VELMUX_CONFIG", self.root.join("config.toml").display().to_string()),
            ("CLAUDE_CONFIG_DIR", self.claude_dir.display().to_string()),
            ("CODEX_HOME", self.root.join("codex").display().to_string()),
            ("VELMUX_BIN", VELMUX.into()),
            ("FAKE_GH_PRS", self.root.join("prs.json").display().to_string()),
            ("BROWSER", self.root.join(format!("bin/browser{}", std::env::consts::EXE_SUFFIX)).display().to_string()),
            ("FAKE_OPENED", self.root.join("opened.txt").display().to_string()),
            ("VELMUX_PR_EVERY", "1".into()),
            ("GIT_TERMINAL_PROMPT", "0".into()),
            ("ENV", String::new()),
        ]
    }

    /// A git repo `<root>/ws/<name>` with one commit.
    pub fn repo(&self, name: &str) -> PathBuf {
        let dir = self.root.join("ws").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        self.run_in(&dir, "git init -q -b main && echo hi > README.md && git add -A && git commit -qm init");
        dir
    }

    pub fn run_in(&self, dir: &Path, cmd: &str) -> String {
        let out = posix_sh().arg("-c").arg(cmd).current_dir(dir).envs(self.vars()).output().unwrap();
        assert!(out.status.success(), "`{cmd}` failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    pub fn velmux(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(VELMUX).args(args).current_dir(dir).envs(self.vars()).output().unwrap()
    }

    pub fn json(&self, dir: &Path, args: &[&str]) -> serde_json::Value {
        let out = self.velmux(dir, args);
        assert!(out.status.success(), "velmux {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// Wait until `f` holds, failing with `what` after a while.
    pub fn until(&self, what: &str, mut f: impl FnMut() -> bool) {
        let start = Instant::now();
        while !f() {
            assert!(start.elapsed() < WAIT, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn slot(&self, repo: &Path, id: &str) -> serde_json::Value {
        let slots = self.json(repo, &["status", "--json"]);
        slots.as_array().unwrap().iter().find(|s| s["id"] == id).cloned().unwrap()
    }

    /// Attach a client in `dir`, starting the server if needed.
    pub fn attach(&self, dir: &Path) -> Ui {
        let pair = native_pty_system().openpty(PtySize { rows: ROWS, cols: COLS, pixel_width: 0, pixel_height: 0 }).unwrap();
        let mut cmd = CommandBuilder::new(VELMUX);
        cmd.cwd(dir);
        for (k, v) in self.vars() {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let parser = Arc::new(Mutex::new(vt100::Parser::new(ROWS.try_into().unwrap(), COLS.try_into().unwrap(), 1000)));
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let raw = Arc::new(Mutex::new(Vec::new()));
        let (p, w, r) = (parser.clone(), writer.clone(), raw.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut raw = r.lock().unwrap();
                raw.extend_from_slice(&buf[..n]);
                let over = raw.len().saturating_sub(2000);
                raw.drain(..over);
                drop(raw);
                let mut p = p.lock().unwrap();
                p.process(&buf[..n]);
                // Where's the cursor? (ConPTY asks on start, and waits.)
                if buf[..n].windows(4).any(|q| q == b"\x1b[6n") {
                    let (row, col) = p.screen().cursor_position();
                    // (In one write: ConPTY doesn't piece a split answer together.)
                    let mut w = w.lock().unwrap();
                    let _ = w.write_all(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
                    let _ = w.flush();
                }
            }
        });
        let ui = Ui { parser, writer, child: Mutex::new(child), _master: pair.master, raw, log: self.state.join("server.log"), socket: self.socket.clone() };
        ui.wait_for("menu");
        ui
    }

    pub fn stop(&self, dir: &Path) {
        let _ = self.velmux(dir, &["stop"]);
        self.until("the server to stop", || UnixStream::connect(&self.socket).is_err());
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new(VELMUX).arg("stop").envs(self.vars()).output();
        let start = Instant::now();
        while UnixStream::connect(&self.socket).is_ok() && start.elapsed() < WAIT {
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

pub struct Ui {
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    pub _master: Box<dyn portable_pty::MasterPty + Send>,
    /// The last of what the client wrote, as it came (for a failure's report).
    pub raw: Arc<Mutex<Vec<u8>>>,
    pub log: PathBuf,
    pub socket: PathBuf,
}

impl Drop for Ui {
    fn drop(&mut self) {
        let _ = self.child.lock().unwrap_or_else(|e| e.into_inner()).kill();
    }
}

impl Ui {
    pub fn screen(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    pub fn send(&self, bytes: &[u8]) {
        let mut w = self.writer.lock().unwrap();
        w.write_all(bytes).unwrap();
        w.flush().unwrap();
        drop(w);
        std::thread::sleep(Duration::from_millis(120));
    }

    /// The prefix (ctrl+a) then `key`.
    pub fn prefix(&self, key: &str) {
        self.send(b"\x01");
        self.send(key.as_bytes());
    }

    pub fn line(&self, text: &str) {
        self.send(text.as_bytes());
        self.send(b"\r");
    }

    /// The pane area's rows run together at full width, so text that wrapped
    /// (a pane started narrower than it is now) still matches.
    pub fn pane_text(&self) -> String {
        let p = self.parser.lock().unwrap();
        p.screen().rows(33, COLS - 33).collect()
    }

    pub fn wait_for(&self, needle: &str) -> String {
        let start = Instant::now();
        loop {
            let screen = self.screen();
            if screen.contains(needle) || self.pane_text().contains(needle) {
                return screen;
            }
            assert!(start.elapsed() < WAIT, "timed out waiting for {needle:?}; screen:\n{screen}\n{}", self.report());
            std::thread::sleep(Duration::from_millis(100));
        }
    }


    /// What a failure is worth knowing beyond the screen: whether the client
    /// is still running, the end of its raw output, and the server's log.
    pub fn report(&self) -> String {
        let status = self.child.lock().unwrap().try_wait().map(|s| s.map_or("running".to_string(), |s| format!("exited {s:?}")));
        let raw = String::from_utf8_lossy(&self.raw.lock().unwrap()).escape_debug().to_string();
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        let tail: Vec<&str> = log.lines().rev().take(30).collect();
        let server = if UnixStream::connect(&self.socket).is_ok() { "up" } else { "not answering" };
        format!("client: {status:?}\nserver: {server}\nraw output: {raw}\nserver.log:\n{}", tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
    }

    pub fn wait_gone_text(&self, needle: &str) {
        let start = Instant::now();
        while self.screen().contains(needle) {
            assert!(start.elapsed() < WAIT, "{needle:?} never went away; screen:\n{}", self.screen());
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Screen position (0-based col, row) of the first `needle`.
    pub fn find(&self, needle: &str) -> Option<(u16, u16)> {
        let p = self.parser.lock().unwrap();
        // The sidebar: what clicks are aimed at.
        let rows: Vec<String> = p.screen().rows(0, 32).collect();
        rows.iter().enumerate().find_map(|(r, line)| line.find(needle).map(|i| (line[..i].chars().count() as u16, r as u16)))
    }

    pub fn click_on(&self, needle: &str, right: bool) {
        self.wait_for(needle);
        let (x, y) = self.find(needle).unwrap();
        let b = if right { 2 } else { 0 };
        self.send(format!("\x1b[<{b};{};{}M\x1b[<{b};{};{}m", x + 2, y + 1, x + 2, y + 1).as_bytes());
    }

    /// The session id the newest fake Claude printed (once its whole line is in).
    pub fn claude_id(&self) -> String {
        let start = Instant::now();
        loop {
            let screen = self.screen();
            if let Some(at) = screen.rfind("FAKE-CLAUDE id=") {
                let rest = &screen[at + "FAKE-CLAUDE id=".len()..];
                if let Some((id, _)) = rest.split_once(" mode=") {
                    return id.to_string();
                }
            }
            assert!(start.elapsed() < WAIT, "no fake Claude; screen:\n{screen}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The fake Claude in the focused pane quits.
    pub fn exit_claude(&self) {
        self.line("exit");
        self.wait_for("FAKE-CLAUDE bye");
    }
}


/// The terminal palette screenshots are drawn with (a dark theme: ANSI 0–15,
/// then the default foreground and background).
const ANSI: [&str; 16] = [
    "#1e1e2e", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
    "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
];
const FG: &str = "#cdd6f4";
const BG: &str = "#181825";

fn color(c: vt100::Color, default: &str) -> String {
    match c {
        vt100::Color::Default => default.to_string(),
        vt100::Color::Idx(i) if i < 16 => ANSI[i as usize].to_string(),
        vt100::Color::Idx(i) if i < 232 => {
            let i = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            format!("#{:02x}{:02x}{:02x}", level(i / 36), level(i / 6 % 6), level(i % 6))
        }
        vt100::Color::Idx(i) => {
            let v = 8 + (i - 232) * 10;
            format!("#{v:02x}{v:02x}{v:02x}")
        }
        vt100::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

impl Ui {
    /// The screen as an SVG "screenshot" in a window frame: every cell's
    /// character, colours and attributes, laid on a fixed grid.
    pub fn svg(&self, title: &str) -> String {
        let p = self.parser.lock().unwrap();
        let screen = p.screen();
        let (rows, cols) = (ROWS as usize, COLS as usize);
        let (cw, ch, pad, bar) = (8.4, 18.0, 16.0, 34.0);
        let (w, h) = (cols as f64 * cw + pad * 2.0, rows as f64 * ch + pad * 2.0 + bar);
        let mut out = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" font-family="'SF Mono','JetBrains Mono',Menlo,Consolas,monospace" font-size="14">
<rect width="{w}" height="{h}" rx="10" fill="{BG}"/>
<rect width="{w}" height="{bar}" rx="10" fill="#11111b"/><rect y="{}" width="{w}" height="10" fill="#11111b"/>
<circle cx="20" cy="17" r="6" fill="#ff5f57"/><circle cx="40" cy="17" r="6" fill="#febc2e"/><circle cx="60" cy="17" r="6" fill="#28c840"/>
<text x="{}" y="22" fill="#7f849c" text-anchor="middle" font-size="13">{}</text>
"##,
            bar - 10.0,
            w / 2.0,
            escape(title)
        );
        for r in 0..rows {
            let y = bar + pad + r as f64 * ch;
            let mut c = 0;
            while c < cols {
                let Some(cell) = screen.cell(r as u16, c as u16) else { break };
                let (mut fg, mut bg) = (color(cell.fgcolor(), FG), color(cell.bgcolor(), BG));
                if cell.inverse() {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let style = (fg.clone(), bg.clone(), cell.bold(), cell.italic(), cell.underline());
                // A run of cells with the same look.
                let start = c;
                let mut text = String::new();
                while c < cols {
                    let Some(cell) = screen.cell(r as u16, c as u16) else { break };
                    let (mut f, mut b) = (color(cell.fgcolor(), FG), color(cell.bgcolor(), BG));
                    if cell.inverse() {
                        std::mem::swap(&mut f, &mut b);
                    }
                    if (f, b, cell.bold(), cell.italic(), cell.underline()) != style {
                        break;
                    }
                    if !cell.is_wide_continuation() {
                        text.push_str(if cell.has_contents() { cell.contents() } else { " " });
                    }
                    c += 1;
                }
                let x = pad + start as f64 * cw;
                let width = (c - start) as f64 * cw;
                if bg != BG {
                    out.push_str(&format!(r#"<rect x="{x:.1}" y="{y:.1}" width="{width:.1}" height="{ch}" fill="{bg}"/>"#));
                }
                if !text.trim().is_empty() {
                    let mut attrs = String::new();
                    if style.2 {
                        attrs.push_str(r#" font-weight="bold""#);
                    }
                    if style.3 {
                        attrs.push_str(r#" font-style="italic""#);
                    }
                    if style.4 {
                        attrs.push_str(r#" text-decoration="underline""#);
                    }
                    // Pin each run to its grid width, whatever font draws it.
                    out.push_str(&format!(
                        r#"<text x="{x:.1}" y="{:.1}" fill="{fg}" xml:space="preserve" textLength="{width:.1}" lengthAdjust="spacingAndGlyphs"{attrs}>{}</text>"#,
                        y + ch * 0.75,
                        escape(&text)
                    ));
                }
                out.push('\n');
            }
        }
        out.push_str("</svg>\n");
        out
    }
}
