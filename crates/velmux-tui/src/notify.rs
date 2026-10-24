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

/// Play a sound without blocking: velmux's own (`chime` when an agent
/// finishes, `knock` when one's blocked on you), a macOS system sound name
/// (`Glass`, `Funk`, …), or a path to a sound file.
pub fn play(sound: &str) {
    if sound.is_empty() || sound == "none" {
        return;
    }
    let path = if let Some(own) = builtin(sound) {
        own.display().to_string()
    } else if sound.contains('/') || cfg!(windows) && sound.contains('\\') {
        sound.to_string()
    } else if cfg!(target_os = "macos") {
        format!("/System/Library/Sounds/{sound}.aiff")
    } else if cfg!(windows) {
        format!(r"C:\Windows\Media\{sound}.wav")
    } else {
        format!("/usr/share/sounds/freedesktop/stereo/{sound}.oga")
    };
    let mut player = if cfg!(target_os = "macos") {
        std::process::Command::new("afplay")
    } else if cfg!(windows) {
        // Windows has no player command; .NET's plays WAVs (which velmux's own are).
        let mut ps = std::process::Command::new("powershell");
        ps.args(["-NoProfile", "-NonInteractive", "-Command"]);
        ps.arg(format!("(New-Object Media.SoundPlayer {}).PlaySync()", velmux_pool::sys::quote(&path)));
        velmux_pool::sys::detach(&mut ps);
        ps
    } else {
        std::process::Command::new("paplay")
    };
    if !cfg!(windows) {
        player.arg(path);
    }
    let _ = player
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|mut child| std::thread::spawn(move || child.wait()));
}

/// The sounds velmux ships, by name (see NOTICE for where each comes from).
/// Bundled ones are WAVs in the binary; `chime` and `knock` are synthesised.
const BUNDLED: &[(&str, &[u8])] = &[
    ("herdr-done", include_bytes!("../sounds/herdr-done.wav")),
    ("herdr-request", include_bytes!("../sounds/herdr-request.wav")),
    ("confirm", include_bytes!("../sounds/confirm.wav")),
    ("glass", include_bytes!("../sounds/glass.wav")),
    ("pluck", include_bytes!("../sounds/pluck.wav")),
    ("question", include_bytes!("../sounds/question.wav")),
    ("bong", include_bytes!("../sounds/bong.wav")),
];

/// The picker's choices for "finished" and for "blocked" (`none` = silent).
pub const DONE_SOUNDS: &[&str] = &["herdr-done", "confirm", "glass", "pluck", "chime", "none"];
pub const BLOCKED_SOUNDS: &[&str] = &["herdr-request", "question", "bong", "knock", "none"];

/// A velmux sound as a file to play: written to the state dir on first use
/// (players need a path), rewritten if it changed with an upgrade.
fn builtin(name: &str) -> Option<std::path::PathBuf> {
    let bytes: Vec<u8> = match name {
        // (start s, frequency Hz, length s, loudness)
        "chime" => wav(&synth(&[(0.0, 783.99, 0.55, 0.9), (0.11, 1174.66, 0.75, 0.8)])),
        "knock" => wav(&synth(&[(0.0, 440.0, 0.16, 1.0), (0.17, 440.0, 0.22, 1.0)])),
        _ => BUNDLED.iter().find(|(n, _)| *n == name)?.1.to_vec(),
    };
    let path = crate::paths::state_dir().ok()?.join("sounds").join(format!("{name}.wav"));
    if std::fs::metadata(&path).map(|m| m.len()).ok() != Some(bytes.len() as u64) {
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, &bytes).ok()?;
    }
    Some(path)
}

