//! End-to-end tests (harness in `common/`).

mod common;

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use common::*;

#[test]
fn base_claude_claims_a_slot_and_releases_it() {
    let env = Env::new();
    let repo = env.repo("app");
    let ui = env.attach(&repo);
    ui.prefix("a");
    let id = ui.claude_id();
    ui.line("claim feat/e2e");
    ui.wait_for("claimed ");
    ui.wait_for("▣ feat/e2e");
    ui.wait_for("@1");
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

    let opened = env.root.join("opened.txt");
    let ui = env.attach(&repo);
    ui.wait_for("#7⇡ · ⏏ free");
    // Its number opens the PR.
    ui.click_on("#7⇡", false);
    env.until("the PR to open", || std::fs::read_to_string(&opened).is_ok_and(|t| t.trim() == "https://github.com/acme/app/pull/7"));
    // The slot's menu offers to release it, and so does `⏏ free` on its row.
    ui.click_on("feat/pr", true);
    ui.wait_for("release (PR merged)");
    ui.send(b"\x1b");
    ui.click_on("⏏ free", false);
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

#[test]
fn new_space_and_new_session_buttons() {
    let env = Env::new();
    let repo = env.repo("app");
    env.repo("other");
    let ui = env.attach(&repo);

    // ⊕ new space: repos in the scratch dir that aren't spaces yet.
    ui.click_on("new space", false);
    ui.wait_for(" new space ");
    let screen = ui.wait_for("other");
    assert!(!screen.contains("│ app"), "app is already a space:\n{screen}");
    ui.send(b"oth\r");
    ui.wait_for("added other");
    ui.wait_for("other › base");

    // The divider's +: a shell in the scratch dir, as its own session.
    let plus = || {
        let y = ui.find("──── +").map(|(_, y)| y).expect("sessions divider");
        ui.send(format!("\x1b[<0;30;{}M\x1b[<0;30;{}m", y + 1, y + 1).as_bytes());
    };
    plus();
    ui.wait_for("scratch ·");
    ui.line("pwd");
    ui.wait_for(&env.root.join("ws").display().to_string());
    plus();
    ui.wait_for("scratch-2");
}

#[test]
fn the_welcome_screen_sets_you_up() {
    let env = Env::new();
    env.repo("app");
    let home = env.root.join("home");
    // Nothing added yet: the welcome, with setup still to do.
    let ui = env.attach(&home);
    ui.wait_for("Welcome to velmux");
    ui.wait_for("get set up");
    ui.wait_for("teach Claude to work in velmux");
    // Single keys work: there's no pane to take them.
    ui.send(b"k");
    ui.wait_for("Claude skill installed");
    assert!(env.claude_dir.join("skills/velmux/SKILL.md").exists());
    ui.wait_gone_text("teach Claude to work in velmux");
    ui.send(b"n");
    ui.wait_for(" new space ");
    ui.send(b"app\r");
    ui.wait_for("app › base");
    // An empty repo checkout offers its own keys.
    ui.wait_for("claim a slot yourself");
    ui.send(b"a");
    ui.claude_id();
}

#[test]
fn the_guard_hook_keeps_edits_out_of_the_checkout() {
    let env = Env::new();
    let repo = env.repo("app");
    let pools = env.root.join("pools/app/slot-1");
    std::fs::create_dir_all(&pools).unwrap();
    let guard = |file: &Path| {
        let input = serde_json::json!({"tool_name": "Edit", "tool_input": {"file_path": file}, "cwd": repo}).to_string();
        let mut child = std::process::Command::new(VELMUX)
            .arg("guard")
            .envs(env.vars())
            .env("VELMUX_BASE", &repo)
            .env("VELMUX_POOL_DIR", env.root.join("pools/app"))
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        (out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let (code, why) = guard(&repo.join("src/new.rs"));
    assert_eq!(code, Some(2), "an edit in the checkout is blocked");
    assert!(why.contains("velmux claim"), "{why}");
    assert_eq!(guard(Path::new("README.md")).0, Some(2), "relative to cwd, too");
    assert_eq!(guard(&pools.join("README.md")).0, Some(0), "slots are fine");
    assert_eq!(guard(&env.root.join("elsewhere.txt")).0, Some(0));
}

#[test]
fn changes_in_the_checkout_are_flagged_and_move_to_a_slot() {
    let env = Env::new();
    let repo = env.repo("app");
    let ui = env.attach(&repo);
    std::fs::write(repo.join("README.md"), "edited in the checkout\n").unwrap();
    std::fs::write(repo.join("notes.txt"), "new\n").unwrap();
    ui.wait_for("✎2");
    ui.click_on("◆ base", true);
    ui.wait_for("move changes to a slot…");
    ui.send(b"\r");
    ui.wait_for("branch for the checkout's 2 changed file(s)");
    ui.line("feat/moved");
    ui.wait_for("feat/moved*"); // dirty: the changes came along
    let slot = env.json(&repo, &["status", "--json"]).as_array().unwrap().iter().find(|s| s["branch"] == "feat/moved").cloned().unwrap();
    let path = Path::new(slot["path"].as_str().unwrap());
    assert_eq!(std::fs::read_to_string(path.join("README.md")).unwrap(), "edited in the checkout\n");
    assert!(path.join("notes.txt").exists());
    assert_eq!(env.run_in(&repo, "git status --porcelain"), "", "the checkout is clean");
    ui.wait_gone_text("✎2");
}

#[test]
fn update_swaps_in_a_downloaded_binary() {
    let env = Env::new();
    let root = &env.root;
    // A "downloaded" install (outside target/, so not a dev build).
    let bin = root.join("opt/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(VELMUX, bin.join("velmux")).unwrap();
    // The new release: an archive holding a stand-in binary, its checksum, and
    // the API's answer, all as local files.
    let release = root.join("release/velmux-v99.0.0-test");
    std::fs::create_dir_all(&release).unwrap();
    std::fs::write(release.join("velmux"), "#!/bin/sh\necho 'velmux 99.0.0'\n").unwrap();
    sh(&root.join("release"), "tar -czf new.tar.gz velmux-v99.0.0-test && shasum -a 256 new.tar.gz > new.tar.gz.sha256");
    let archive = format!("file://{}", root.join("release/new.tar.gz").display());
    let targets = ["aarch64-apple-darwin", "x86_64-apple-darwin", "x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"];
    let downloads: serde_json::Map<String, serde_json::Value> = targets.iter().map(|t| (t.to_string(), archive.clone().into())).collect();
    let api = root.join("latest.json");
    std::fs::write(&api, serde_json::json!({"version": "99.0.0", "url": "https://example.com/r", "downloads": downloads}).to_string()).unwrap();

    let run = |args: &[&str]| {
        let out = Command::new(bin.join("velmux")).args(args).envs(env.vars()).env("VELMUX_UPDATE_URL", format!("file://{}", api.display())).output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
    };
    let (ok, said) = run(&["update", "--check"]);
    assert!(ok && said.contains("velmux 99.0.0 is out"), "{said}");
    let (ok, said) = run(&["update"]);
    assert!(ok, "{said}");
    let (_, now) = run(&["--version"]);
    assert_eq!(now.trim(), "velmux 99.0.0", "the binary was swapped");

    // A tampered download is refused and changes nothing.
    std::fs::copy(VELMUX, bin.join("velmux")).unwrap();
    std::fs::write(root.join("release/new.tar.gz.sha256"), "0000  new.tar.gz\n").unwrap();
    let (ok, said) = run(&["update"]);
    assert!(!ok && said.contains("checksum"), "{said}");
    let (_, now) = run(&["--version"]);
    assert!(now.contains(env!("CARGO_PKG_VERSION")), "left as it was: {now}");
}

#[test]
fn a_codex_conversation_is_followed_and_resumed() {
    let env = Env::new();
    let repo = env.repo("app");
    let ui = env.attach(&repo);
    // Pick Codex for a new conversation in the repo.
    ui.click_on("◆ base", true);
    ui.wait_for("new conversation with…");
    let menu = ui.screen();
    let down = menu.lines().skip_while(|l| !l.contains("new conversation")).position(|l| l.contains("new conversation with…")).unwrap();
    for _ in 0..down {
        ui.send(b"j");
    }
    ui.send(b"\r");
    ui.wait_for("new conversation with…");
    ui.wait_for("Codex");
    let picker = ui.screen();
    let at = picker.lines().skip_while(|l| !l.contains("new conversation with…")).skip(1).position(|l| l.contains("Codex")).unwrap();
    for _ in 0..at {
        ui.send(b"j");
    }
    ui.send(b"\r");
    let screen = ui.wait_for("FAKE-CODEX id=");
    let id = screen.split("FAKE-CODEX id=").nth(1).unwrap().split_whitespace().next().unwrap().to_string();
    assert!(screen.contains("mode=new"));
    // velmux finds its session from the transcript (a few seconds), saves it,
    // and brings it back as `codex resume` after a stop.
    std::thread::sleep(Duration::from_secs(5));
    env.stop(&repo);
    drop(ui);
    let ui = env.attach(&repo);
    ui.wait_for(&format!("FAKE-CODEX id={id} mode=resume"));
}

#[test]
fn an_agent_with_a_session_hook_is_resumed() {
    let env = Env::new();
    let repo = env.repo("app");
    // Droid is set up: `skill install` gives it velmux's session hook.
    let factory = env.root.join("home/.factory");
    std::fs::create_dir_all(&factory).unwrap();
    std::fs::write(factory.join("settings.json"), r#"{"model":"mine"}"#).unwrap();
    let out = env.velmux(&repo, &["skill", "install"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("session hook for Droid"), "{}", String::from_utf8_lossy(&out.stdout));
    let settings = std::fs::read_to_string(factory.join("settings.json")).unwrap();
    assert!(settings.contains("agent-hook droid") && settings.contains("\"model\": \"mine\""), "{settings}");
    // Started by hand in a shell, its hook tells velmux the session…
    let ui = env.attach(&repo);
    ui.prefix("c");
    ui.wait_for("$");
    ui.line("droid");
    let screen = ui.wait_for("FAKE-DROID id=");
    let id = screen.split("FAKE-DROID id=").nth(1).unwrap().split_whitespace().next().unwrap().to_string();
    std::thread::sleep(Duration::from_secs(2));
    // …so after a stop it comes back as `droid --resume <id>`.
    env.stop(&repo);
    drop(ui);
    let ui = env.attach(&repo);
    ui.wait_for(&format!("FAKE-DROID id={id} mode=resume"));
    // And uninstalling takes the hook out again, leaving your settings.
    env.velmux(&repo, &["skill", "uninstall"]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(factory.join("settings.json")).unwrap()).unwrap(), serde_json::json!({"model":"mine"}));
}

#[test]
fn the_sidebar_resizes_by_its_edge_and_keys_and_keeps_its_width() {
    let env = Env::new();
    let repo = env.repo("app");
    let ui = env.attach(&repo);
    // The column of the sidebar's edge, on the second row.
    let edge = |ui: &common::Ui| ui.screen().lines().nth(1).and_then(|l| l.chars().position(|c| c == '│')).unwrap();
    let start = edge(&ui);
    // Press on the edge, drag it 10 columns right, let go (SGR mouse, 1-based).
    let row = 5;
    ui.send(format!("\x1b[<0;{};{row}M", start + 1).as_bytes());
    ui.send(format!("\x1b[<32;{};{row}M", start + 6).as_bytes());
    ui.send(format!("\x1b[<32;{};{row}M", start + 11).as_bytes());
    ui.send(format!("\x1b[<0;{};{row}m", start + 11).as_bytes());
    env.until("the edge to follow the drag", || edge(&ui) == start + 10);
    // The keys move it 4 at a time, and repeat without the prefix.
    ui.prefix("<");
    env.until("the sidebar to narrow", || edge(&ui) == start + 6);
    ui.prefix(">");
    ui.send(b">");
    env.until("a repeated > without the prefix", || edge(&ui) == start + 14);
    // It won't go narrower than 20 columns.
    ui.prefix("<");
    ui.send(b"<<<<<<<<");
    env.until("the sidebar to stop at its minimum", || edge(&ui) == 20);
    // A key after the repeat window goes to the pane again.
    std::thread::sleep(Duration::from_secs(1));
    ui.send(b">");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(edge(&ui), 20);
    ui.prefix(">");
    ui.prefix(">");
    env.until("the sidebar to widen", || edge(&ui) == 28);
    // It stays that wide after a restart.
    env.stop(&repo);
    drop(ui);
    let ui = env.attach(&repo);
    env.until("the width to come back", || edge(&ui) == 28);
}

#[test]
fn a_slots_conversations_are_named_after_it_unless_you_named_them() {
    let env = Env::new();
    let repo = env.repo("app");
    let ui = env.attach(&repo);
    ui.prefix("a");
    let id = ui.claude_id();
    ui.line("say hello");
    ui.line("claim feat/names");
    ui.wait_for("@1");
    let transcript = || {
        std::fs::read_dir(env.claude_dir.join("projects")).unwrap().flatten().map(|d| d.path().join(format!("{id}.jsonl"))).find(|p| p.is_file()).unwrap()
    };
    let title = || velmux_pool::claude::custom_title(&transcript());
    env.until("the conversation to be named after the branch", || title().as_deref() == Some("feat/names"));
    // Renaming the slot renames it too.
    ui.prefix("W");
    ui.wait_for("name");
    ui.line("checkout");
    env.until("the conversation to follow the slot's name", || title().as_deref() == Some("checkout"));
    // Once you've named it yourself (/rename), velmux leaves it alone.
    velmux_pool::claude::set_title(&transcript(), &id, "my own name").unwrap();
    ui.prefix("W");
    ui.wait_for("name");
    ui.send(&[0x15]); // clear the field
    ui.line("payments");
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(title().as_deref(), Some("my own name"));
}

#[test]
fn a_new_claude_from_a_slot_starts_in_the_checkout() {
    let env = Env::new();
    let repo = env.repo("app");
    assert!(env.velmux(&repo, &["init"]).status.success());
    assert!(env.velmux(&repo, &["claim", "feat/x", "--no-fetch", "--no-install"]).status.success());
    let ui = env.attach(&repo);
    ui.click_on("feat/x", false);
    ui.wait_for("app › slot-1");
    // From the slot's empty screen, and with the prefix key alike.
    ui.send(b"a");
    ui.wait_for("FAKE-CLAUDE id=");
    ui.wait_for("app › base");
    ui.click_on("feat/x", false);
    ui.wait_for("app › slot-1");
    ui.prefix("a");
    ui.wait_for("app › base");
}

#[test]
fn next_and_previous_place_follow_the_sidebar() {
    let env = Env::new();
    let repo = env.repo("app");
    assert!(env.velmux(&repo, &["init"]).status.success());
    for b in ["feat/a", "feat/b"] {
        assert!(env.velmux(&repo, &["claim", b, "--no-fetch", "--no-install"]).status.success());
    }
    let ui = env.attach(&repo);
    ui.wait_for("feat/b");
    // ⌘⌃] (as Ghostty sends it) steps through every place listed, panes or not.
    let next = || ui.send(b"\x1b[93;13u");
    for want in ["app › slot-1", "app › slot-2", "app › base"] {
        next();
        ui.wait_for(want);
    }
    ui.send(b"\x1b[91;13u");
    ui.wait_for("app › slot-2");
}

#[test]
fn focus_follows_the_mouse_when_turned_on() {
    let env = Env::new();
    let repo = env.repo("app");
    let config = env.root.join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("focus_follows_mouse = true\n{text}")).unwrap();
    let ui = env.attach(&repo);
    ui.prefix("c");
    ui.wait_for("$");
    ui.prefix("|"); // split right; the new right pane has focus
    std::thread::sleep(Duration::from_secs(1));
    let focused = || {
        let out = env.velmux(&repo, &["debug"]);
        String::from_utf8_lossy(&out.stdout).split_whitespace().nth(1).unwrap_or_default().to_string()
    };
    let right = focused();
    // Move (no button) over the left pane: SGR code 35.
    ui.send(b"\x1b[<35;40;10M");
    env.until("focus to follow the pointer left", || focused() != right);
    let left = focused();
    ui.send(b"\x1b[<35;110;10M");
    env.until("focus to follow it back right", || focused() == right);
    assert_ne!(left, right);
}
