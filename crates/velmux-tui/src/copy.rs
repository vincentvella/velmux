//! Copy mode: scroll a pane's history, select with vi keys, yank to the clipboard.
//!
//! Positions are absolute lines: `0..scrollback_len` is history (oldest first),
//! followed by the visible screen rows. The pane's output is frozen while copy mode
//! is open (see `Pane::freeze`), so these stay stable.

/// An absolute (line, column) position.
pub type Pos = (usize, u16);

pub struct Copy {
    pub pane: u64,
    pub cursor: Pos,
    pub anchor: Option<Pos>,
    /// `V`: the selection covers whole lines.
    pub linewise: bool,
    /// Scrollback offset applied to the view (0 = live bottom).
    pub offset: usize,
    /// `Some` while typing a `/` or `?` search.
    pub search: Option<String>,
    /// Direction of the search being typed / last run: `true` = up (older).
    pub search_up: bool,
    last_search: Option<String>,
    pub message: Option<String>,
}

pub enum Action {
    Stay,
    Exit,
    Yank(String),
}

fn dims<CB: vt100::Callbacks>(p: &vt100::Parser<CB>) -> (usize, u16, u16) {
    let s = p.screen();
    let (rows, cols) = s.size();
    (s.scrollback_len(), rows.get(), cols.get())
}

/// Text of one absolute line, cell by cell, and whether it wraps into the next.
pub fn line_cells<CB: vt100::Callbacks>(p: &mut vt100::Parser<CB>, abs: usize) -> (Vec<String>, bool) {
    let (sb, _, cols) = dims(p);
    let saved = p.screen().scrollback();
    let (offset, row) = if abs < sb { (sb - abs, 0u16) } else { (0, (abs - sb) as u16) };
    p.screen_mut().set_scrollback(offset);
    let screen = p.screen();
    let cells = (0..cols)
        .map(|c| match screen.cell(row, c) {
            Some(cell) if cell.is_wide_continuation() => String::new(),
            Some(cell) if cell.has_contents() => cell.contents().to_string(),
            _ => " ".to_string(),
        })
        .collect();
    let wrapped = screen.row_wrapped(row);
    p.screen_mut().set_scrollback(saved);
    (cells, wrapped)
}

fn line_text<CB: vt100::Callbacks>(p: &mut vt100::Parser<CB>, abs: usize) -> String {
    line_cells(p, abs).0.concat().trim_end().to_string()
}

impl Copy {
    pub fn enter<CB: vt100::Callbacks>(pane: u64, p: &vt100::Parser<CB>) -> Copy {
        let (sb, _, _) = dims(p);
        let (row, col) = p.screen().cursor_position();
        Copy {
            pane,
            cursor: (sb + row as usize, col),
            anchor: None,
            linewise: false,
            offset: 0,
            search: None,
            search_up: false,
            last_search: None,
            // Full-screen apps (vim, `tui = "fullscreen"` Claude) draw on the alternate
            // screen, which has no history; only what's visible can be copied.
            message: p
                .screen()
                .alternate_screen()
                .then(|| "full-screen app: no history here (it scrolls itself) — copy what's visible".into()),
        }
    }

    /// First absolute line shown in the view.
    pub fn top<CB: vt100::Callbacks>(&self, p: &vt100::Parser<CB>) -> usize {
        dims(p).0 - self.offset
    }

    /// Normalised selection (start <= end), inclusive; whole lines when linewise.
    pub fn selection(&self) -> Option<(Pos, Pos)> {
        let a = self.anchor?;
        let (start, end) = if a <= self.cursor { (a, self.cursor) } else { (self.cursor, a) };
        Some(if self.linewise { ((start.0, 0), (end.0, u16::MAX)) } else { (start, end) })
    }

    /// Clamp the cursor and scroll so it's visible; apply the offset to the parser.
    pub fn sync<CB: vt100::Callbacks>(&mut self, p: &mut vt100::Parser<CB>) {
        let (sb, rows, cols) = dims(p);
        let total = sb + rows as usize;
        self.cursor.0 = self.cursor.0.min(total - 1);
        self.cursor.1 = self.cursor.1.min(cols - 1);
        self.offset = self.offset.min(sb);
        let top = sb - self.offset;
        if self.cursor.0 < top {
            self.offset = sb - self.cursor.0;
        } else if self.cursor.0 >= top + rows as usize {
            self.offset = sb - (self.cursor.0 + 1 - rows as usize);
        }
        p.screen_mut().set_scrollback(self.offset);
    }

