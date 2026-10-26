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
    ui.wait_for(PROMPT);
    ui.line(if cfg!(windows) { "1..80 | % { \"e2e-line-$_\" }" } else { "i=1; while [ $i -le 80 ]; do echo e2e-line-$i; i=$((i+1)); done" });
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

// (On Windows a restart can't hand the panes over: it restarts them.)
#[cfg(unix)]
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

    // The scratch heading's +: a shell in the scratch dir, as its own session.
    let plus = || {
        let y = ui.find("SCRATCH").map(|(_, y)| y).expect("the scratch heading");
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

// (On Windows, updates come as a zip, unpacked by PowerShell.)
#[cfg(unix)]
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
    ui.wait_for(PROMPT);
    ui.line("droid");
    let screen = ui.wait_for("FAKE-DROID id=");
    let id = screen.split("FAKE-DROID id=").nth(1).unwrap().split_whitespace().next().unwrap().to_string();
    std::thread::sleep(Duration::from_secs(2));
    // …so after a stop it comes back as `droid --resume <id>`.
    env.stop(&repo);
    drop(ui);
    let ui = env.attach(&repo);
    ui.wait_for(&format!("FAKE-DROID id={id} mode=resume"));
    // Its permission menu shows as blocked (herdr's rules for Droid).
    ui.line("ask");
    ui.wait_for("Enter to select");
    env.until("the sidebar to show it blocked", || ui.find("▲").is_some());
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
    let transcript = || {
        std::fs::read_dir(env.claude_dir.join("projects")).unwrap().flatten().map(|d| d.path().join(format!("{id}.jsonl"))).find(|p| p.is_file()).unwrap()
    };
    let title = || velmux_pool::claude::custom_title(&transcript());
    // In the checkout: the repo, then with Claude's own title once it has one.
    env.until("a checkout conversation to be named after its repo", || title().as_deref() == Some("app"));
    ui.line("aititle Plan the login flow");
    ui.wait_for("titled ");
    env.until("Claude's title to join it", || title().as_deref() == Some("app · Plan the login flow"));
    ui.line("claim feat/names");
    ui.wait_for("@1");
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
fn a_workstream_groups_slots_from_several_repos() {
    let env = Env::new();
    let app = env.repo("app");
    let api = env.repo("api");
    for repo in [&app, &api] {
        assert!(env.velmux(repo, &["init"]).status.success());
    }
    assert!(env.velmux(&app, &["claim", "feat/a", "--no-fetch", "--no-install"]).status.success());
    let ui = env.attach(&app);
    // Start one from a slot's menu: the slot moves under it.
    ui.click_on("feat/a", true);
    ui.click_on("add to workstream…", false);
    ui.click_on("new workstream…", false);
    ui.wait_for("name for the workstream");
    ui.line("checkout");
    ui.wait_for("WORKSTREAMS");
    ui.wait_for("◇ checkout");
    ui.wait_for("@1 app");
    let dir = env.root.join("pools/.workstreams/checkout");
    let app_slot = env.slot(&app, "slot-1")["path"].as_str().unwrap().to_string();
    env.until("the folder to link the app's slot", || velmux_pool::sys::read_link(&dir.join("app")).is_ok_and(|t| t.to_string_lossy() == app_slot));
    // From a shell in the workstream, claim a slot in another repo into it.
    ui.send(b"c");
    ui.wait_for(PROMPT);
    ui.line(&format!("velmux claim feat/b --repo {} --no-fetch", api.display()));
    ui.wait_for("into the workstream checkout");
    ui.wait_for("@1 api");
    let api_slot = env.slot(&api, "slot-1");
    assert_eq!(api_slot["status"], "inuse");
    env.until("the folder to link the api's slot too", || velmux_pool::sys::read_link(&dir.join("api")).is_ok());
    // `velmux workstream`: from anywhere by name (JSON for scripts)…
    env.until("velmux workstream to list both slots", || {
        let w = env.json(&app, &["workstream", "checkout", "--json"]);
        let slots = w["slots"].as_array().cloned().unwrap_or_default();
        let has = |repo: &str, branch: &str| slots.iter().any(|s| s["repo"] == repo && s["branch"] == branch);
        w["name"] == "checkout" && has("app", "feat/a") && has("api", "feat/b")
    });
    // …and, inside it, the one you're in.
    ui.line("velmux workstream");
    ui.wait_for("checkout  (");
    // A shell open in one of its slots…
    ui.click_on("feat/a", false);
    ui.wait_for("app › slot-1");
    ui.send(b"c");
    ui.wait_for(PROMPT);
    ui.line("echo still-here");
    ui.wait_for("still-here");
    // Releasing it hands every slot back and removes it…
    ui.click_on("◇ checkout", true);
    ui.click_on("release workstream…", false);
    ui.wait_for("y yes");
    ui.send(b"y");
    env.until("both slots to be free", || env.slot(&app, "slot-1")["status"] == "free" && env.slot(&api, "slot-1")["status"] == "free");
    ui.wait_gone_text("◇ checkout");
    // …the shells (the slot's, and the workstream's own) are still running, in
    // app's checkout…
    ui.click_on("◆ base", false);
    ui.wait_for("app › base · shell");
    ui.wait_for("still-here");
    ui.wait_for("into the workstream checkout");
    ui.line("echo alive");
    ui.wait_for("alive");
    // …and its folder, emptied of links, goes once nothing works in it.
    assert!(std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0) == 0, "no links left");
    // (Both shells are in the checkout now: leave the folder in each.
    // PowerShell's `cd` keeps the process where it was: move that too.)
    let leave = if cfg!(windows) { "cd /; [Environment]::CurrentDirectory = 'C:\\'" } else { "cd /" };
    ui.line(leave);
    ui.prefix("j");
    ui.line(leave);
    env.until("its folder to go", || !dir.exists());
}

