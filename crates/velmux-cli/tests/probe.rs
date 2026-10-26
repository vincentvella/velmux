//! TEMPORARY: how a lone space reaches a Windows pane.
#![cfg(windows)]
mod common;

use common::*;

#[test]
fn probe() {
    let env = Env::new();
    let repo = env.repo("app");
    let ui = env.attach(&repo);
    ui.prefix("c");
    ui.wait_for(PROMPT);
    for k in ["e", "c", "h", "o", " ", "'", "x", "y", "'"] {
        ui.send(k.as_bytes());
    }
    std::thread::sleep(std::time::Duration::from_secs(1));
    eprintln!("PROBE typed one by one:\n{}", ui.screen());
    ui.send(b"\r");
    ui.send(b"\x01");
    ui.send(b" ");
    std::thread::sleep(std::time::Duration::from_secs(2));
    eprintln!("PROBE prefix then space:\n{}", ui.screen());
    ui.send(b"\x1b");
    ui.send(b"\x01 ");
    std::thread::sleep(std::time::Duration::from_secs(2));
    eprintln!("PROBE prefix+space in one write:\n{}", ui.screen());
    eprintln!("PROBE debug: {}", String::from_utf8_lossy(&env.velmux(&repo, &["debug"]).stdout).lines().next().unwrap_or_default());
}
