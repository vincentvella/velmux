//! Key bindings: named actions bound either after the prefix (`prefix+v`) or
//! directly, with no prefix ("root" bindings like `super+d`).
//!
//! A spec is `[prefix+]` + modifiers (`ctrl`, `alt`, `shift`, `super`) + a key,
//! e.g. `prefix+|`, `super+shift+enter`, `super+1..9`. Each spec is encoded to the
//! bytes a terminal sends for it. Chords a legacy terminal can't express (`super`
//! anything, `ctrl+digit`) use CSI-u (`ESC [ code ; mods u`), the kitty keyboard
//! protocol's encoding. Apps never receive those unless the terminal is told to
//! send them, so root bindings on them can't steal keys from programs, and
//! [`ghostty_config`] prints the matching terminal config.

use std::collections::BTreeMap;

use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    ClaimSlot,
    ReleaseSlot,
    AgentBase,
    NewShell,
    NewEditor,
    SplitRight,
    SplitDown,
    ClosePane,
    Zoom,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    ResizeLeft,
    ResizeRight,
    ResizeUp,
    ResizeDown,
    CopyMode,
    CopyModePageUp,
    NextPane,
    PrevPane,
    /// Slot N of the current repo.
    Slot(u8),
    /// The Nth location with panes, in sidebar order.
    Location(u8),
    PrevLocation,
    NextLocation,
    NewTab,
    NextTab,
    PrevTab,
    Tab(u8),
    ToggleSidebar,
    /// Jump to the next agent that's blocked or finished unseen.
    Attention,
    Detach,
    Quit,
    Help,
    /// Send the prefix key itself to the pane.
    SendPrefix,
}

/// `(config name, indexed?)` for every action, in help order.
const NAMES: &[(&str, Action)] = &[
    ("claim_slot", Action::ClaimSlot),
    ("release_slot", Action::ReleaseSlot),
    ("agent_base", Action::AgentBase),
    ("new_shell", Action::NewShell),
    ("new_editor", Action::NewEditor),
    ("split_right", Action::SplitRight),
    ("split_down", Action::SplitDown),
    ("close_pane", Action::ClosePane),
    ("zoom", Action::Zoom),
    ("focus_left", Action::FocusLeft),
    ("focus_right", Action::FocusRight),
    ("focus_up", Action::FocusUp),
    ("focus_down", Action::FocusDown),
    ("resize_left", Action::ResizeLeft),
    ("resize_right", Action::ResizeRight),
    ("resize_up", Action::ResizeUp),
    ("resize_down", Action::ResizeDown),
    ("copy_mode", Action::CopyMode),
    ("copy_mode_page_up", Action::CopyModePageUp),
    ("next_pane", Action::NextPane),
    ("prev_pane", Action::PrevPane),
    ("slot", Action::Slot(0)),
    ("location", Action::Location(0)),
    ("prev_location", Action::PrevLocation),
    ("next_location", Action::NextLocation),
    ("new_tab", Action::NewTab),
    ("next_tab", Action::NextTab),
    ("prev_tab", Action::PrevTab),
    ("tab", Action::Tab(0)),
    ("toggle_sidebar", Action::ToggleSidebar),
    ("attention", Action::Attention),
    ("detach", Action::Detach),
    ("quit", Action::Quit),
    ("help", Action::Help),
    ("send_prefix", Action::SendPrefix),
];

fn indexed(a: Action) -> Option<fn(u8) -> Action> {
    match a {
        Action::Slot(_) => Some(Action::Slot),
        Action::Location(_) => Some(Action::Location),
        Action::Tab(_) => Some(Action::Tab),
        _ => None,
    }
}