#[test]
fn slots_drag_into_a_workstream_and_leave_it() {
    let env = Env::new();
    let app = env.repo("app");
    assert!(env.velmux(&app, &["init"]).status.success());
    assert!(env.velmux(&app, &["claim", "feat/drag", "--no-fetch", "--no-install"]).status.success());
    let ui = env.attach(&app);
    ui.wait_for("feat/drag");
    // `+` on the heading starts an empty one.
    let (_, y) = ui.find("WORKSTREAMS").unwrap();
    let plus = ui.screen().lines().nth(y as usize).unwrap().chars().take(32).collect::<String>().rfind('+').unwrap();
    ui.send(format!("\x1b[<0;{};{}M\x1b[<0;{};{}m", plus + 1, y + 1, plus + 1, y + 1).as_bytes());
    ui.wait_for("name for the workstream");
    ui.line("dragged");
    ui.wait_for("◇ dragged");
    // Drag the slot onto it (SGR: press, move, release).
    let (sx, sy) = ui.find("feat/drag").unwrap();
    let (_, wy) = ui.find("◇ dragged").unwrap();
    ui.send(format!("\x1b[<0;{};{}M", sx + 2, sy + 1).as_bytes());
    ui.send(format!("\x1b[<32;{};{}M", sx + 2, sy).as_bytes());
    ui.send(format!("\x1b[<32;{};{}M", sx + 2, wy + 1).as_bytes());
    ui.send(format!("\x1b[<0;{};{}m", sx + 2, wy + 1).as_bytes());
    ui.wait_for("@1 app");
    // It's all still there after the server stops and starts again.
    env.stop(&app);
    drop(ui);
    let ui = env.attach(&app);
    ui.wait_for("◇ dragged");
    ui.wait_for("@1 app");
    // Its menu takes it out again: back under its repo.
    ui.click_on("feat/drag", true);
    ui.click_on("leave workstream", false);
    env.until("the slot to be back with its repo", || {
        let (repos, slot) = (ui.find("REPOS"), ui.find("▣ feat/drag"));
        !ui.screen().contains("@1 app") && matches!((repos, slot), (Some((_, r)), Some((_, s))) if r < s)
    });
    // A conversation in the workstream is named after it.
    ui.click_on("◇ dragged", false);
    ui.wait_for("a workstream:");
    ui.send(b"a");
    let id = ui.claude_id();
    ui.line("say hi");
    let transcript = || std::fs::read_dir(env.claude_dir.join("projects")).into_iter().flatten().flatten().map(|d| d.path().join(format!("{id}.jsonl"))).find(|p| p.is_file());
    let start = std::time::Instant::now();
    while transcript().and_then(|t| velmux_pool::claude::custom_title(&t)).as_deref() != Some("dragged") {
        assert!(
            start.elapsed() < WAIT,
            "its conversation wasn't named after the workstream: {:?}\n{}\n{}",
            transcript(),
            transcript().and_then(|t| std::fs::read_to_string(t).ok()).unwrap_or_default(),
            ui.report()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_claude_claims_in_another_repo_and_comes_back_there() {
    let env = Env::new();
    let app = env.repo("app");
    let api = env.repo("api");
    for repo in [&app, &api] {
        assert!(env.velmux(repo, &["init"]).status.success());
    }
    let ui = env.attach(&app);
    ui.prefix("a");
    let id = ui.claude_id();
    // From app's checkout, a slot in api: the conversation moves over there.
    ui.line(&format!("claim feat/x --repo {}", api.display()));
    ui.wait_for("claimed ");
    ui.wait_for("api › slot-1 · feat/x");
    let slot = env.slot(&api, "slot-1");
    assert_eq!((slot["status"].as_str(), slot["branch"].as_str()), (Some("inuse"), Some("feat/x")));
    assert_eq!(env.slot(&app, "slot-1")["status"], "free", "nothing claimed in app");
    assert!(ui.screen().contains(&format!("FAKE-CLAUDE id={id}")), "the same conversation, moved");
    // Released, it comes back to api's checkout (the repo the slot was in).
    ui.line("release");
    env.until("api's slot to be free", || env.slot(&api, "slot-1")["status"] == "free");
    ui.wait_for("api › base");
    assert!(ui.screen().contains(&format!("FAKE-CLAUDE id={id}")), "still the same conversation");
}

#[test]
fn a_workstream_slot_released_from_its_own_pane_leaves_it() {
    let env = Env::new();
    let app = env.repo("app");
    assert!(env.velmux(&app, &["init"]).status.success());
    assert!(env.velmux(&app, &["claim", "feat/w", "--no-fetch", "--no-install"]).status.success());
    let ui = env.attach(&app);
    ui.click_on("feat/w", true);
    ui.click_on("add to workstream…", false);
    ui.click_on("new workstream…", false);
    ui.wait_for("name for the workstream");
    ui.line("ws");
    ui.wait_for("@1 app");
    let link = env.root.join("pools/.workstreams/ws/app");
    env.until("the link", || velmux_pool::sys::read_link(&link).is_ok());
    // A shell in the slot (under the workstream) releases it.
    ui.click_on("feat/w", false);
    ui.wait_for("app › slot-1");
    ui.send(b"c");
    ui.wait_for(PROMPT);
    ui.line("velmux release");
    env.until("the slot to be free", || env.slot(&app, "slot-1")["status"] == "free");
    // The pane is back in app's checkout; the slot has left the workstream (which stays).
    ui.wait_for("app › base");
    ui.wait_gone_text("@1 app");
    ui.wait_for("◇ ws");
    env.until("its link to go", || std::fs::symlink_metadata(&link).is_err());
}

#[test]
fn releasing_a_slot_from_its_menu_moves_its_panes_back() {
    let env = Env::new();
    let app = env.repo("app");
    assert!(env.velmux(&app, &["init"]).status.success());
    assert!(env.velmux(&app, &["claim", "feat/menu", "--no-fetch", "--no-install"]).status.success());
    let ui = env.attach(&app);
    ui.click_on("feat/menu", false);
    ui.wait_for("app › slot-1");
    ui.send(b"c");
    ui.wait_for(PROMPT);
    ui.line("echo still-here");
    ui.wait_for("still-here");
    ui.click_on("feat/menu", true);
    ui.click_on("release slot", false);
    ui.wait_for("its panes move back to the");
    ui.send(b"y");
    env.until("the slot to be free", || env.slot(&app, "slot-1")["status"] == "free");
    // The shell is still running, now in the checkout.
    ui.wait_for("app › base · shell");
    ui.wait_for("still-here");
    ui.line("echo alive");
    ui.wait_for("alive");
}

#[test]
fn a_workstream_claims_a_slot_in_a_repo_from_its_screen() {
    let env = Env::new();
    let app = env.repo("app");
    let api = env.repo("api");
    for repo in [&app, &api] {
        assert!(env.velmux(repo, &["init"]).status.success());
    }
    // Both repos open in velmux (attaching in a repo adds it).
    drop(env.attach(&api));
    let ui = env.attach(&app);
    ui.wait_for("api");
    let (_, y) = ui.find("WORKSTREAMS").unwrap();
    let plus = ui.screen().lines().nth(y as usize).unwrap().chars().take(32).collect::<String>().rfind('+').unwrap();
    ui.send(format!("\x1b[<0;{};{}M\x1b[<0;{};{}m", plus + 1, y + 1, plus + 1, y + 1).as_bytes());
    ui.wait_for("name for the workstream");
    ui.line("pair");
    ui.wait_for("claim a slot in a repo");
    // n: pick the repo, then name the branch.
    ui.send(b"n");
    ui.wait_for("a slot in…");
    let menu = ui.screen();
    let at = menu.lines().skip_while(|l| !l.contains("a slot in…")).skip(1).position(|l| l.contains(" api")).unwrap();
    for _ in 0..at {
        ui.send(b"j");
    }
    ui.send(b"\r");
    ui.wait_for("branch for a slot in api");
    ui.line("feat/pair");
    env.until("api's slot to be claimed", || env.slot(&api, "slot-1")["status"] == "inuse");
    ui.wait_for("@1 api");
    assert_eq!(env.slot(&api, "slot-1")["branch"], "feat/pair");
    assert_eq!(env.slot(&app, "slot-1")["status"], "free");
    env.until("its link", || velmux_pool::sys::read_link(&env.root.join("pools/.workstreams/pair/api")).is_ok());
}

#[test]
fn a_panes_right_click_menu_splits_zooms_and_closes_it() {
    let env = Env::new();
    let repo = env.repo("app");
    let ui = env.attach(&repo);
    ui.prefix("c");
    ui.wait_for(PROMPT);
    let right_click = |x: u16, y: u16| ui.send(format!("\x1b[<2;{x};{y}M\x1b[<2;{x};{y}m").as_bytes());
    let count = |needle: &str| ui.screen().matches(needle).count();
    // Right-click in the pane: its menu; the first item splits it.
    right_click(70, 12);
    ui.wait_for("split right");
    ui.wait_for("close pane");
    ui.send(b"\r");
    // Split panes get a title bar each.
    let titles = || ui.screen().lines().nth(1).map_or(0, |l| l.matches("   shell").count());
    env.until("two panes, each with a title", || titles() == 2);
    // Now it can zoom (and unzoom) from the menu too.
    right_click(110, 12);
    ui.wait_for("zoom");
    let menu = ui.screen();
    let at = menu.lines().skip_while(|l| !l.contains("split right")).position(|l| l.contains(" zoom")).unwrap();
    for _ in 0..at {
        ui.send(b"j");
    }
    ui.send(b"\r");
    env.until("one pane shown, zoomed", || titles() <= 1);
    right_click(70, 12);
    ui.wait_for("unzoom");
    // And close it: back to one pane.
    let menu = ui.screen();
    let at = menu.lines().skip_while(|l| !l.contains("split right")).position(|l| l.contains("close pane")).unwrap();
    for _ in 0..at {
        ui.send(b"j");
    }
    ui.send(b"\r");
    env.until("one pane left", || {
        let s = ui.screen();
        titles() == 0 && !s.contains("close pane") && count(PROMPT) >= 1
    });
}

// (Windows' ConPTY keeps a program's mouse reporting to itself, so velmux
// can't see it there.)
#[cfg(unix)]
#[test]
fn an_agent_that_takes_the_mouse_still_gets_the_pane_menu() {
    let env = Env::new();
    let repo = env.repo("app");
    let ui = env.attach(&repo);
    ui.prefix("a");
    ui.claude_id();
    // Claude Code turns mouse reporting on: a right-click is still velmux's menu.
    ui.line("mouse");
    ui.line("say ready");
    ui.wait_for("said ready");
    let debug = || String::from_utf8_lossy(&env.velmux(&repo, &["debug"]).stdout).into_owned();
    let start = std::time::Instant::now();
    while !debug().contains("mouse ButtonMotion") {
        assert!(start.elapsed() < WAIT, "the pane never reported mouse mode: {}", debug());
        std::thread::sleep(Duration::from_millis(100));
    }
    ui.send(b"\x1b[<2;80;12M\x1b[<2;80;12m");
    ui.wait_for("split right");
}

#[test]
fn dragging_a_selection_to_a_panes_edge_scrolls_both_ways() {
    let env = Env::new();
    let repo = env.repo("app");
    let ui = env.attach(&repo);
    ui.prefix("c");
    ui.wait_for(PROMPT);
    // (PowerShell echoes `1..400` itself: wait for what follows the numbers.)
    let (list, done) = if cfg!(windows) { ("1..400; 'list' + 'ed'", "listed") } else { ("seq 1 400", "400") };
    ui.line(list);
    ui.wait_for(done);
    // Scroll well back into history with the wheel (SGR 64 = wheel up).
    for _ in 0..40 {
        ui.send(b"\x1b[<64;80;10M");
    }
    // The number on the pane's first row (just under its title).
    let top = |ui: &Ui| {
        let s = ui.screen();
        s.lines().nth(1).and_then(|l| l.split('│').nth(1)).and_then(|t| t.split_whitespace().next()?.parse::<u32>().ok()).unwrap_or(0)
    };
    env.until("the view to be back in history", || (1..300).contains(&top(&ui)));
    let before = top(&ui);
    // Drag from the middle onto the pane's last row (the screen's foot) and hold.
    ui.send(b"\x1b[<0;80;10M");
    ui.send(b"\x1b[<32;80;20M");
    ui.send(format!("\x1b[<32;80;{ROWS}M").as_bytes());
    env.until("the view to scroll down by itself", || top(&ui) > before + 20);
    // Held above the top edge, it scrolls back up the same way.
    let lower = top(&ui);
    ui.send(b"\x1b[<32;80;1M");
    env.until("the view to scroll up by itself", || top(&ui) + 20 < lower);
    ui.send(b"\x1b[<0;80;1m");
}

#[test]
fn a_workstream_sums_up_its_prs_and_opens_them() {
    let env = Env::new();
    let app = env.repo("app");
    let api = env.repo("api");
    for (repo, name) in [(&app, "app"), (&api, "api")] {
        env.run_in(repo, &format!("git remote add origin https://github.com/acme/{name}.git"));
        assert!(env.velmux(repo, &["init"]).status.success());
    }
    assert!(env.velmux(&app, &["claim", "feat/a", "--no-fetch", "--no-install"]).status.success());
    assert!(env.velmux(&api, &["claim", "feat/b", "--no-fetch", "--no-install"]).status.success());
    let prs = r#"[
      {"headRefName":"feat/a","number":11,"url":"https://github.com/acme/app/pull/11","state":"OPEN","isDraft":false,
       "statusCheckRollup":[{"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":"FAILURE"}]},
      {"headRefName":"feat/b","number":22,"url":"https://github.com/acme/api/pull/22","state":"OPEN","isDraft":false,
       "statusCheckRollup":[{"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":"SUCCESS"}]}
    ]"#;
    std::fs::write(env.root.join("prs.json"), prs).unwrap();
    drop(env.attach(&api));
    let ui = env.attach(&app);
    ui.wait_for("#11✗");
    ui.wait_for("#22✓");
    // Both slots into one workstream.
    ui.click_on("feat/a", true);
    ui.click_on("add to workstream…", false);
    ui.click_on("new workstream…", false);
    ui.wait_for("name for the workstream");
    ui.line("pair");
    ui.click_on("feat/b", true);
    ui.click_on("add to workstream…", false);
    ui.wait_for("new workstream…");
    ui.send(b"\r"); // its first item: the workstream "pair"
    // Its row sums them up: two PRs, one failing.
    ui.wait_for("2 PR ✗1");
    // And opens them all.
    ui.click_on("◇ pair", true);
    ui.click_on("open all PRs", false);
    let opened = env.root.join("opened.txt");
    env.until("both PRs to open", || {
        let t = std::fs::read_to_string(&opened).unwrap_or_default();
        t.contains("acme/app/pull/11") && t.contains("acme/api/pull/22")
    });
    // Both merge: it's done, says so, and `⏏ release` on its row hands it all back.
    std::fs::write(env.root.join("prs.json"), prs.replace("\"OPEN\"", "\"MERGED\"")).unwrap();
    ui.wait_for("all 2 PRs merged");
    ui.wait_for("⏏ release");
    ui.click_on("⏏ release", false);
    ui.wait_for("y yes");
    ui.send(b"y");
    env.until("both slots to be free", || env.slot(&app, "slot-1")["status"] == "free" && env.slot(&api, "slot-1")["status"] == "free");
}

#[test]
fn a_conversation_to_the_right_starts_beside_the_pane() {
    let env = Env::new();
    let repo = env.repo("app");
    assert!(env.velmux(&repo, &["init"]).status.success());
    assert!(env.velmux(&repo, &["claim", "feat/side", "--no-fetch", "--no-install"]).status.success());
    let ui = env.attach(&repo);
    ui.click_on("feat/side", false);
    ui.wait_for("app › slot-1");
    ui.send(b"c");
    ui.wait_for(PROMPT);
    ui.send(b"\x1b[<2;70;12M\x1b[<2;70;12m");
    ui.wait_for("conversation to the right");
    let menu = ui.screen();
    let at = menu.lines().skip_while(|l| !l.contains("split right")).position(|l| l.contains("conversation to the right")).unwrap();
    for _ in 0..at {
        ui.send(b"j");
    }
    ui.send(b"\r");
    // A Claude beside the shell, in the slot (you chose the spot), not the checkout.
    ui.wait_for("FAKE-CLAUDE id=");
    ui.wait_for("app › slot-1");
    assert!(!ui.screen().contains("app › base"), "{}", ui.screen());
}

#[test]
fn a_released_workstream_comes_back_whole() {
    let env = Env::new();
    let app = env.repo("app");
    let api = env.repo("api");
    for repo in [&app, &api] {
        assert!(env.velmux(repo, &["init"]).status.success());
    }
    for (repo, branch) in [(&app, "feat/a"), (&api, "feat/b")] {
        assert!(env.velmux(repo, &["claim", branch, "--no-fetch", "--no-install"]).status.success());
        let slot = env.slot(repo, "slot-1");
        env.run_in(Path::new(slot["path"].as_str().unwrap()), "echo wip > wip.txt");
    }
    drop(env.attach(&api));
    let ui = env.attach(&app);
    ui.wait_for("feat/b");
    for branch in ["feat/a", "feat/b"] {
        ui.click_on(branch, true);
        ui.click_on("add to workstream…", false);
        if branch == "feat/a" {
            ui.click_on("new workstream…", false);
            ui.wait_for("name for the workstream");
            ui.line("pair");
        } else {
            ui.wait_for("new workstream…");
            ui.send(b"\r");
        }
    }
    ui.wait_for("@1 api");
    // Released half-way: both branches park, and it's kept as one.
    ui.click_on("◇ pair", true);
    ui.click_on("release workstream…", false);
    ui.wait_for("y yes");
    ui.send(b"y");
    env.until("both slots free", || env.slot(&app, "slot-1")["status"] == "free" && env.slot(&api, "slot-1")["status"] == "free");
    ui.wait_for("◌ pair");
    ui.wait_for("2 branches");
    assert!(ui.find("◌ feat/a").is_none(), "its branches are listed with it, not their repos:\n{}", ui.screen());
    // Still there after the server stops and starts again.
    env.stop(&app);
    drop(ui);
    let ui = env.attach(&app);
    ui.wait_for("◌ pair");
    // Resume it: a workstream again, both branches claimed back with their work.
    ui.click_on("◌ pair", false);
    ui.wait_for("resume workstream");
    ui.send(b"\r");
    ui.wait_for("◇ pair");
    // (In whichever slot each claim took.)
    let slot_on = |repo: &Path, branch: &str| {
        env.json(repo, &["status", "--json"]).as_array().unwrap().iter().find(|s| s["branch"] == branch && s["status"] == "inuse").cloned()
    };
    env.until("both branches back in slots", || slot_on(&app, "feat/a").is_some() && slot_on(&api, "feat/b").is_some());
    ui.wait_for(" app");
    ui.wait_for(" api");
    for (repo, branch) in [(&app, "feat/a"), (&api, "feat/b")] {
        let slot = slot_on(repo, branch).unwrap();
        assert!(Path::new(slot["path"].as_str().unwrap()).join("wip.txt").exists(), "its uncommitted work came back");
    }
    ui.wait_gone_text("◌ pair");
}

#[test]
fn prefix_space_opens_the_menu_for_whats_focused() {
    let env = Env::new();
    let repo = env.repo("app");
    assert!(env.velmux(&repo, &["init"]).status.success());
    assert!(env.velmux(&repo, &["claim", "feat/keys", "--no-fetch", "--no-install"]).status.success());
    let ui = env.attach(&repo);
    ui.click_on("feat/keys", false);
    ui.wait_for("app › slot-1");
    // An empty place: its own menu. (Waiting for items the keys popup, up
    // between the prefix and the key, doesn't list.)
    ui.prefix(" ");
    ui.wait_for("copy path");
    ui.send(b"\x1b");
    ui.wait_gone_text("copy path");
    // A pane: the pane's menu, whose last item is its place's.
    ui.send(b"c");
    ui.wait_for(PROMPT);
    ui.prefix(" ");
    ui.wait_for("conversation to the right");
    let menu = ui.screen();
    let at = menu.lines().skip_while(|l| !l.contains("split right")).position(|l| l.contains("feat/keys…")).unwrap_or_else(|| panic!("no feat/keys… in the menu:\n{menu}"));
    for _ in 0..at {
        ui.send(b"j");
    }
    ui.send(b"\r");
    ui.wait_for("copy path");
}

#[test]
fn a_workstream_syncs_all_its_slots() {
    let env = Env::new();
    let app = env.repo("app");
    assert!(env.velmux(&app, &["init"]).status.success());
    assert!(env.velmux(&app, &["claim", "feat/s", "--no-fetch", "--no-install"]).status.success());
    let ui = env.attach(&app);
    ui.click_on("feat/s", true);
    ui.click_on("add to workstream…", false);
    ui.click_on("new workstream…", false);
    ui.wait_for("name for the workstream");
    ui.line("syncs");
    ui.wait_for("@1 app");
    // main moves on: the slot falls behind…
    env.run_in(&app, "echo more > more.txt && git add -A && git commit -qm more");
    ui.wait_for("↓1");
    // …and the workstream's menu brings every slot up to date.
    ui.click_on("◇ syncs", true);
    ui.click_on("sync all slots", false);
    env.until("the slot to be up to date", || env.slot(&app, "slot-1")["behind"] == 0);
    ui.wait_gone_text("↓1");
}

#[test]
fn a_workstream_pushes_its_branches_for_prs() {
    let env = Env::new();
    let app = env.repo("app");
    let api = env.repo("api");
    for (repo, name) in [(&app, "app"), (&api, "api")] {
        let bare = env.root.join(format!("remotes/{name}.git"));
        env.run_in(repo, &format!("git clone -q --bare . '{0}' && git remote add origin '{0}' && git fetch -q origin", bare.display()));
        assert!(env.velmux(repo, &["init"]).status.success());
    }
    for (repo, branch) in [(&app, "feat/p"), (&api, "feat/q")] {
        assert!(env.velmux(repo, &["claim", branch, "--no-fetch", "--no-install"]).status.success());
        let slot = env.slot(repo, "slot-1");
        env.run_in(Path::new(slot["path"].as_str().unwrap()), "echo x > x && git add x && git commit -qm x");
    }
    drop(env.attach(&api));
    let ui = env.attach(&app);
    ui.wait_for("feat/q");
    for branch in ["feat/p", "feat/q"] {
        ui.click_on(branch, true);
        ui.click_on("add to workstream…", false);
        if branch == "feat/p" {
            ui.click_on("new workstream…", false);
            ui.wait_for("name for the workstream");
            ui.line("both");
        } else {
            ui.wait_for("new workstream…");
            ui.send(b"\r");
        }
    }
    ui.wait_for("@1 api");
    // One action pushes every slot without a PR and opens one for each.
    ui.click_on("◇ both", true);
    ui.click_on("push & open PRs", false);
    let pushed = |name: &str, branch: &str| {
        let out = Command::new("git").args(["ls-remote", "--heads"]).arg(env.root.join(format!("remotes/{name}.git"))).arg(branch).output().unwrap();
        !out.stdout.is_empty()
    };
    env.until("both branches pushed", || pushed("app", "feat/p") && pushed("api", "feat/q"));
    ui.wait_for("finish the PRs in your browser");
}

#[test]
fn old_parked_work_is_flagged_and_discarded_from_the_repos_menu() {
    let env = Env::new();
    let app = env.repo("app");
    assert!(env.velmux(&app, &["init"]).status.success());
    assert!(env.velmux(&app, &["claim", "feat/stale", "--no-fetch", "--no-install"]).status.success());
    let slot = env.slot(&app, "slot-1");
    env.run_in(Path::new(slot["path"].as_str().unwrap()), "echo wip > wip.txt");
    assert!(env.velmux(&app, &["release", "slot-1"]).status.success());
    // Parked 40 days ago.
    let state = env.root.join("pools/app/pool.json");
    let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    v["parked"][0]["parked_at"] = serde_json::json!(now - 40 * 86400);
    std::fs::write(&state, v.to_string()).unwrap();
    let ui = env.attach(&app);
    ui.wait_for("◌ feat/stale");
    ui.wait_for("40d");
    // The repo's menu offers to clear it out.
    ui.click_on("app", true);
    ui.click_on("discard parked work older", false);
    ui.wait_for("y yes");
    ui.send(b"y");
    ui.wait_gone_text("◌ feat/stale");
    assert!(env.json(&app, &["parked", "--json"]).as_array().unwrap().is_empty());
}

#[test]
fn the_agents_view_puts_what_needs_you_first_then_groups_the_rest() {
    let env = Env::new();
    let app = env.repo("app");
    let api = env.repo("api");
    drop(env.attach(&api));
    let ui = env.attach(&app);
    // A Claude in each repo's checkout.
    ui.prefix("a");
    ui.claude_id();
    ui.click_on("api", false);
    ui.wait_for("api › base");
    ui.send(b"a");
    ui.claude_id();
    ui.line("usage 84000");
    ui.wait_for("used 84000");
    ui.prefix("g");
    ui.wait_for("APP");
    ui.wait_for("API");
    assert!(!ui.screen().contains("NEEDS YOU"));
    // How full each Claude's context is.
    ui.wait_for("84k context");
    // api's asks for permission: it moves up, under NEEDS YOU, saying for how long.
    let ask = env.root.join("ask.ans");
    std::fs::write(&ask, "Bash command\n  rm -rf dist\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel\n").unwrap();
    ui.line(&format!("show {}", ask.display()));
    ui.wait_for("NEEDS YOU");
    ui.wait_for("blocked");
    let (needs, apps) = (ui.find("NEEDS YOU").unwrap().1, ui.find("APP").unwrap().1);
    let api_row = ui.find("api › base").unwrap().1;
    assert!(needs < api_row && api_row < apps, "the blocked one first:\n{}", ui.screen());
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
    ui.wait_for(PROMPT);
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
