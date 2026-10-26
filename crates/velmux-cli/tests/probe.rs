//! TEMPORARY: prefix space in a slot on Windows, step by step.
#![cfg(windows)]
mod common;

use std::time::Duration;

use common::*;

fn shot(ui: &Ui, what: &str) {
    std::thread::sleep(Duration::from_millis(800));
    let s = ui.screen();
    let pane: Vec<&str> = s.lines().take(14).collect();
    eprintln!("PROBE {what}:\n{}", pane.join("\n"));
}

#[test]
fn probe() {
    let env = Env::new();
    let repo = env.repo("app");
    assert!(env.velmux(&repo, &["init"]).status.success());
    assert!(env.velmux(&repo, &["claim", "feat/keys", "--no-fetch", "--no-install"]).status.success());
    let ui = env.attach(&repo);
    ui.click_on("feat/keys", false);
    ui.wait_for("app › slot-1");
    shot(&ui, "clicked");
    ui.prefix(" ");
    shot(&ui, "prefix space in the empty slot");
    ui.send(b"\x1b");
    shot(&ui, "esc");
    ui.send(b"c");
    ui.wait_for(PROMPT);
    shot(&ui, "shell");
    ui.prefix(" ");
    shot(&ui, "prefix space in the shell");
    ui.send(b" ");
    shot(&ui, "another space");
}