    /// Handle one key from `bytes`; returns how many bytes it used and what to do.
    pub fn key<CB: vt100::Callbacks>(&mut self, bytes: &[u8], p: &mut vt100::Parser<CB>) -> (usize, Action) {
        self.message = None;
        if self.search.is_some() {
            return self.search_key(bytes, p);
        }
        let (sb, rows, _) = dims(p);
        let page = rows as isize;
        let (used, act) = match bytes {
            [0x1b, b'[', b'A', ..] => (3, Some(b'k')),
            [0x1b, b'[', b'B', ..] => (3, Some(b'j')),
            [0x1b, b'[', b'C', ..] => (3, Some(b'l')),
            [0x1b, b'[', b'D', ..] => (3, Some(b'h')),
            [0x1b, b'[', b'5', b'~', ..] => (4, Some(0x02)),
            [0x1b, b'[', b'6', b'~', ..] => (4, Some(0x06)),
            [0x1b, b'[', b'H', ..] => (3, Some(b'g')),
            [0x1b, b'[', b'F', ..] => (3, Some(b'G')),
            [0x1b, b'[', rest @ ..] => {
                // Unknown CSI: skip to its final byte.
                let len = rest.iter().position(|b| (0x40..=0x7e).contains(b)).map_or(bytes.len(), |i| i + 3);
                (len, None)
            }
            [0x1b] => return (1, Action::Exit),
            [b, ..] => (1, Some(*b)),
            [] => return (0, Action::Stay),
        };
        let Some(k) = act else { return (used, Action::Stay) };
        let line = self.cursor.0 as isize;
        match k {
            b'q' | 0x03 => return (used, Action::Exit),
            b'k' => self.cursor.0 = (line - 1).max(0) as usize,
            b'j' => self.cursor.0 = (line + 1) as usize,
            b'h' => self.cursor.1 = self.cursor.1.saturating_sub(1),
            b'l' => self.cursor.1 = self.cursor.1.saturating_add(1),
            0x15 => self.cursor.0 = (line - page / 2).max(0) as usize,
            0x04 => self.cursor.0 = (line + page / 2) as usize,
            0x02 => self.cursor.0 = (line - page).max(0) as usize,
            0x06 => self.cursor.0 = (line + page) as usize,
            b'g' => self.cursor = (0, 0),
            b'G' => self.cursor = (sb + rows as usize - 1, 0),
            b'0' => self.cursor.1 = 0,
            b'$' => self.cursor.1 = line_text(p, self.cursor.0).chars().count().saturating_sub(1) as u16,
            b'v' | b' ' | b'V' => {
                let linewise = k == b'V';
                if self.anchor.is_some() && self.linewise == linewise {
                    self.anchor = None;
                } else {
                    self.anchor.get_or_insert(self.cursor);
                    self.linewise = linewise;
                }
            }
            b'y' | b'\r' => {
                let text = match self.selection() {
                    Some((a, b)) => self.extract(p, a, b),
                    None => line_text(p, self.cursor.0),
                };
                return (used, Action::Yank(text));
            }
            b'/' | b'?' => {
                self.search = Some(String::new());
                self.search_up = k == b'?';
            }
            b'n' => self.find(p, self.search_up),
            b'N' => self.find(p, !self.search_up),
            _ => {}
        }
        self.sync(p);
        (used, Action::Stay)
    }

    fn search_key<CB: vt100::Callbacks>(&mut self, bytes: &[u8], p: &mut vt100::Parser<CB>) -> (usize, Action) {
        let Some(query) = self.search.as_mut() else { return (bytes.len(), Action::Stay) };
        match bytes[0] {
            b'\r' | b'\n' => {
                let q = std::mem::take(query);
                self.search = None;
                if !q.is_empty() {
                    self.last_search = Some(q);
                }
                self.find(p, self.search_up);
                self.sync(p);
                (1, Action::Stay)
            }
            0x1b | 0x03 => {
                self.search = None;
                (bytes.len(), Action::Stay)
            }
            0x7f | 0x08 => {
                query.pop();
                (1, Action::Stay)
            }
            _ => {
                // Take printable input up to the next control byte (e.g. the Enter
                // that arrives in the same read as a fast-typed query).
                let end = bytes.iter().position(|b| b.is_ascii_control()).unwrap_or(bytes.len());
                query.push_str(&String::from_utf8_lossy(&bytes[..end]));
                (end, Action::Stay)
            }
        }
    }

    /// Move to the next match of the last search; `older` = upward into history.
    /// Smartcase: case-insensitive unless the query has an uppercase letter.
    fn find<CB: vt100::Callbacks>(&mut self, p: &mut vt100::Parser<CB>, older: bool) {
        let Some(q) = self.last_search.clone() else { return };
        let fold = !q.chars().any(char::is_uppercase);
        let needle = if fold { q.to_lowercase() } else { q.clone() };
        let (sb, rows, _) = dims(p);
        let total = sb + rows as usize;
        let lines: Box<dyn Iterator<Item = usize>> = if older {
            Box::new((0..self.cursor.0).rev())
        } else {
            Box::new(self.cursor.0 + 1..total)
        };
        for abs in lines {
            let text = line_text(p, abs);
            let hay = if fold { text.to_lowercase() } else { text };
            if let Some(byte) = hay.find(&needle) {
                self.cursor = (abs, hay[..byte].chars().count() as u16);
                return;
            }
        }
        self.message = Some(format!("not found: {q}"));
    }