/// Defaults: velmux's prefix keys, plus root bindings mirroring a Cmd-driven
/// (herdr / cmux style) layout. The root ones only fire if your terminal sends
/// them; `velmux keys ghostty` prints the Ghostty side.
fn defaults() -> Vec<(Action, &'static [&'static str])> {
    use Action::*;
    vec![
        (ClaimSlot, &["prefix+n", "super+n"]),
        (ReleaseSlot, &["prefix+r", "super+shift+w"]),
        (AgentBase, &["prefix+a", "super+shift+a"]),
        (NewShell, &["prefix+c"]),
        (NewEditor, &["prefix+e"]),
        (SplitRight, &["prefix+|", "prefix+%", "super+d"]),
        (SplitDown, &["prefix+-", "prefix+\"", "super+shift+d"]),
        (ClosePane, &["prefix+x", "super+w"]),
        (Zoom, &["prefix+z", "super+shift+enter"]),
        (FocusLeft, &["prefix+left", "super+alt+left"]),
        (FocusRight, &["prefix+right", "super+alt+right"]),
        (FocusUp, &["prefix+up", "super+alt+up"]),
        (FocusDown, &["prefix+down", "super+alt+down"]),
        (ResizeLeft, &["prefix+H"]),
        (ResizeRight, &["prefix+L"]),
        (ResizeUp, &["prefix+K"]),
        (ResizeDown, &["prefix+J"]),
        (CopyMode, &["prefix+["]),
        (CopyModePageUp, &["prefix+pageup"]),
        (NextPane, &["prefix+j"]),
        (PrevPane, &["prefix+k"]),
        (Slot(0), &["prefix+1..9"]),
        (Location(0), &["prefix+alt+1..9", "super+1..9"]),
        (PrevLocation, &["prefix+,", "super+ctrl+["]),
        (NextLocation, &["prefix+.", "super+ctrl+]"]),
        (NewTab, &["prefix+t", "super+t"]),
        (NextTab, &["prefix+tab", "super+shift+]"]),
        (PrevTab, &["prefix+shift+tab", "super+shift+["]),
        (Tab(0), &["ctrl+1..9"]),
        (ToggleSidebar, &["prefix+b", "super+b"]),
        (Attention, &["prefix+o", "super+shift+u"]),
        (Detach, &["prefix+d"]),
        (Quit, &["prefix+Q"]),
        (Help, &["prefix+?"]),
        (SendPrefix, &["prefix+prefix"]),
    ]
}

// ---------- key specs ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Char(char),
    Enter,
    Tab,
    Esc,
    Space,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Mods {
    ctrl: bool,
    alt: bool,
    shift: bool,
    sup: bool,
}

impl Mods {
    /// xterm/kitty modifier parameter: 1 + shift + 2·alt + 4·ctrl + 8·super.
    fn param(self) -> u8 {
        1 + self.shift as u8 + 2 * self.alt as u8 + 4 * self.ctrl as u8 + 8 * self.sup as u8
    }

    fn any(self) -> bool {
        self.ctrl || self.alt || self.shift || self.sup
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    mods: Mods,
    key: Key,
}

/// One parsed binding: after the prefix or not, and its chord (`None` = the prefix key).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Spec {
    prefixed: bool,
    chord: Option<Chord>,
}

fn parse_key(name: &str) -> Result<Key> {
    Ok(match name {
        "enter" | "return" => Key::Enter,
        "tab" => Key::Tab,
        "esc" | "escape" => Key::Esc,
        "space" => Key::Space,
        "backspace" => Key::Backspace,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "plus" => Key::Char('+'),
        "minus" => Key::Char('-'),
        "comma" => Key::Char(','),
        "period" => Key::Char('.'),
        s if s.chars().count() == 1 => Key::Char(s.chars().next().unwrap()),
        s => bail!("unknown key {s:?}"),
    })
}

