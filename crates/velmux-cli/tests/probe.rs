//! TEMPORARY: keys just after a new Windows pane starts.
#![cfg(windows)]
mod common;

use std::time::Duration;

use common::*;

fn shot(ui: &Ui, what: &str) {
    std::thread::sleep(Duration::from_millis(1500));
    let s = ui.screen();
    let pane: Vec<&str> = s.lines().take(8).collect();
    eprintln!("PROBE {what}:\n{}", pane.join("\n"));
}

fn run(key: &str) {
    let env = Env::new();
    let repo = env.repo("app");
    let ui = env.attach(&repo);
    ui.prefix("c");
    ui.wait_for(PROMPT);
    ui.prefix(key);
    shot(&ui, &format!("prefix {key:?} straight after the prompt"));
    ui.send(b"x");
    shot(&ui, "then x");
    let out = env.velmux(&repo, &["debug"]);
    eprintln!("PROBE debug: {}", String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or_default());
}

#[test]
fn probe() {
    run(" ");
    run("|");
}