    /// Text between two positions (inclusive), rejoining soft-wrapped lines.
    fn extract<CB: vt100::Callbacks>(&self, p: &mut vt100::Parser<CB>, a: Pos, b: Pos) -> String {
        let mut out = String::new();
        for abs in a.0..=b.0 {
            let (cells, wrapped) = line_cells(p, abs);
            let from = if abs == a.0 { a.1 as usize } else { 0 };
            let to = if abs == b.0 { (b.1 as usize + 1).min(cells.len()) } else { cells.len() };
            let piece = cells.get(from..to.max(from)).map(|c| c.concat()).unwrap_or_default();
            if abs != b.0 && wrapped {
                out.push_str(&piece);
            } else {
                out.push_str(piece.trim_end());
                if abs != b.0 {
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// OSC 52 sequence that asks the outer terminal to set its clipboard.
pub fn osc52(text: &str) -> Vec<u8> {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes())).into_bytes()
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU16;

    fn parser(rows: u16, cols: u16, text: &str) -> vt100::Parser {
        let mut p = vt100::Parser::new(NonZeroU16::new(rows).unwrap(), NonZeroU16::new(cols).unwrap(), 100);
        p.process(text.as_bytes());
        p
    }

    fn keys(c: &mut Copy, p: &mut vt100::Parser, input: &[u8]) -> Option<String> {
        let mut rest = input;
        while !rest.is_empty() {
            let (n, act) = c.key(rest, p);
            rest = &rest[n.max(1)..];
            match act {
                Action::Yank(t) => return Some(t),
                Action::Exit => return None,
                Action::Stay => {}
            }
        }
        None
    }

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(osc52("hi"), b"\x1b]52;c;aGk=\x07".to_vec());
    }

    #[test]
    fn scrolls_into_history_and_yanks_a_line() {
        let text: String = (1..=20).map(|i| format!("line {i}\r\n")).collect();
        let mut p = parser(5, 20, &text);
        assert!(p.screen().scrollback_len() >= 15);
        let mut c = Copy::enter(1, &p);
        // Jump to the top of history: the view scrolls and line 1 is yankable.
        assert_eq!(keys(&mut c, &mut p, b"gy").as_deref(), Some("line 1"));
        assert_eq!(c.offset, p.screen().scrollback_len());
        assert_eq!(p.screen().scrollback(), c.offset);
    }

    #[test]
    fn selection_spans_lines_and_rejoins_wraps() {
        // 10 cols: "abcdefghijKLM" soft-wraps after j.
        let mut p = parser(6, 10, "first\r\nabcdefghijKLM\r\nlast\r\n");
        let mut c = Copy::enter(1, &p);
        c.cursor = (0, 2);
        let got = keys(&mut c, &mut p, b"vjjj$y").unwrap();
        assert_eq!(got, "rst\nabcdefghijKLM\nlast");
    }

    #[test]
    fn linewise_selection_copies_whole_lines() {
        let mut p = parser(6, 20, "alpha one\r\nbeta two\r\ngamma\r\n");
        let mut c = Copy::enter(1, &p);
        c.cursor = (0, 4);
        assert_eq!(keys(&mut c, &mut p, b"Vjy").unwrap(), "alpha one\nbeta two");
        // `v` after `V` switches the same selection to charwise.
        let mut c = Copy::enter(1, &p);
        c.cursor = (0, 6);
        assert_eq!(keys(&mut c, &mut p, b"Vjvy").unwrap(), "one\nbeta tw");
    }

    #[test]
    fn search_finds_older_matches_with_smartcase() {
        let text: String = (1..=30).map(|i| format!("row {i} {}\r\n", if i == 7 { "Needle" } else { "hay" })).collect();
        let mut p = parser(5, 30, &text);
        let mut c = Copy::enter(1, &p);
        // `?` searches up from the live bottom…
        assert!(keys(&mut c, &mut p, b"?needle\r").is_none());
        assert_eq!(line_text(&mut p, c.cursor.0), "row 7 Needle");
        assert_eq!(c.cursor.1, 6);
        // …`/` searches down from the top.
        keys(&mut c, &mut p, b"g/needle\r");
        assert_eq!(line_text(&mut p, c.cursor.0), "row 7 Needle");
        // Uppercase query is case-sensitive.
        keys(&mut c, &mut p, b"G?NEEDLE\r");
        assert_eq!(c.message.as_deref(), Some("not found: NEEDLE"));
    }

    #[test]
    fn escape_and_q_exit() {
        let mut p = parser(5, 20, "x\r\n");
        let mut c = Copy::enter(1, &p);
        assert!(matches!(c.key(b"q", &mut p).1, Action::Exit));
        assert!(matches!(c.key(b"\x1b", &mut p).1, Action::Exit));
        // An arrow key is not a bare escape.
        assert!(matches!(c.key(b"\x1b[A", &mut p).1, Action::Stay));
    }
}