/// Parse a spec; `1..9` ranges expand to nine specs.
fn parse(spec: &str) -> Result<Vec<(Spec, Option<u8>)>> {
    let spec = spec.trim();
    let (base, range) = match spec.strip_suffix("1..9") {
        Some(base) => (base.to_string(), true),
        None => (spec.to_string(), false),
    };
    let mut parts: Vec<&str> = match base.strip_suffix('+') {
        // `super+` before a range: just separators.
        Some(head) if range => head.split('+').filter(|p| !p.is_empty()).collect(),
        // A literal `+` key, e.g. `prefix++`.
        Some(head) => {
            let head = head.strip_suffix('+').unwrap_or(head);
            let mut v: Vec<&str> = head.split('+').filter(|p| !p.is_empty()).collect();
            v.push("+");
            v
        }
        None => base.split('+').collect(),
    };
    let mut prefixed = false;
    if parts.first() == Some(&"prefix") {
        prefixed = true;
        parts.remove(0);
    }
    if parts == ["prefix"] && prefixed {
        return Ok(vec![(Spec { prefixed, chord: None }, None)]);
    }
    let key_name = if range { None } else { parts.pop() };
    let mut mods = Mods::default();
    for m in &parts {
        match *m {
            "ctrl" => mods.ctrl = true,
            "alt" | "opt" | "meta" => mods.alt = true,
            "shift" => mods.shift = true,
            "super" | "cmd" => mods.sup = true,
            other => bail!("unknown modifier {other:?} in {spec:?}"),
        }
    }
    if range {
        return Ok((1..=9u8)
            .map(|n| (Spec { prefixed, chord: Some(Chord { mods, key: Key::Char((b'0' + n) as char) }) }, Some(n)))
            .collect());
    }
    let Some(key_name) = key_name.filter(|k| !k.is_empty()) else { bail!("no key in {spec:?}") };
    let key = parse_key(key_name)?;
    if !prefixed && !mods.any() {
        bail!("{spec:?}: a binding with no prefix needs a modifier, or it would steal plain typing");
    }
    Ok(vec![(Spec { prefixed, chord: Some(Chord { mods, key }) }, None)])
}

/// Bytes a terminal sends for `c`.
fn encode(c: &Chord) -> Vec<u8> {
    let m = c.mods;
    let csi_u = |code: u32| format!("\x1b[{code};{}u", m.param()).into_bytes();
    let csi_mod = |fin: char| {
        if m.any() { format!("\x1b[1;{}{fin}", m.param()) } else { format!("\x1b[{fin}") }.into_bytes()
    };
    let tilde = |n: u8| if m.any() { format!("\x1b[{n};{}~", m.param()) } else { format!("\x1b[{n}~") }.into_bytes();
    match c.key {
        Key::Up => csi_mod('A'),
        Key::Down => csi_mod('B'),
        Key::Right => csi_mod('C'),
        Key::Left => csi_mod('D'),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        _ if m.sup => csi_u(key_code(c.key)),
        Key::Char(ch) if m.ctrl && ch.is_ascii_digit() => csi_u(ch as u32),
        Key::Char(ch) => {
            let mut ch = ch;
            if m.shift && ch.is_ascii_lowercase() {
                ch = ch.to_ascii_uppercase();
            }
            let mut out = Vec::new();
            if m.alt {
                out.push(0x1b);
            }
            if m.ctrl {
                out.push(match ch.to_ascii_lowercase() {
                    c @ 'a'..='z' => c as u8 - b'a' + 1,
                    '@' | ' ' => 0,
                    '[' => 0x1b,
                    '\\' => 0x1c,
                    ']' => 0x1d,
                    '^' => 0x1e,
                    '_' => 0x1f,
                    _ => return csi_u(ch as u32),
                });
            } else {
                let mut b = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
            }
            out
        }
        Key::Tab if m.shift && !m.alt && !m.ctrl => b"\x1b[Z".to_vec(),
        Key::Enter | Key::Tab | Key::Esc | Key::Space | Key::Backspace if m.ctrl || m.shift => csi_u(key_code(c.key)),
        k => {
            let mut out = if m.alt { vec![0x1b] } else { Vec::new() };
            out.push(key_code(k) as u8);
            out
        }
    }
}

fn key_code(k: Key) -> u32 {
    match k {
        Key::Char(c) => c.to_ascii_lowercase() as u32,
        Key::Enter => 13,
        Key::Tab => 9,
        Key::Esc => 27,
        Key::Space => 32,
        Key::Backspace => 127,
        _ => 0,
    }
}

// ---------- keymap ----------

