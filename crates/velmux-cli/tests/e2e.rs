//! End-to-end tests (harness in `common/`).

mod common;

use std::path::Path;
use std::process::Output;
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
    ui.wait_for("feat/moved* ·"); // dirty: the changes came along
    let slot = env.json(&repo, &["status", "--json"]).as_array().unwrap().iter().find(|s| s["branch"] == "feat/moved").cloned().unwrap();
    let path = Path::new(slot["path"].as_str().unwrap());
    assert_eq!(std::fs::read_to_string(path.join("README.md")).unwrap(), "edited in the checkout\n");
    assert!(path.join("notes.txt").exists());
    assert_eq!(env.run_in(&repo, "git status --porcelain"), "", "the checkout is clean");
    ui.wait_gone_text("✎2");
}
