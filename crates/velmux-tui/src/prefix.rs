//! The prefix key that switches input from the focused pane to velmux commands.

use anyhow::{bail, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefix {
    /// Byte the terminal sends for the chord.
    pub byte: u8,
    /// Spelled out for hints, e.g. `ctrl+a`.
    pub label: String,
    /// Compact form for dense help text, e.g. `^a`.
    pub short: String,
}

impl Default for Prefix {
    fn default() -> Prefix {
        Prefix::parse("C-b").unwrap()
    }
}

impl Prefix {
    /// Accepts `C-a`, `ctrl+a`, `ctrl-a`, `^a` (any case), and the ctrl
    /// punctuation chords `space`/`@`, `\`, `]`, `^`, `_`.
    pub fn parse(spec: &str) -> Result<Prefix> {
        let lower = spec.trim().to_ascii_lowercase();
        let key = ["ctrl+", "ctrl-", "c-", "^"]
            .iter()
            .find_map(|p| lower.strip_prefix(p))
            .ok_or_else(|| anyhow::anyhow!("prefix {spec:?} must be a ctrl chord like \"C-a\""))?;
        let (byte, name) = match key {
            "space" | "@" => (0x00, "space".to_string()),
            k if k.len() == 1 => {
                let c = k.as_bytes()[0];
                let byte = match c {
                    b'a'..=b'z' => c - b'a' + 1,
                    b'\\' => 0x1c,
                    b']' => 0x1d,
                    b'^' => 0x1e,
                    b'_' => 0x1f,
                    b'[' => bail!("ctrl+[ is Escape and can't be the prefix"),
                    _ => bail!("unsupported prefix key {spec:?}"),
                };
                (byte, (c as char).to_string())
            }
            _ => bail!("unsupported prefix key {spec:?}"),
        };
        match byte {
            0x08 => bail!("ctrl+h is Backspace in most terminals and can't be the prefix"),
            0x09 => bail!("ctrl+i is Tab and can't be the prefix"),
            0x0a | 0x0d => bail!("ctrl+j / ctrl+m are Enter and can't be the prefix"),
            _ => {}
        }
        Ok(Prefix { byte, label: format!("ctrl+{name}"), short: format!("^{name}") })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_spellings() {
        for spec in ["C-a", "ctrl+a", "Ctrl-A", "^a", " c-a "] {
            let p = Prefix::parse(spec).unwrap();
            assert_eq!((p.byte, p.label.as_str(), p.short.as_str()), (0x01, "ctrl+a", "^a"), "{spec}");
        }
        assert_eq!(Prefix::default().byte, 0x02);
        assert_eq!(Prefix::parse("C-space").unwrap().byte, 0x00);
        assert_eq!(Prefix::parse("C-]").unwrap().byte, 0x1d);
    }

    #[test]
    fn rejects_keys_that_break_typing() {
        for spec in ["C-[", "C-m", "C-j", "C-i", "C-h", "a", "M-a", "C-1", "C-ab"] {
            assert!(Prefix::parse(spec).is_err(), "{spec} should be rejected");
        }
    }
}