#[derive(Debug, Default)]
pub struct Keymap {
    /// Key sequences after the prefix.
    prefixed: Vec<(Vec<u8>, Action)>,
    /// Sequences matched anywhere in normal input.
    root: Vec<(Vec<u8>, Action)>,
    /// Root chords, kept for the Ghostty export.
    root_chords: Vec<(Chord, Action)>,
    /// Display specs per action (for help).
    pub specs: BTreeMap<Action, Vec<String>>,
}

impl Keymap {
    /// Defaults with `overrides` (`action name → specs`) replacing an action's keys.
    /// An empty list unbinds it. Returns warnings for anything invalid.
    pub fn build(prefix_byte: u8, overrides: &BTreeMap<String, Vec<String>>) -> (Keymap, Vec<String>) {
        let mut warnings = Vec::new();
        let mut table: Vec<(Action, Vec<String>)> =
            defaults().into_iter().map(|(a, s)| (a, s.iter().map(|x| x.to_string()).collect())).collect();
        for (name, specs) in overrides {
            match NAMES.iter().find(|(n, _)| n == name) {
                Some((_, action)) => {
                    let entry = table.iter_mut().find(|(a, _)| a == action).unwrap();
                    entry.1 = specs.clone();
                }
                None => warnings.push(format!("keys: unknown action {name:?}")),
            }
        }
        let mut map = Keymap::default();
        for (action, specs) in table {
            for spec in &specs {
                let parsed = match parse(spec) {
                    Ok(p) => p,
                    Err(e) => {
                        warnings.push(format!("keys.{}: {e:#}", name_of(action)));
                        continue;
                    }
                };
                for (s, n) in parsed {
                    let act = match (indexed(action), n) {
                        (Some(f), Some(n)) => f(n),
                        (None, Some(_)) => {
                            warnings.push(format!("keys.{}: 1..9 only works for slot, location and tab", name_of(action)));
                            break;
                        }
                        (Some(_), None) => {
                            warnings.push(format!("keys.{}: needs a 1..9 range", name_of(action)));
                            break;
                        }
                        (None, None) => action,
                    };
                    let bytes = match &s.chord {
                        Some(c) => encode(c),
                        None => vec![prefix_byte],
                    };
                    if s.prefixed {
                        map.prefixed.push((bytes, act));
                    } else {
                        map.root.push((bytes, act));
                        if let Some(c) = s.chord {
                            map.root_chords.push((c, act));
                        }
                    }
                }
                map.specs.entry(action).or_default().push(spec.clone());
            }
        }
        // Longest sequences first, so `ESC [ 1 ; 3 D` wins over `ESC`.
        map.prefixed.sort_by_key(|(b, _)| std::cmp::Reverse(b.len()));
        map.root.sort_by_key(|(b, _)| std::cmp::Reverse(b.len()));
        (map, warnings)
    }

    /// The action bound right after the prefix, and how many bytes it used.
    pub fn after_prefix(&self, bytes: &[u8]) -> Option<(Action, usize)> {
        self.prefixed.iter().find(|(seq, _)| bytes.starts_with(seq)).map(|(seq, a)| (*a, seq.len()))
    }

    /// The first root binding in `bytes`: `(offset, action, length)`.
    pub fn find_root(&self, bytes: &[u8]) -> Option<(usize, Action, usize)> {
        if self.root.is_empty() {
            return None;
        }
        (0..bytes.len()).find_map(|i| {
            self.root.iter().find(|(seq, _)| bytes[i..].starts_with(seq)).map(|(seq, a)| (i, *a, seq.len()))
        })
    }

    /// One-line help: each action with its first prefix key (or root chord).
    pub fn help(&self, prefix_short: &str) -> String {
        let pick = |a: Action| {
            self.specs.get(&a).and_then(|s| {
                s.iter()
                    .find(|x| x.starts_with("prefix+"))
                    .or(s.first())
                    .map(|x| x.replacen("prefix+", "", 1))
            })
        };
        let parts: Vec<String> = [
            (Action::ClaimSlot, "claim"),
            (Action::AgentBase, "claude@base"),
            (Action::SplitRight, "split"),
            (Action::NewTab, "tab"),
            (Action::Location(0), "go"),
            (Action::Attention, "attention"),
            (Action::ToggleSidebar, "sidebar"),
            (Action::CopyMode, "copy"),
            (Action::Zoom, "zoom"),
            (Action::ReleaseSlot, "release"),
            (Action::ClosePane, "close"),
            (Action::Detach, "detach"),
            (Action::Quit, "quit"),
        ]
        .into_iter()
        .filter_map(|(a, label)| pick(a).map(|k| format!("{k} {label}")))
        .collect();
        format!("{prefix_short} then: {}", parts.join(" · "))
    }
}

