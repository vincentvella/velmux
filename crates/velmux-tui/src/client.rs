use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use crossterm::event::{DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};

use crate::paths;
use crate::proto::{ClientMsg, ServerMsg};

/// Attach this terminal to the server, starting one if none is running.
pub fn attach(cwd: PathBuf) -> Result<()> {
    let stream = connect_or_spawn()?;
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let mut writer = stream.try_clone()?;
    ClientMsg::Hello { cols, rows, cwd }.write(&mut writer)?;

    terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let reason = pump(stream, writer);
    // The server turns mouse and focus reporting on; always turn them off on the way out.
    let _ = execute!(stdout, DisableMouseCapture, DisableFocusChange, DisableBracketedPaste, LeaveAlternateScreen, cursor::Show);
    let _ = terminal::disable_raw_mode();
    match reason {
        Ok(msg) => eprintln!("velmux: {msg}"),
        Err(e) => eprintln!("velmux: connection lost: {e:#}"),
    }
    // The stdin thread is blocked in read(); exit rather than join it.
    std::process::exit(0);
}

fn pump(mut stream: UnixStream, writer: UnixStream) -> Result<String> {
    // The writer is swapped out when the server restarts in place.
    let writer = std::sync::Arc::new(std::sync::Mutex::new(writer));
    let input = writer.clone();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 {
                break;
            }
            // A failed write mid-restart just drops that keystroke.
            let _ = ClientMsg::Input(buf[..n].to_vec()).write(&mut *input.lock().unwrap());
        }
    });
    let resize = writer.clone();
    std::thread::spawn(move || {
        let mut last = terminal::size().unwrap_or((0, 0));
        loop {
            std::thread::sleep(Duration::from_millis(150));
            let now = terminal::size().unwrap_or(last);
            if now != last {
                last = now;
                let _ = (ClientMsg::Resize { cols: now.0, rows: now.1 }).write(&mut *resize.lock().unwrap());
            }
        }
    });

    let mut stdout = std::io::stdout();
    loop {
        match ServerMsg::read(&mut stream)? {
            ServerMsg::Output(bytes) => {
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            }
            ServerMsg::Bye(reason) if reason == "restarting" => {
                stream = reconnect()?;
                let (cols, rows) = terminal::size().unwrap_or((80, 24));
                let mut w = stream.try_clone()?;
                // An empty cwd tells the server to keep your current place.
                ClientMsg::Hello { cols, rows, cwd: PathBuf::new() }.write(&mut w)?;
                *writer.lock().unwrap() = w;
            }
            ServerMsg::Bye(reason) | ServerMsg::Reply(reason) => return Ok(reason),
        }
    }
}

/// Wait for the restarted server's socket (it re-binds after exec).
fn reconnect() -> Result<UnixStream> {
    let sock = paths::socket()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(s) = UnixStream::connect(&sock) {
            return Ok(s);
        }
    }
    bail!("server didn't come back after restarting; see {}", paths::log()?.display())
}

/// Send a one-shot control request to the server (starting it if needed).
pub fn request(msg: ClientMsg) -> Result<String> {
    let mut stream = connect_or_spawn()?;
    msg.write(&mut stream)?;
    match ServerMsg::read(&mut stream)? {
        ServerMsg::Reply(text) | ServerMsg::Bye(text) => Ok(text),
        ServerMsg::Output(_) => bail!("unexpected output from server"),
    }
}

/// `argv[0]` as an absolute path, symlinks kept: as typed if it has a `/`,
/// else the first match on `PATH`.
fn launched_as() -> Option<std::path::PathBuf> {
    let arg0 = std::path::PathBuf::from(std::env::args_os().next()?);
    if arg0.components().count() > 1 {
        return Some(if arg0.is_absolute() { arg0 } else { std::env::current_dir().ok()?.join(arg0) });
    }
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(&arg0)).find(|p| p.is_file())
}

fn connect_or_spawn() -> Result<UnixStream> {
    let sock = paths::socket()?;
    if let Ok(s) = UnixStream::connect(&sock) {
        return Ok(s);
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::log()?)?;
    let exe = std::env::current_exe()?;
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(exe);
    // The path you ran (e.g. Homebrew's `bin/velmux` link), not the versioned
    // file behind it: an upgrade replaces what it points at, and the server
    // watches it to restart into the new version.
    if let Some(launched) = launched_as() {
        cmd.env("VELMUX_EXE", launched);
    }
    cmd.arg("__server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()
        .context("starting velmux server")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(s) = UnixStream::connect(&sock) {
            return Ok(s);
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    bail!("server did not start; see {}", paths::log()?.display())
}
