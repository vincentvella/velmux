//! Mouse input: the client terminal reports SGR events (`ESC [ < b ; x ; y M/m`);
//! the server either acts on them or re-encodes them for a pane's app in the
//! protocol and encoding that app asked for.

use vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// Turn on button + drag reporting with SGR coordinates in the client terminal.
pub const ENABLE: &[u8] = b"\x1b[?1002h\x1b[?1006h";
/// Also every movement with no button held (for focus follows mouse).
pub const ENABLE_HOVER: &[u8] = b"\x1b[?1003h\x1b[?1006h";
pub const DISABLE: &[u8] = b"\x1b[?1003l\x1b[?1002l\x1b[?1006l";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    /// Raw SGR button code: low bits = button, +4/+8/+16 modifiers, +32 motion, +64 wheel.
    pub code: u16,
    /// 0-based cell coordinates in the client terminal.
    pub x: u16,
    pub y: u16,
    pub release: bool,
}

impl MouseEvent {
    /// Moving with no button held (only reported with [`ENABLE_HOVER`]).
    pub fn is_hover(&self) -> bool {
        self.is_motion() && self.code & 0b11 == 3
    }

    pub fn is_motion(&self) -> bool {
        self.code & 32 != 0
    }

    /// `Some(-1)` wheel up, `Some(1)` wheel down.
    pub fn wheel(&self) -> Option<isize> {
        (self.code & 64 != 0).then_some(if self.code & 1 == 0 { -1 } else { 1 })
    }

    pub fn is_left(&self) -> bool {
        self.wheel().is_none() && self.code & 0b11 == 0
    }

    pub fn is_right(&self) -> bool {
        self.wheel().is_none() && self.code & 0b11 == 2
    }

    pub fn is_press(&self) -> bool {
        !self.release && !self.is_motion()
    }
}

pub enum Chunk<'a> {
    Bytes(&'a [u8]),
    Mouse(MouseEvent),
}

const SGR: &[u8] = b"\x1b[<";

/// Split client input into plain bytes and mouse events. A sequence cut off at
/// the end of the read is returned as `carry`, to be prepended to the next one.
pub fn split(input: &[u8]) -> (Vec<Chunk<'_>>, Vec<u8>) {
    let mut out = Vec::new();
    let mut rest = input;
    loop {
        let Some(at) = find(rest, SGR) else {
            // A trailing `ESC [` is an unfinished sequence; finish it with the next read.
            // (A lone ESC is a real Escape keypress and must not be held back.)
            let keep = if rest.ends_with(b"\x1b[") { 2 } else { 0 };
            let (bytes, carry) = rest.split_at(rest.len() - keep);
            if !bytes.is_empty() {
                out.push(Chunk::Bytes(bytes));
            }
            return (out, carry.to_vec());
        };
        if at > 0 {
            out.push(Chunk::Bytes(&rest[..at]));
        }
        let body = &rest[at + SGR.len()..];
        match parse(body) {
            Parsed::Event(ev, len) => {
                out.push(Chunk::Mouse(ev));
                rest = &body[len..];
            }
            Parsed::Incomplete => return (out, rest[at..].to_vec()),
            Parsed::Invalid => {
                out.push(Chunk::Bytes(&rest[at..at + SGR.len()]));
                rest = body;
            }
        }
    }
}

enum Parsed {
    Event(MouseEvent, usize),
    Incomplete,
    Invalid,
}