pub fn name_of(a: Action) -> &'static str {
    let probe = match a {
        Action::Slot(_) => Action::Slot(0),
        Action::Location(_) => Action::Location(0),
        Action::Tab(_) => Action::Tab(0),
        other => other,
    };
    NAMES.iter().find(|(_, x)| *x == probe).map_or("?", |(n, _)| n)
}

/// Ghostty `keybind` lines that make the terminal send every root binding.
pub fn ghostty_config(map: &Keymap) -> String {
    let mut out = String::from("# velmux root bindings — paste into your Ghostty config\n");
    for (chord, action) in &map.root_chords {
        let mut name = Vec::new();
        for (on, label) in [(chord.mods.sup, "super"), (chord.mods.ctrl, "ctrl"), (chord.mods.alt, "alt"), (chord.mods.shift, "shift")] {
            if on {
                name.push(label.to_string());
            }
        }
        name.push(match chord.key {
            Key::Char(c) if c.is_ascii_digit() => format!("digit_{c}"),
            Key::Char('[') => "bracket_left".into(),
            Key::Char(']') => "bracket_right".into(),
            Key::Char(',') => "comma".into(),
            Key::Char('.') => "period".into(),
            Key::Char('-') => "minus".into(),
            Key::Char(c) => c.to_ascii_lowercase().to_string(),
            Key::Enter => "enter".into(),
            Key::Tab => "tab".into(),
            Key::Esc => "escape".into(),
            Key::Space => "space".into(),
            Key::Backspace => "backspace".into(),
            Key::Up => "arrow_up".into(),
            Key::Down => "arrow_down".into(),
            Key::Left => "arrow_left".into(),
            Key::Right => "arrow_right".into(),
            Key::PageUp => "page_up".into(),
            Key::PageDown => "page_down".into(),
        });
        let bytes = encode(chord);
        let value = match bytes.strip_prefix(b"\x1b[") {
            Some(rest) => format!("csi:{}", String::from_utf8_lossy(rest)),
            None => {
                let esc: String = bytes.iter().map(|b| format!("\\x{b:02x}")).collect();
                format!("text:{esc}")
            }
        };
        let label = match action {
            Action::Location(n) | Action::Tab(n) | Action::Slot(n) => format!("{} {n}", name_of(*action)),
            a => name_of(*a).to_string(),
        };
        out.push_str(&format!("keybind = {}={value}  # {label}\n", name.join("+")));
        // Ghostty also matches the character form (`super+1`) of a digit key; bind
        // it too so an older `super+1=…` line can't win.
        if let Key::Char(c) = chord.key {
            if c.is_ascii_digit() {
                name.pop();
                name.push(c.to_string());
                out.push_str(&format!("keybind = {}={value}  # {label}\n", name.join("+")));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(spec: &str) -> Vec<u8> {
        let (s, _) = parse(spec).unwrap().remove(0);
        encode(&s.chord.unwrap())
    }

    #[test]
    fn encodes_like_a_terminal() {
        assert_eq!(chord("prefix+v"), b"v");
        assert_eq!(chord("prefix+H"), b"H");
        assert_eq!(chord("prefix+|"), b"|");
        assert_eq!(chord("prefix+left"), b"\x1b[D");
        assert_eq!(chord("prefix+shift+tab"), b"\x1b[Z");
        assert_eq!(chord("prefix+pageup"), b"\x1b[5~");
        assert_eq!(chord("alt+x"), b"\x1bx");
        assert_eq!(chord("ctrl+g"), b"\x07");
        assert_eq!(chord("alt+left"), b"\x1b[1;3D");
        // Unexpressible in legacy encoding: CSI-u.
        assert_eq!(chord("super+t"), b"\x1b[116;9u");
        assert_eq!(chord("super+shift+d"), b"\x1b[100;10u");
        assert_eq!(chord("super+shift+enter"), b"\x1b[13;10u");
        assert_eq!(chord("super+alt+left"), b"\x1b[1;11D");
        assert_eq!(chord("ctrl+3"), b"\x1b[51;5u");
        assert_eq!(chord("super+ctrl+["), b"\x1b[91;13u");
    }

    #[test]
    fn parses_ranges_prefix_and_rejects_bad_specs() {
        let r = parse("super+1..9").unwrap();
        assert_eq!(r.len(), 9);
        assert_eq!(r[2].1, Some(3));
        assert!(!r[0].0.prefixed);
        assert_eq!(parse("prefix+prefix").unwrap()[0].0.chord, None);
        assert_eq!(chord("prefix++"), b"+");
        assert!(parse("x").is_err(), "plain key without prefix or modifier");
        assert!(parse("hyper+x").is_err());
        assert!(parse("prefix+nope").is_err());
    }

    #[test]
    fn keymap_matches_prefix_and_root_and_honours_overrides() {
        let (map, warn) = Keymap::build(0x02, &BTreeMap::new());
        assert!(warn.is_empty(), "{warn:?}");
        assert_eq!(map.after_prefix(b"vrest"), None);
        assert_eq!(map.after_prefix(b"|"), Some((Action::SplitRight, 1)));
        assert_eq!(map.after_prefix(b"\x1b3"), Some((Action::Location(3), 2)));
        assert_eq!(map.after_prefix(b"\x02"), Some((Action::SendPrefix, 1)));
        assert_eq!(map.after_prefix(b"3"), Some((Action::Slot(3), 1)));
        // Root chords are found mid-stream; plain typing never matches.
        assert_eq!(map.find_root(b"ls\x1b[116;9u"), Some((2, Action::NewTab, 8)));
        assert_eq!(map.find_root(b"\x1b[51;5u"), Some((0, Action::Tab(3), 7)));
        assert_eq!(map.find_root(b"hello world\x1b[A"), None);

        let mut o = BTreeMap::new();
        o.insert("split_right".to_string(), vec!["prefix+v".to_string(), "alt+v".to_string()]);
        o.insert("new_tab".to_string(), vec![]);
        o.insert("bogus".to_string(), vec!["prefix+q".to_string()]);
        o.insert("zoom".to_string(), vec!["z".to_string()]);
        let (map, warn) = Keymap::build(0x01, &o);
        assert_eq!(map.after_prefix(b"v"), Some((Action::SplitRight, 1)));
        assert_eq!(map.after_prefix(b"|"), None);
        assert_eq!(map.find_root(b"\x1bv"), Some((0, Action::SplitRight, 2)));
        assert_eq!(map.find_root(b"\x1b[116;9u"), None, "new_tab unbound");
        assert_eq!(warn.len(), 2, "{warn:?}");
        assert_eq!(map.after_prefix(b"\x01"), Some((Action::SendPrefix, 1)));
    }

    #[test]
    fn exports_ghostty_keybinds() {
        let (map, _) = Keymap::build(0x02, &BTreeMap::new());
        let g = ghostty_config(&map);
        assert!(g.contains("keybind = super+t=csi:116;9u  # new_tab"), "{g}");
        assert!(g.contains("keybind = super+digit_2=csi:50;9u  # location 2"));
        assert!(g.contains("keybind = super+2=csi:50;9u  # location 2"));
        assert!(g.contains("keybind = ctrl+digit_1=csi:49;5u  # tab 1"));
        assert!(g.contains("keybind = super+alt+arrow_left=csi:1;11D  # focus_left"));
        assert!(g.contains("keybind = super+ctrl+bracket_left=csi:91;13u  # prev_location"));
        assert!(g.contains("keybind = super+shift+enter=csi:13;10u  # zoom"));
    }
}
