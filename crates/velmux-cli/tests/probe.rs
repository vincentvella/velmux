//! TEMPORARY: what a Windows runner's ConPTY and server start do.
#![cfg(windows)]
mod common;

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

fn pty_run(prog: &str, args: &[&str], env: Option<&Env>, secs: u64) {
    let pair = native_pty_system().openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }).unwrap();
    let mut cmd = CommandBuilder::new(prog);
    cmd.args(args);
    if let Some(env) = env {
        for (k, v) in env.vars() {
            cmd.env(k, v);
        }
        cmd.cwd(&env.root);
    }
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
    let raw = Arc::new(Mutex::new(Vec::new()));
    let (w, r) = (writer.clone(), raw.clone());
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            r.lock().unwrap().extend_from_slice(&buf[..n]);
            if buf[..n].windows(4).any(|q| q == b"\x1b[6n") {
                let mut w = w.lock().unwrap();
                let _ = w.write_all(b"\x1b[1;1R");
                let _ = w.flush();
                eprintln!("  (replied to 6n)");
            }
        }
    });
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_millis(200));
    }
    eprintln!("PROBE {prog} {args:?}: status {:?}\n  raw: {}", child.try_wait(), String::from_utf8_lossy(&raw.lock().unwrap()).escape_debug());
    let _ = child.kill();
}

#[test]
fn probe() {
    pty_run("cmd", &["/c", "echo hello-from-cmd"], None, 4);
    let env = Env::new();
    let mut child = std::process::Command::new(VELMUX).arg("debug").current_dir(&env.root).envs(env.vars()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break Some(s);
        }
        if start.elapsed() > Duration::from_secs(20) {
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    eprintln!("PROBE velmux debug (no pty): exited {status:?} after {:?}", start.elapsed());
    if status.is_some() {
        let out = child.wait_with_output().unwrap();
        eprintln!("  out: {}\n  err: {}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    } else {
        let _ = child.kill();
    }
    eprintln!("  server.log: {}", std::fs::read_to_string(env.state.join("server.log")).unwrap_or_default());
    eprintln!("  state: {:?}", std::fs::read_dir(&env.state).map(|d| d.flatten().map(|e| e.file_name()).collect::<Vec<_>>()));
    eprintln!("  server up: {}", uds_windows::UnixStream::connect(&env.socket).is_ok());
    env.stop(&env.root);
    eprintln!("PROBE stopped");
    pty_run(VELMUX, &["--version"], Some(&env), 4);
    pty_run(VELMUX, &[], Some(&env), 8);
    eprintln!("  server.log after pty attach: {}", std::fs::read_to_string(env.state.join("server.log")).unwrap_or_default());
}
