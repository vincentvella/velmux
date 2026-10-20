//! Telling you when an agent needs you: a desktop notification (through the
//! client terminal's OSC 777, or the OS when no client is attached) and a sound.

use crate::detect::AgentState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Finished while you were looking elsewhere.
    Done,
    /// Waiting on you (a permission prompt, a question).
    Blocked,
}

/// What a state change is worth telling you about.
pub fn transition(
    before: (Option<AgentState>, bool),
    after: (Option<AgentState>, bool),
) -> Option<Kind> {
    let (was, was_done) = before;
    let (now, now_done) = after;
    if now == Some(AgentState::Blocked) && was != Some(AgentState::Blocked) {
        Some(Kind::Blocked)
    } else if now_done && !was_done {
        Some(Kind::Done)
    } else {
        None
    }
}

/// OSC 777 desktop notification, understood by Ghostty, kitty, WezTerm, foot…
/// Separators and control characters are stripped from both fields.
pub fn osc777(title: &str, body: &str) -> Vec<u8> {
    let clean = |s: &str| s.chars().filter(|c| !c.is_control() && *c != ';').collect::<String>();
    format!("\x1b]777;notify;{};{}\x1b\\", clean(title), clean(body)).into_bytes()
}

/// Play a sound without blocking: a macOS system sound name (`Glass`, `Funk`, …),
/// or a path to a sound file.
pub fn play(sound: &str) {
    if sound.is_empty() {
        return;
    }
    let path = if sound.contains('/') {
        sound.to_string()
    } else if cfg!(target_os = "macos") {
        format!("/System/Library/Sounds/{sound}.aiff")
    } else {
        format!("/usr/share/sounds/freedesktop/stereo/{sound}.oga")
    };
    let player: &[&str] = if cfg!(target_os = "macos") { &["afplay"] } else { &["paplay"] };
    let _ = std::process::Command::new(player[0])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|mut child| std::thread::spawn(move || child.wait()));
}

/// The OS's own notification, for when no terminal is attached to show one.
pub fn system(title: &str, body: &str) {
    let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("osascript")
            .arg("-e")
            .arg(format!("display notification \"{}\" with title \"{}\"", quote(body), quote(title)))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    } else {
        std::process::Command::new("notify-send")
            .args([title, body])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    };
    if let Ok(mut child) = result {
        std::thread::spawn(move || child.wait());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentState::*;

    #[test]
    fn notices_blocking_and_unseen_finishes_only() {
        assert_eq!(transition((Some(Working), false), (Some(Blocked), false)), Some(Kind::Blocked));
        assert_eq!(transition((Some(Idle), false), (Some(Blocked), false)), Some(Kind::Blocked));
        assert_eq!(transition((Some(Blocked), false), (Some(Blocked), false)), None, "still blocked: once");
        assert_eq!(transition((Some(Working), false), (Some(Idle), true)), Some(Kind::Done));
        assert_eq!(transition((Some(Working), false), (Some(Idle), false)), None, "finished while watched");
        assert_eq!(transition((Some(Idle), true), (Some(Idle), true)), None);
        assert_eq!(transition((None, false), (Some(Idle), false)), None);
    }

    #[test]
    fn osc777_strips_separators() {
        assert_eq!(osc777("velmux", "a;b\x07c"), b"\x1b]777;notify;velmux;abc\x1b\\".to_vec());
    }
}
