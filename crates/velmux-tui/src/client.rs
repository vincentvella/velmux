use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste};
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
    // The server turns mouse reporting on; always turn it off on the way out.
    let _ = execute!(stdout, DisableMouseCapture, DisableBracketedPaste, LeaveAlternateScreen, cursor::Show);
    let _ = terminal::disable_raw_mode();
    match reason {
        Ok(msg) => eprintln!("velmux: {msg}"),
        Err(e) => eprintln!("velmux: connection lost: {e:#}"),
    }
    // The stdin thread is blocked in read(); exit rather than join it.
    std::process::exit(0);
}

fn pump(mut stream: UnixStream, writer: UnixStream) -> Result<String> {
    let mut input = writer.try_clone()?;
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || ClientMsg::Input(buf[..n].to_vec()).write(&mut input).is_err() {
                break;
            }
        }
    });
    let mut resize = writer;
    std::thread::spawn(move || {
        let mut last = terminal::size().unwrap_or((0, 0));
        loop {
            std::thread::sleep(Duration::from_millis(150));
            let now = terminal::size().unwrap_or(last);
            if now != last {
                last = now;
                if (ClientMsg::Resize { cols: now.0, rows: now.1 }).write(&mut resize).is_err() {
                    break;
                }
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
            ServerMsg::Bye(reason) => return Ok(reason),
        }
    }
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
    Command::new(exe)
        .arg("__server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()
        .context("starting velmux server")?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Ok(s) = UnixStream::connect(&sock) {
            return Ok(s);
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    bail!("server did not start; see {}", paths::log()?.display())
}