fn parse(body: &[u8]) -> Parsed {
    let mut nums = [0u32; 3];
    let mut idx = 0;
    let mut digits = 0;
    for (i, &b) in body.iter().enumerate() {
        match b {
            b'0'..=b'9' => {
                nums[idx] = nums[idx].saturating_mul(10).saturating_add((b - b'0') as u32);
                digits += 1;
            }
            b';' if idx < 2 && digits > 0 => {
                idx += 1;
                digits = 0;
            }
            b'M' | b'm' if idx == 2 && digits > 0 => {
                let ev = MouseEvent {
                    code: nums[0].min(u16::MAX as u32) as u16,
                    x: nums[1].saturating_sub(1).min(u16::MAX as u32) as u16,
                    y: nums[2].saturating_sub(1).min(u16::MAX as u32) as u16,
                    release: b == b'm',
                };
                return Parsed::Event(ev, i + 1);
            }
            _ => return Parsed::Invalid,
        }
        if i > 24 {
            return Parsed::Invalid;
        }
    }
    Parsed::Incomplete
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Re-encode `ev` at pane-local `(x, y)` for an app that enabled `mode`/`encoding`,
/// or `None` if the app didn't ask for this kind of event.
pub fn encode(ev: &MouseEvent, x: u16, y: u16, mode: MouseProtocolMode, encoding: MouseProtocolEncoding) -> Option<Vec<u8>> {
    let held = ev.code & 0b11 != 3;
    let wanted = match mode {
        MouseProtocolMode::None => false,
        MouseProtocolMode::Press => ev.is_press(),
        MouseProtocolMode::PressRelease => !ev.is_motion(),
        MouseProtocolMode::ButtonMotion => !ev.is_motion() || held,
        MouseProtocolMode::AnyMotion => true,
    };
    if !wanted {
        return None;
    }
    let (cx, cy) = (x as u32 + 1, y as u32 + 1);
    match encoding {
        MouseProtocolEncoding::Sgr => {
            let end = if ev.release { 'm' } else { 'M' };
            Some(format!("\x1b[<{};{};{}{end}", ev.code, cx, cy).into_bytes())
        }
        legacy => {
            // Legacy encodings can't say which button was released: use 3.
            let code = if ev.release { (ev.code & !0b11) | 3 } else { ev.code } as u32;
            let vals = [code + 32, cx + 32, cy + 32];
            let mut out = b"\x1b[M".to_vec();
            for v in vals {
                match legacy {
                    MouseProtocolEncoding::Utf8 => {
                        let c = char::from_u32(v.min(2047))?;
                        let mut b = [0u8; 4];
                        out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
                    }
                    _ => out.push(u8::try_from(v).ok()?),
                }
            }
            Some(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(input: &[u8]) -> (Vec<String>, Vec<u8>) {
        let (chunks, carry) = split(input);
        let shown = chunks
            .iter()
            .map(|c| match c {
                Chunk::Bytes(b) => format!("bytes:{}", String::from_utf8_lossy(b)),
                Chunk::Mouse(e) => format!("mouse:{}@{},{}{}", e.code, e.x, e.y, if e.release { "r" } else { "" }),
            })
            .collect();
        (shown, carry)
    }

    #[test]
    fn splits_mouse_from_keys() {
        let (got, carry) = events(b"ab\x1b[<0;10;5Mcd\x1b[<0;10;5m");
        assert_eq!(got, ["bytes:ab", "mouse:0@9,4", "bytes:cd", "mouse:0@9,4r"]);
        assert!(carry.is_empty());
        // Arrow keys and other CSI pass through untouched.
        let (got, _) = events(b"\x1b[A\x1b[5~");
        assert_eq!(got, ["bytes:\x1b[A\x1b[5~"]);
    }

    #[test]
    fn carries_sequences_split_across_reads() {
        let (got, carry) = events(b"x\x1b[<64;3");
        assert_eq!(got, ["bytes:x"]);
        assert_eq!(carry, b"\x1b[<64;3");
        let mut next = carry.clone();
        next.extend_from_slice(b";7M");
        assert_eq!(events(&next).0, ["mouse:64@2,6"]);
        let (got, carry) = events(b"y\x1b[");
        assert_eq!((got, carry), (vec!["bytes:y".to_string()], b"\x1b[".to_vec()));
    }

    #[test]
    fn classifies_buttons() {
        let ev = |code, release| MouseEvent { code, x: 0, y: 0, release };
        assert!(ev(0, false).is_left() && ev(0, false).is_press());
        assert_eq!(ev(64, false).wheel(), Some(-1));
        assert_eq!(ev(65, false).wheel(), Some(1));
        assert!(ev(32, false).is_motion() && !ev(32, false).is_press());
        assert!(!ev(2, false).is_left());
    }

    #[test]
    fn encodes_for_the_apps_protocol() {
        let press = MouseEvent { code: 0, x: 4, y: 2, release: false };
        let release = MouseEvent { release: true, ..press };
        let drag = MouseEvent { code: 32, ..press };
        use MouseProtocolEncoding as E;
        use MouseProtocolMode as M;
        assert_eq!(encode(&press, 4, 2, M::Press, E::Sgr).unwrap(), b"\x1b[<0;5;3M");
        assert_eq!(encode(&release, 4, 2, M::Press, E::Sgr), None);
        assert_eq!(encode(&release, 4, 2, M::PressRelease, E::Sgr).unwrap(), b"\x1b[<0;5;3m");
        assert_eq!(encode(&drag, 4, 2, M::PressRelease, E::Sgr), None);
        assert!(encode(&drag, 4, 2, M::ButtonMotion, E::Sgr).is_some());
        assert_eq!(encode(&press, 4, 2, M::None, E::Sgr), None);
        assert_eq!(encode(&press, 4, 2, M::Press, E::Default).unwrap(), vec![0x1b, b'[', b'M', 32, 37, 35]);
        assert_eq!(encode(&release, 4, 2, M::PressRelease, E::Default).unwrap()[3], 35);
        assert_eq!(encode(&press, 300, 2, M::Press, E::Default), None);
        assert!(encode(&press, 300, 2, M::Press, E::Utf8).is_some());
    }
}