/// `config` with `key = "value"` set in its `[notify]` table (made if missing),
/// everything else as it was.
pub fn set_notify_key(config: &str, key: &str, value: &str) -> String {
    let line = format!("{key} = \"{value}\"");
    let mut out: Vec<String> = Vec::new();
    let (mut in_notify, mut done) = (false, false);
    for l in config.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            if in_notify && !done {
                let blanks = out.iter().rev().take_while(|l| l.trim().is_empty()).count();
                out.insert(out.len() - blanks, line.clone());
                done = true;
            }
            in_notify = t == "[notify]";
        }
        if in_notify && !done && t.strip_prefix(key).is_some_and(|r| r.trim_start().starts_with('=')) {
            out.push(line.clone());
            done = true;
            continue;
        }
        out.push(l.to_string());
    }
    if !done {
        if in_notify {
            out.push(line);
        } else {
            if out.last().is_some_and(|l| !l.trim().is_empty()) {
                out.push(String::new());
            }
            out.push("[notify]".into());
            out.push(line);
        }
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

const RATE: u32 = 44_100;

/// Bell-like notes (a fundamental and a softer octave, quick attack,
/// exponential fade), mixed and kept well below full scale.
fn synth(notes: &[(f32, f32, f32, f32)]) -> Vec<i16> {
    let total = notes.iter().map(|(at, _, len, _)| at + len).fold(0.0, f32::max);
    let n = (total * RATE as f32) as usize;
    let mut mix = vec![0f32; n];
    for &(at, freq, len, loud) in notes {
        let start = (at * RATE as f32) as usize;
        let count = (len * RATE as f32) as usize;
        for i in 0..count.min(n.saturating_sub(start)) {
            let t = i as f32 / RATE as f32;
            let attack = (t / 0.006).min(1.0);
            let fade = (-t * 7.0 / len).exp();
            let tau = std::f32::consts::TAU;
            let tone = (tau * freq * t).sin() + 0.25 * (tau * freq * 2.0 * t).sin();
            mix[start + i] += tone * attack * fade * loud;
        }
    }
    let peak = mix.iter().fold(0f32, |m, v| m.max(v.abs())).max(1e-6);
    mix.iter().map(|v| (v / peak * 0.35 * i16::MAX as f32) as i16).collect()
}

/// 16-bit mono PCM WAV.
fn wav(samples: &[i16]) -> Vec<u8> {
    let data = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
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
    } else if cfg!(windows) {
        // A toast through WinRT, which Windows PowerShell (not pwsh) can reach,
        // shown as PowerShell's: an unregistered app's toasts don't appear.
        let q = velmux_pool::sys::quote;
        let script = format!(
            "$m = [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]; \
             $t = $m::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
             $x = $t.GetElementsByTagName('text'); \
             [void]$x.Item(0).AppendChild($t.CreateTextNode({})); [void]$x.Item(1).AppendChild($t.CreateTextNode({})); \
             $m::CreateToastNotifier('{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\\WindowsPowerShell\\v1.0\\powershell.exe').Show([Windows.UI.Notifications.ToastNotification]::new($t))",
            q(title),
            q(body)
        );
        let mut ps = std::process::Command::new("powershell");
        ps.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
        velmux_pool::sys::detach(&mut ps);
        ps.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn()
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
    fn builtin_sounds_are_valid_wavs() {
        let chime = wav(&synth(&[(0.0, 784.0, 0.5, 1.0), (0.1, 1175.0, 0.7, 0.8)]));
        assert_eq!(&chime[..4], b"RIFF");
        assert_eq!(&chime[8..16], b"WAVEfmt ");
        let seconds = (chime.len() - 44) as f32 / 2.0 / RATE as f32;
        assert!((seconds - 0.8).abs() < 0.01, "{seconds}");
        let samples = synth(&[(0.0, 440.0, 0.2, 1.0)]);
        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak > 10_000 && peak < 12_000, "soft, not clipping: {peak}");
    }

    #[test]
    fn bundled_sounds_are_wavs() {
        for (name, bytes) in BUNDLED {
            assert_eq!(&bytes[..4], b"RIFF", "{name}");
        }
        for name in DONE_SOUNDS.iter().chain(BLOCKED_SOUNDS).filter(|n| **n != "none") {
            assert!(BUNDLED.iter().any(|(n, _)| n == name) || matches!(*name, "chime" | "knock"), "{name}");
        }
    }

    #[test]
    fn sets_notify_keys_keeping_the_rest() {
        assert_eq!(set_notify_key("prefix = \"C-a\"\n", "done_sound", "glass"), "prefix = \"C-a\"\n\n[notify]\ndone_sound = \"glass\"\n");
        let cfg = "a = 1\n[notify]\nsound = true\ndone_sound = \"Glass\"\n\n[keys]\nx = []\n";
        assert_eq!(set_notify_key(cfg, "done_sound", "pluck"), "a = 1\n[notify]\nsound = true\ndone_sound = \"pluck\"\n\n[keys]\nx = []\n");
        assert_eq!(set_notify_key(cfg, "blocked_sound", "bong"), "a = 1\n[notify]\nsound = true\ndone_sound = \"Glass\"\nblocked_sound = \"bong\"\n\n[keys]\nx = []\n");
    }

    #[test]
    fn osc777_strips_separators() {
        assert_eq!(osc777("velmux", "a;b\x07c"), b"\x1b]777;notify;velmux;abc\x1b\\".to_vec());
    }
}
