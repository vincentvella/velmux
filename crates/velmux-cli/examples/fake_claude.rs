//! A stand-in for Claude Code in velmux's end-to-end tests. Installed as
//! `…/claude/versions/9.9.9` so velmux detects it as Claude, it records its
//! session the way Claude does (`<CLAUDE_CONFIG_DIR>/sessions/<pid>.json` and a
//! transcript under `projects/`), then reads commands, one per line:
//!
//! - `say <prompt>` (or `say <prompt> | <reply>`): a turn in the transcript
//! - `claim <branch>` / `release`: run `velmux claim` / `velmux release`
//! - `sh <command>`: run a shell command in the current folder
//! - `show <file>`: clear the screen and draw the file (a mock screen, for
//!   screenshots); `title <text>`: set the window title
//! - `exit`

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let (id, resumed) = match (flag("--resume"), flag("--session-id")) {
        (Some(id), _) => (id, true),
        (None, Some(id)) => (id, false),
        (None, None) => (format!("fake-{}", std::process::id()), false),
    };
    let claude_dir = PathBuf::from(std::env::var("CLAUDE_CONFIG_DIR").expect("CLAUDE_CONFIG_DIR"));
    let cwd = std::env::current_dir().unwrap();
    let pid = std::process::id();
    std::fs::create_dir_all(claude_dir.join("sessions")).unwrap();
    std::fs::write(claude_dir.join("sessions").join(format!("{pid}.json")), format!(r#"{{"pid":{pid},"sessionId":"{id}"}}"#)).unwrap();

    let transcript = existing_transcript(&claude_dir, &id).unwrap_or_else(|| {
        let dir = velmux_pool::claude::project_dir(&claude_dir, &cwd);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{id}.jsonl"))
    });
    if !resumed {
        // The transcript exists from the start (velmux only resumes ones that do).
        std::fs::OpenOptions::new().create(true).append(true).open(&transcript).unwrap();
    }
    let velmux = std::env::var("VELMUX_BIN").unwrap_or_else(|_| "velmux".into());
    let mut here = cwd.clone();
    println!("FAKE-CLAUDE id={id} mode={}", if resumed { "resume" } else { "new" });
    let stdin = std::io::stdin();
    let mut prompt = true;
    loop {
        if std::mem::replace(&mut prompt, true) {
            print!("> ");
        }
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim();
        let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
        match cmd {
            "say" => {
                // `say <prompt>` or `say <prompt> | <reply>`.
                let (prompt, reply) = rest.split_once(" | ").map_or((rest.to_string(), format!("reply to {rest}")), |(p, r)| (p.to_string(), r.to_string()));
                record(&transcript, "user", &prompt, &here);
                record(&transcript, "assistant", &reply, &here);
                println!("said {prompt}");
            }
            "claim" | "release" => {
                let mut c = Command::new(&velmux);
                c.arg(cmd).current_dir(&here);
                if !rest.is_empty() {
                    c.arg(rest);
                }
                let out = c.output().unwrap();
                let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if cmd == "claim" && out.status.success() {
                    here = PathBuf::from(text.lines().last().unwrap_or_default());
                    let _ = std::env::set_current_dir(&here);
                    println!("claimed {}", here.display());
                } else {
                    println!("{cmd}: {text} {}", String::from_utf8_lossy(&out.stderr).trim());
                    if cmd == "release" {
                        here = cwd.clone();
                    }
                }
            }
            "sh" => {
                let ok = Command::new("sh").arg("-c").arg(rest).current_dir(&here).status().is_ok_and(|s| s.success());
                println!("ran {ok}");
            }
            "show" => {
                print!("\x1b[H\x1b[2J\x1b[3J{}", std::fs::read_to_string(rest).unwrap_or_default().replace('\n', "\r\n"));
                prompt = false;
            }
            "title" => {
                print!("\x1b]2;{rest}\x07");
                // Claude also records its title for the conversation.
                let line = serde_json::json!({"type": "ai-title", "aiTitle": rest.trim_start_matches(|c: char| !c.is_alphanumeric()), "sessionId": id});
                let mut f = std::fs::OpenOptions::new().append(true).open(&transcript).unwrap();
                writeln!(f, "{line}").unwrap();
                prompt = false;
            }
            "exit" => {
                println!("FAKE-CLAUDE bye");
                break;
            }
            "" => {}
            other => println!("unknown {other}"),
        }
    }
}

fn existing_transcript(claude_dir: &Path, id: &str) -> Option<PathBuf> {
    std::fs::read_dir(claude_dir.join("projects")).ok()?.flatten().map(|d| d.path().join(format!("{id}.jsonl"))).find(|p| p.exists())
}

fn record(transcript: &Path, role: &str, text: &str, cwd: &Path) {
    let branch = Command::new("git").args(["branch", "--show-current"]).current_dir(cwd).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let content = if role == "user" { serde_json::json!(text) } else { serde_json::json!([{"type": "text", "text": text}]) };
    let line = serde_json::json!({"type": role, "message": {"role": role, "content": content}, "cwd": cwd, "gitBranch": branch});
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(transcript).unwrap();
    writeln!(f, "{line}").unwrap();
}
