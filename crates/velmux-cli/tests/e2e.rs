//! End-to-end: the real `velmux` binary driven through a pseudo-terminal, with
//! a fake Claude (`examples/fake_claude.rs`) and a fake `gh`, each test on its
//! own socket, state, config, Claude dir and HOME.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const VELMUX: &str = env!("CARGO_BIN_EXE_velmux");
const COLS: u16 = 120;
const ROWS: u16 = 32;
const WAIT: Duration = Duration::from_secs(25);

/// One server at a time: they're heavy, and timing is easier to reason about.
static SERIAL: Mutex<()> = Mutex::new(());

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    socket: PathBuf,
    claude_dir: PathBuf,
    state: PathBuf,
    path_var: String,
    _lock: std::sync::MutexGuard<'static, ()>,
}

fn sh(dir: &Path, cmd: &str) -> String {
    let out = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "`{cmd}` failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn fake_claude() -> PathBuf {
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
    fn new() -> Env {
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
        let gh = "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'gh version 0 (fake)' ;;\n  pr) cat \"$FAKE_GH_PRS\" 2>/dev/null || echo '[]' ;;\nesac\n";
        std::fs::write(bin.join("gh"), gh).unwrap();
        sh(&bin, "chmod +x gh");
        std::fs::write(
            root.join("config.toml"),
            format!(
                "prefix = \"C-a\"\nauto_restart = false\nslots = 2\nfetch_every = 0\npool_root = \"{}\"\n[notify]\ndesktop = false\nsound = false\n",
                root.join("pools").display()
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

    fn vars(&self) -> Vec<(&'static str, String)> {
        vec![
            ("HOME", self.root.join("home").display().to_string()),
            ("PATH", self.path_var.clone()),
            ("SHELL", "/bin/sh".into()),
            ("TERM", "xterm-256color".into()),
            ("VELMUX_SOCKET", self.socket.display().to_string()),
            ("VELMUX_STATE_DIR", self.state.display().to_string()),
            ("VELMUX_CONFIG", self.root.join("config.toml").display().to_string()),
            ("CLAUDE_CONFIG_DIR", self.claude_dir.display().to_string()),
            ("VELMUX_BIN", VELMUX.into()),
            ("FAKE_GH_PRS", self.root.join("prs.json").display().to_string()),
            ("GIT_TERMINAL_PROMPT", "0".into()),
            ("ENV", String::new()),
        ]
    }

    /// A git repo `<root>/ws/<name>` with one commit.
    fn repo(&self, name: &str) -> PathBuf {
        let dir = self.root.join("ws").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        self.run_in(&dir, "git init -q -b main && echo hi > README.md && git add -A && git commit -qm init");
        dir
    }

    fn run_in(&self, dir: &Path, cmd: &str) -> String {
        let out = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).envs(self.vars()).output().unwrap();
        assert!(out.status.success(), "`{cmd}` failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn velmux(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(VELMUX).args(args).current_dir(dir).envs(self.vars()).output().unwrap()
    }

    fn json(&self, dir: &Path, args: &[&str]) -> serde_json::Value {
        let out = self.velmux(dir, args);
        assert!(out.status.success(), "velmux {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// Wait until `f` holds, failing with `what` after a while.
    fn until(&self, what: &str, mut f: impl FnMut() -> bool) {
        let start = Instant::now();
        while !f() {
            assert!(start.elapsed() < WAIT, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn slot(&self, repo: &Path, id: &str) -> serde_json::Value {
        let slots = self.json(repo, &["status", "--json"]);
        slots.as_array().unwrap().iter().find(|s| s["id"] == id).cloned().unwrap()
    }

    /// Attach a client in `dir`, starting the server if needed.
    fn attach(&self, dir: &Path) -> Ui {
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

    fn stop(&self, dir: &Path) {
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

struct Ui {
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Drop for Ui {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

impl Ui {
    fn screen(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    fn send(&self, bytes: &[u8]) {
        let mut w = self.writer.lock().unwrap();
        w.write_all(bytes).unwrap();
        w.flush().unwrap();
        drop(w);
        std::thread::sleep(Duration::from_millis(120));
    }

    /// The prefix (ctrl+a) then `key`.
    fn prefix(&self, key: &str) {
        self.send(b"\x01");
        self.send(key.as_bytes());
    }

    fn line(&self, text: &str) {
        self.send(text.as_bytes());
        self.send(b"\r");
    }

    /// The pane area's rows run together at full width, so text that wrapped
    /// (a pane started narrower than it is now) still matches.
    fn pane_text(&self) -> String {
        let p = self.parser.lock().unwrap();
        p.screen().rows(33, COLS - 33).collect()
    }

    fn wait_for(&self, needle: &str) -> String {
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


    /// Screen position (0-based col, row) of the first `needle`.
    fn find(&self, needle: &str) -> Option<(u16, u16)> {
        let p = self.parser.lock().unwrap();
        // The sidebar: what clicks are aimed at.
        let rows: Vec<String> = p.screen().rows(0, 32).collect();
        rows.iter().enumerate().find_map(|(r, line)| line.find(needle).map(|i| (line[..i].chars().count() as u16, r as u16)))
    }

    fn click_on(&self, needle: &str, right: bool) {
        self.wait_for(needle);
        let (x, y) = self.find(needle).unwrap();
        let b = if right { 2 } else { 0 };
        self.send(format!("\x1b[<{b};{};{}M\x1b[<{b};{};{}m", x + 2, y + 1, x + 2, y + 1).as_bytes());
    }

    /// The session id the newest fake Claude printed (once its whole line is in).
    fn claude_id(&self) -> String {
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
    fn exit_claude(&self) {
        self.line("exit");
        self.wait_for("FAKE-CLAUDE bye");
    }
}

#[test]
fn base_claude_claims_a_slot_and_releases_it() {
    let env = Env::new();
    let repo = env.repo("app");
    let ui = env.attach(&repo);
    ui.prefix("a");
    let id = ui.claude_id();
    ui.line("claim feat/e2e");
    ui.wait_for("claimed ");
    ui.wait_for("feat/e2e ·1");
    let slot = env.slot(&repo, "slot-1");
    assert_eq!((slot["status"].as_str(), slot["branch"].as_str()), (Some("inuse"), Some("feat/e2e")));
    ui.wait_for("slot-1 · feat/e2e");

    ui.line("release");
    env.until("slot-1 to be free", || env.slot(&repo, "slot-1")["status"] == "free");
    ui.wait_for("app › base");
    assert!(ui.screen().contains(&format!("FAKE-CLAUDE id={id}")), "the same conversation, back in the base");
}

#[test]
fn parked_work_resumes_with_its_claude() {
    let env = Env::new();
    let repo = env.repo("app");
    let ui = env.attach(&repo);
    ui.prefix("a");
    let id = ui.claude_id();
    ui.line("claim feat/park");
    ui.wait_for("claimed ");
    ui.line("sh echo wip > wip.txt");
    ui.wait_for("ran true");
    ui.line("release");
    ui.wait_for("feat/park");
    ui.wait_for("1 file");
    let parked = env.json(&repo, &["parked", "--json"]);
    assert_eq!(parked[0]["branch"], "feat/park");
    assert_eq!(parked[0]["sessions"][0], id.as_str());

    // Its Claude is still open (back in the base): resuming moves it over.
    ui.click_on("feat/park", false);
    ui.wait_for("resume in a slot");
    ui.send(b"\r");
    ui.wait_for("resumed feat/park");
    let slot = env.json(&repo, &["status", "--json"]).as_array().unwrap().iter().find(|s| s["branch"] == "feat/park").cloned().unwrap();
    assert!(Path::new(slot["path"].as_str().unwrap()).join("wip.txt").exists(), "stash restored");
    let screen = ui.wait_for("· feat/park :30");
    assert!(screen.contains(&format!("FAKE-CLAUDE id={id} mode=new")), "the live conversation moved, not a new one:\n{screen}");
    assert!(env.json(&repo, &["parked", "--json"]).as_array().unwrap().is_empty());
}

#[test]
fn parked_work_resumes_a_closed_claude_from_its_transcript() {
    let env = Env::new();
    let repo = env.repo("app");
    let ui = env.attach(&repo);
    ui.prefix("a");
    let id = ui.claude_id();
    ui.line("claim feat/gone");
    ui.wait_for("claimed ");
    ui.line("sh echo wip > wip.txt");
    ui.wait_for("ran true");
    ui.line("release");
    ui.wait_for("1 file");
    ui.exit_claude();

    ui.click_on("feat/gone", true);
    ui.wait_for("resume in a slot");
    ui.send(b"\r");
    ui.wait_for(&format!("FAKE-CLAUDE id={id} mode=resume"));
}

#[test]
fn a_stopped_server_restores_scrollback() {
    let env = Env::new();
    let repo = env.repo("app");
    let ui = env.attach(&repo);
    ui.prefix("c");
    ui.wait_for("$");
    ui.line("i=1; while [ $i -le 80 ]; do echo e2e-line-$i; i=$((i+1)); done");
    ui.wait_for("e2e-line-80");
    env.stop(&repo);
    drop(ui);

    let ui = env.attach(&repo);
    ui.wait_for("app › base · ");
    ui.prefix("[");
    ui.send(b"g");
    ui.wait_for("e2e-line-1");
}

#[test]
fn a_stopped_server_brings_claude_back_as_itself() {
    let env = Env::new();
    let repo = env.repo("app");
    let ui = env.attach(&repo);
    ui.prefix("a");
    let id = ui.claude_id();
    env.stop(&repo);
    drop(ui);
    let ui = env.attach(&repo);
    ui.wait_for(&format!("FAKE-CLAUDE id={id} mode=resume"));
}

#[test]
fn live_restart_keeps_panes_running() {
    let env = Env::new();
    let repo = env.repo("app");
    let ui = env.attach(&repo);
    ui.prefix("a");
    ui.claude_id();
    let out = env.velmux(&repo, &["restart"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("restarting"));
    std::thread::sleep(Duration::from_secs(2));
    ui.wait_for("menu");
    ui.line("say still here");
    ui.wait_for("said still here");
}

#[test]
fn find_a_conversation_and_resume_it() {
    let env = Env::new();
    let repo = env.repo("app");
    let ui = env.attach(&repo);
    ui.prefix("a");
    let id = ui.claude_id();
    ui.line("say the purple elephant problem");
    ui.wait_for("said the purple elephant problem");
    ui.exit_claude();

    ui.prefix("/");
    ui.wait_for("find conversation");
    ui.send(b"elephant");
    ui.wait_for("1 of 1");
    ui.wait_for("purple elephant");
    ui.send(b"\r");
    // Resumed beside the shell it left behind, so its line wraps in the split.
    let screen = ui.wait_for("mode=resume");
    assert!(screen.contains(&format!("FAKE-CLAUDE id={}", &id[..10])), "{screen}");
}

#[test]
fn a_merged_pr_offers_release_and_isnt_parked() {
    let env = Env::new();
    let repo = env.repo("app");
    env.run_in(&repo, "git remote add origin https://github.com/acme/app.git");
    assert!(env.velmux(&repo, &["init"]).status.success());
    let out = env.velmux(&repo, &["claim", "feat/pr", "--no-fetch", "--no-install"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let slot = env.slot(&repo, "slot-1");
    env.run_in(Path::new(slot["path"].as_str().unwrap()), "echo x > x && git add x && git commit -qm x");
    let prs = r#"[{"headRefName":"feat/pr","number":7,"url":"https://github.com/acme/app/pull/7","state":"MERGED","isDraft":false,
        "statusCheckRollup":[{"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":"SUCCESS"}]}]"#;
    std::fs::write(env.root.join("prs.json"), prs).unwrap();

    let ui = env.attach(&repo);
    ui.wait_for("#7⇡");
    ui.click_on("feat/pr", true);
    ui.wait_for("release (PR merged)");
    ui.send(b"\r");
    ui.wait_for("y yes");
    ui.send(b"y");
    env.until("slot-1 to be free", || env.slot(&repo, "slot-1")["status"] == "free");
    assert!(env.json(&repo, &["parked", "--json"]).as_array().unwrap().is_empty(), "merged work isn't parked");
}

#[test]
fn the_claude_skill_installs_updates_and_uninstalls() {
    let env = Env::new();
    let repo = env.repo("app");
    let text = |out: Output| String::from_utf8_lossy(&out.stdout).into_owned();
    let user = env.claude_dir.join("skills/velmux/SKILL.md");
    assert!(text(env.velmux(&repo, &["skill", "install"])).contains("installed"));
    let skill = std::fs::read_to_string(&user).unwrap();
    assert!(skill.starts_with("---\nname: velmux\ndescription: "), "frontmatter first");
    assert!(skill.contains("velmux claim") && skill.contains("velmux release"));
    assert!(text(env.velmux(&repo, &["skill", "install"])).contains("already up to date"));
    std::fs::write(&user, "old").unwrap();
    assert!(text(env.velmux(&repo, &["skill", "install"])).contains("updated"));
    // Per repo, to commit.
    env.velmux(&repo, &["skill", "install", "--project"]);
    assert!(repo.join(".claude/skills/velmux/SKILL.md").exists());
    assert!(text(env.velmux(&repo, &["skill", "uninstall"])).contains("removed"));
    assert!(!user.exists());
}
