use std::io::{Read, Write};
use crate::ipc::UnixStream;
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
    let console = console::Console::setup();
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let reason = pump(stream, writer);
    // The server turns mouse and focus reporting on; always turn them off on the way out.
    let _ = execute!(stdout, DisableMouseCapture, DisableFocusChange, DisableBracketedPaste, LeaveAlternateScreen, cursor::Show);
    drop(console);
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
        let mut stdin = console::input();
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

    let mut stdout = console::output();
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
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|dir| velmux_pool::sys::find_program(&dir, &arg0.to_string_lossy()))
}

/// The command that starts a server from `exe`. Windows won't replace a
/// running program's file, so there the server runs from a copy in the state
/// dir, leaving `exe` free for `cargo build` or an update to replace.
pub(crate) fn server_command(exe: &std::path::Path) -> Result<Command> {
    #[cfg(windows)]
    let exe = &{
        let dir = paths::state_dir()?.join("bin");
        std::fs::create_dir_all(&dir)?;
        // Earlier servers' copies (one still running can't be removed: fine).
        for old in std::fs::read_dir(&dir)?.flatten() {
            let _ = std::fs::remove_file(old.path());
        }
        let copy = dir.join(format!("velmux-server-{}.exe", std::process::id()));
        std::fs::copy(exe, &copy).with_context(|| format!("copying {} to run the server from", exe.display()))?;
        copy
    };
    let mut cmd = Command::new(exe);
    cmd.arg("__server");
    Ok(cmd)
}

/// The velmux you run, which the server and the hooks it writes point at:
/// as launched (`VELMUX_EXE`), not the copy a Windows server runs from.
pub(crate) fn launched_exe() -> Option<PathBuf> {
    std::env::var_os("VELMUX_EXE").map(PathBuf::from).or_else(|| std::env::current_exe().ok())
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
    let mut cmd = server_command(&exe)?;
    // The path you ran (e.g. Homebrew's `bin/velmux` link), not the versioned
    // file behind it: an upgrade replaces what it points at, and the server
    // watches it to restart into the new version. (On Windows the server runs
    // from a copy, so it always needs telling.)
    if let Some(launched) = launched_as().or_else(|| cfg!(windows).then_some(exe)) {
        cmd.env("VELMUX_EXE", launched);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    // Its own session, with no terminal: nothing it runs can take yours.
    velmux_pool::sys::detach(&mut cmd);
    cmd.spawn()
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

/// The terminal's raw bytes in and out: plain stdin and stdout on Unix. A
/// Windows console has to be asked for VT input (keys and mouse as escape
/// sequences, which the server parses) and output, and std's console stdio
/// gets in the way of raw bytes (it ends input at ctrl+z and refuses to write
/// bytes that aren't UTF-8), so there it's read and written directly.
mod console {
    #[cfg(unix)]
    pub struct Console;

    #[cfg(unix)]
    impl Console {
        pub fn setup() -> Console {
            Console
        }
    }

    /// (Nothing to put back on Unix; dropped explicitly like Windows' one.)
    #[cfg(unix)]
    impl Drop for Console {
        fn drop(&mut self) {}
    }

    #[cfg(unix)]
    pub fn input() -> impl std::io::Read {
        std::io::stdin()
    }

    #[cfg(unix)]
    pub fn output() -> impl std::io::Write {
        std::io::stdout()
    }

    #[cfg(windows)]
    pub use windows::*;

    #[cfg(windows)]
    mod windows {
        use std::io::{Read, Write};
        use std::os::windows::io::FromRawHandle;

        use windows_sys::Win32::Globalization::CP_UTF8;
        use windows_sys::Win32::System::Console::*;

        /// The console modes to put back on the way out.
        pub struct Console {
            input: Option<u32>,
            output: Option<u32>,
            output_cp: u32,
        }

        fn mode(handle: STD_HANDLE) -> Option<u32> {
            let mut mode = 0;
            (unsafe { GetConsoleMode(GetStdHandle(handle), &mut mode) } != 0).then_some(mode)
        }

        impl Console {
            /// Call after crossterm's raw mode, which this adds to.
            pub fn setup() -> Console {
                let input = mode(STD_INPUT_HANDLE);
                let output = mode(STD_OUTPUT_HANDLE);
                let output_cp = unsafe { GetConsoleOutputCP() };
                unsafe {
                    if let Some(m) = input {
                        // Quick edit would take mouse drags for its own selection.
                        let m = (m | ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_EXTENDED_FLAGS) & !(ENABLE_QUICK_EDIT_MODE | ENABLE_MOUSE_INPUT | ENABLE_WINDOW_INPUT);
                        SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), m);
                    }
                    if let Some(m) = output {
                        SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), m | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
                    }
                    SetConsoleOutputCP(CP_UTF8);
                }
                Console { input, output, output_cp }
            }
        }

        impl Drop for Console {
            fn drop(&mut self) {
                unsafe {
                    if let Some(m) = self.input {
                        SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), m);
                    }
                    if let Some(m) = self.output {
                        SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), m);
                    }
                    SetConsoleOutputCP(self.output_cp);
                }
            }
        }

        /// Console input as UTF-8, from `ReadConsoleW`.
        struct Input {
            /// A high surrogate left over from the previous read.
            high: Option<u16>,
            /// Encoded bytes that didn't fit the caller's buffer.
            pending: Vec<u8>,
        }

        impl Read for Input {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                while self.pending.is_empty() {
                    let mut wide = [0u16; 1024];
                    let mut n = 0;
                    let ok = unsafe { ReadConsoleW(GetStdHandle(STD_INPUT_HANDLE), wide.as_mut_ptr().cast(), wide.len() as u32, &mut n, std::ptr::null()) };
                    if ok == 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    let mut units: Vec<u16> = self.high.take().into_iter().collect();
                    units.extend_from_slice(&wide[..n as usize]);
                    if units.last().is_some_and(|u| (0xd800..0xdc00).contains(u)) {
                        self.high = units.pop();
                    }
                    let text: String = char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect();
                    self.pending = text.into_bytes();
                }
                let n = buf.len().min(self.pending.len());
                buf[..n].copy_from_slice(&self.pending[..n]);
                self.pending.drain(..n);
                Ok(n)
            }
        }

        pub fn input() -> impl Read {
            Input { high: None, pending: Vec::new() }
        }

        /// The console's output handle as a file (`WriteFile`: bytes as they
        /// are, UTF-8 by the code page set up above). Never closed.
        pub fn output() -> impl Write {
            let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
            Output(std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(handle) }))
        }

        struct Output(std::mem::ManuallyDrop<std::fs::File>);

        impl Write for Output {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.write(buf)
            }

            fn flush(&mut self) -> std::io::Result<()> {
                self.0.flush()
            }
        }
    }
}
