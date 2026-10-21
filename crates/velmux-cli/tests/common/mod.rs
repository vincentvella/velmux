//! The end-to-end harness: the real `velmux` binary driven through a
//! pseudo-terminal, with a fake Claude (`examples/fake_claude.rs`) and a fake
//! `gh`, each on its own socket, state, config, Claude dir and HOME.
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

/// One server at a time: they're heavy, and timing is easier to reason about.
static SERIAL: Mutex<()> = Mutex::new(());

pub struct Env {
    pub _tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub socket: PathBuf,
    pub claude_dir: PathBuf,
    pub state: PathBuf,
    pub path_var: String,
    pub _lock: std::sync::MutexGuard<'static, ()>,
}

pub fn sh(dir: &Path, cmd: &str) -> String {
    let out = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).output().unwrap();
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
    Path::new(VELMUX).parent().unwrap().join("examples/fake_claude")
}

impl Env {
    pub fn new() -> Env {
        let lock = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (home, bin, claude_dir, state) = (root.join("home"), root.join("bin"), root.join("claude"), root.join("state"));
        for d in [&home, &bin, &claude_dir, &state] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(home.join(".gitconfig"), "[user]\n\tname = e2e\n\temail = e2e@example.com\n[commit]\n\tgpgsign = false\n[init]\n\tdefaultBranch = main\n").unwrap();
        // Installed like Claude (`…/claude/versions/<v>`), so it's detected as Claude.
        let versions = root.join("install/claude/versions");
        std::fs::create_dir_all(&versions).unwrap();
        std::fs::copy(fake_claude(), versions.join("9.9.9")).unwrap();
        std::os::unix::fs::symlink(versions.join("9.9.9"), bin.join("claude")).unwrap();
        // The same fake, installed as Codex (`…/codex/bin/codex`), plays Codex.
        let codex = root.join("install/codex/bin");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::copy(fake_claude(), codex.join("codex")).unwrap();
        std::os::unix::fs::symlink(codex.join("codex"), bin.join("codex")).unwrap();
        // And as Droid (followed through its session hook).
        let droid = root.join("install/droid");
        std::fs::create_dir_all(&droid).unwrap();
        std::fs::copy(fake_claude(), droid.join("droid")).unwrap();
        std::os::unix::fs::symlink(droid.join("droid"), bin.join("droid")).unwrap();
        // A stand-in browser: what velmux opens is written to `opened.txt`.
        std::fs::write(bin.join("browser"), format!("#!/bin/sh\necho \"$1\" >> '{}'\n", root.join("opened.txt").display())).unwrap();
        sh(&bin, "chmod +x browser");
        let gh = "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'gh version 0 (fake)' ;;\n  pr) cat \"$FAKE_GH_PRS\" 2>/dev/null || echo '[]' ;;\nesac\n";
        std::fs::write(bin.join("gh"), gh).unwrap();
        sh(&bin, "chmod +x gh");
        std::fs::write(
            root.join("config.toml"),
            format!(
                "prefix = \"C-a\"\nauto_restart = false\nslots = 2\nfetch_every = 0\npool_root = \"{}\"\nscratch_dir = \"{}\"\n[notify]\ndesktop = false\nsound = false\n",
                root.join("pools").display(),
                root.join("ws").display()
            ),
        )
        .unwrap();
        // Short (macOS caps socket paths at 104 bytes) and new per test, so a
        // test never reaches the previous one's server while it shuts down.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let socket = PathBuf::from(format!("/tmp/vmx-e2e-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let velmux_dir = Path::new(VELMUX).parent().unwrap();
        let path_var = format!("{}:{}:{}", bin.display(), velmux_dir.display(), std::env::var("PATH").unwrap_or_default());
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
            ("BROWSER", self.root.join("bin/browser").display().to_string()),
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
        let out = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).envs(self.vars()).output().unwrap();
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
        let p = parser.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                p.lock().unwrap().process(&buf[..n]);
            }
        });
        let writer = pair.master.take_writer().unwrap();
        let ui = Ui { parser, writer: Mutex::new(writer), child, _master: pair.master };
        ui.wait_for("menu");
        ui
    }

    pub fn stop(&self, dir: &Path) {
        let _ = self.velmux(dir, &["stop"]);
        self.until("the server to stop", || std::os::unix::net::UnixStream::connect(&self.socket).is_err());
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new(VELMUX).arg("stop").envs(self.vars()).output();
        let start = Instant::now();
        while std::os::unix::net::UnixStream::connect(&self.socket).is_ok() && start.elapsed() < WAIT {
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

pub struct Ui {
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub writer: Mutex<Box<dyn Write + Send>>,
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
    pub _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Drop for Ui {
    fn drop(&mut self) {
        let _ = self.child.kill();
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
            assert!(start.elapsed() < WAIT, "timed out waiting for {needle:?}; screen:\n{screen}");
            std::thread::sleep(Duration::from_millis(100));
        }
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
