//! Colour themes for velmux's own chrome (sidebar, bars, popups). Pane
//! contents keep the colours their apps chose.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use ratatui::style::Color;

/// Every colour the UI uses, by role.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: String,
    /// Primary text, and the selected/current row's text.
    pub text: Color,
    /// Secondary text (unselected rows, labels).
    pub text2: Color,
    /// Hints, separators, headings.
    pub dim: Color,
    /// Highlight edge, buttons, insertion line, keys in hints.
    pub accent: Color,
    /// Background of the current row / selected menu item.
    pub selected_bg: Color,
    /// Glyphs: `◆` base, `▣` slot.
    pub base_glyph: Color,
    pub slot_glyph: Color,
    /// `*` uncommitted changes.
    pub dirty: Color,
    /// Agent states.
    pub working: Color,
    pub blocked: Color,
    pub done: Color,
    pub idle: Color,
    /// Focused pane's title bar / active tab.
    pub focus_bg: Color,
    pub focus_fg: Color,
    /// Unfocused pane title bars.
    pub title_bg: Color,
    pub title_fg: Color,
    /// Status line.
    pub bar_bg: Color,
    pub bar_fg: Color,
    /// Status line while prompting / in a mode.
    pub alert: Color,
    /// Background jobs in the status line.
    pub jobs: Color,
    pub menu_border: Color,
    pub picker_border: Color,
    pub toast_border: Color,
    /// Copy-mode selection.
    pub copy_bg: Color,
    pub copy_fg: Color,
    /// Copy-mode position tag.
    pub tag_bg: Color,
    pub tag_fg: Color,
}

pub const NAMES: &[&str] = &[
    "default",
    "rose-pine",
    "rose-pine-moon",
    "rose-pine-dawn",
    "catppuccin-mocha",
    "catppuccin-latte",
    "tokyonight",
    "gruvbox",
    "nord",
];

fn hex(s: &str) -> Color {
    parse_color(s).expect("built-in colours are valid")
}

/// Build a theme from a small palette, the way the named schemes describe
/// themselves (base surfaces, text levels, and a handful of hues).
#[allow(clippy::too_many_arguments)]
fn from_palette(
    name: &str,
    surface: &str,
    overlay: &str,
    highlight: &str,
    muted: &str,
    subtle: &str,
    text: &str,
    accent: &str,
    rose: &str,
    foam: &str,
    gold: &str,
    love: &str,
    green: &str,
    on_light: &str,
) -> Theme {
    Theme {
        name: name.into(),
        text: hex(text),
        text2: hex(subtle),
        dim: hex(muted),
        accent: hex(accent),
        selected_bg: hex(overlay),
        base_glyph: hex(rose),
        slot_glyph: hex(foam),
        dirty: hex(gold),
        working: hex(gold),
        blocked: hex(love),
        done: hex(green),
        idle: hex(muted),
        focus_bg: hex(highlight),
        focus_fg: hex(text),
        title_bg: hex(surface),
        title_fg: hex(subtle),
        bar_bg: hex(surface),
        bar_fg: hex(text),
        alert: hex(gold),
        jobs: hex(foam),
        menu_border: hex(accent),
        picker_border: hex(accent),
        toast_border: hex(highlight),
        copy_bg: hex(foam),
        copy_fg: hex(on_light),
        tag_bg: hex(gold),
        tag_fg: hex(on_light),
    }
}

pub fn builtin(name: &str) -> Option<Theme> {
    Some(match name {
        // Today's look: the terminal's own palette, so it follows any terminal theme.
        "default" => Theme {
            name: "default".into(),
            text: Color::White,
            text2: Color::Gray,
            dim: Color::DarkGray,
            accent: Color::Indexed(39),
            selected_bg: Color::Indexed(236),
            base_glyph: Color::Magenta,
            slot_glyph: Color::Cyan,
            dirty: Color::Yellow,
            working: Color::Yellow,
            blocked: Color::Red,
            done: Color::Green,
            idle: Color::DarkGray,
            focus_bg: Color::Indexed(24),
            focus_fg: Color::White,
            title_bg: Color::Indexed(236),
            title_fg: Color::Gray,
            bar_bg: Color::Indexed(236),
            bar_fg: Color::White,
            alert: Color::Yellow,
            jobs: Color::Cyan,
            menu_border: Color::Magenta,
            picker_border: Color::Indexed(24),
            toast_border: Color::Indexed(60),
            copy_bg: Color::Indexed(111),
            copy_fg: Color::Black,
            tag_bg: Color::Yellow,
            tag_fg: Color::Black,
        },
        "rose-pine" => from_palette(
            "rose-pine", "#1f1d2e", "#26233a", "#403d52", "#6e6a86", "#908caa", "#e0def4", "#c4a7e7", "#ebbcba",
            "#9ccfd8", "#f6c177", "#eb6f92", "#31748f", "#191724",
        ),
        "rose-pine-moon" => from_palette(
            "rose-pine-moon", "#2a273f", "#393552", "#44415a", "#6e6a86", "#908caa", "#e0def4", "#c4a7e7",
            "#ea9a97", "#9ccfd8", "#f6c177", "#eb6f92", "#3e8fb0", "#232136",
        ),
        "rose-pine-dawn" => from_palette(
            "rose-pine-dawn", "#fffaf3", "#f2e9e1", "#dfdad9", "#9893a5", "#797593", "#575279", "#907aa9",
            "#d7827e", "#56949f", "#ea9d34", "#b4637a", "#286983", "#faf4ed",
        ),
        "catppuccin-mocha" => from_palette(
            "catppuccin-mocha", "#181825", "#313244", "#45475a", "#6c7086", "#a6adc8", "#cdd6f4", "#cba6f7",
            "#f5c2e7", "#94e2d5", "#f9e2af", "#f38ba8", "#a6e3a1", "#1e1e2e",
        ),
        "catppuccin-latte" => from_palette(
            "catppuccin-latte", "#e6e9ef", "#ccd0da", "#bcc0cc", "#9ca0b0", "#6c6f85", "#4c4f69", "#8839ef",
            "#ea76cb", "#179299", "#df8e1d", "#d20f39", "#40a02b", "#eff1f5",
        ),
        "tokyonight" => from_palette(
            "tokyonight", "#16161e", "#292e42", "#394b70", "#565f89", "#a9b1d6", "#c0caf5", "#7aa2f7", "#bb9af7",
            "#7dcfff", "#e0af68", "#f7768e", "#9ece6a", "#1a1b26",
        ),
        "gruvbox" => from_palette(
            "gruvbox", "#3c3836", "#504945", "#665c54", "#928374", "#a89984", "#ebdbb2", "#83a598", "#d3869b",
            "#8ec07c", "#fabd2f", "#fb4934", "#b8bb26", "#282828",
        ),
        "nord" => from_palette(
            "nord", "#3b4252", "#434c5e", "#4c566a", "#616e88", "#d8dee9", "#eceff4", "#88c0d0", "#b48ead",
            "#8fbcbb", "#ebcb8b", "#bf616a", "#a3be8c", "#2e3440",
        ),
        _ => return None,
    })
}

/// `#rrggbb`, an ANSI index (`0`–`255`), a name (`red`, `darkgray`, …), or `reset`.
pub fn parse_color(s: &str) -> Result<Color> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("colour {s:?}: expected #rrggbb");
        }
        let n = u32::from_str_radix(hex, 16)?;
        return Ok(Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8));
    }
    if let Ok(i) = s.parse::<u8>() {
        return Ok(Color::Indexed(i));
    }
    Ok(match s.to_ascii_lowercase().replace(['-', '_', ' '], "").as_str() {
        "reset" | "default" | "none" => Color::Reset,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        "white" => Color::White,
        _ => bail!("unknown colour {s:?} (use #rrggbb, 0-255, or a name)"),
    })
}

/// The configured theme with per-role overrides applied. Unknown names or roles
/// and bad colours fall back (with a warning) rather than failing.
pub fn resolve(name: Option<&str>, overrides: &BTreeMap<String, String>) -> (Theme, Vec<String>) {
    let mut warnings = Vec::new();
    let name = name.unwrap_or("default");
    let mut t = builtin(name).unwrap_or_else(|| {
        warnings.push(format!("theme {name:?} doesn't exist (try: {})", NAMES.join(", ")));
        builtin("default").unwrap()
    });
    for (role, value) in overrides {
        let color = match parse_color(value) {
            Ok(c) => c,
            Err(e) => {
                warnings.push(format!("theme_colors.{role}: {e:#}"));
                continue;
            }
        };
        let slot = match role.as_str() {
            "text" => &mut t.text,
            "text2" => &mut t.text2,
            "dim" => &mut t.dim,
            "accent" => &mut t.accent,
            "selected_bg" => &mut t.selected_bg,
            "base_glyph" => &mut t.base_glyph,
            "slot_glyph" => &mut t.slot_glyph,
            "dirty" => &mut t.dirty,
            "working" => &mut t.working,
            "blocked" => &mut t.blocked,
            "done" => &mut t.done,
            "idle" => &mut t.idle,
            "focus_bg" => &mut t.focus_bg,
            "focus_fg" => &mut t.focus_fg,
            "title_bg" => &mut t.title_bg,
            "title_fg" => &mut t.title_fg,
            "bar_bg" => &mut t.bar_bg,
            "bar_fg" => &mut t.bar_fg,
            "alert" => &mut t.alert,
            "jobs" => &mut t.jobs,
            "menu_border" => &mut t.menu_border,
            "picker_border" => &mut t.picker_border,
            "toast_border" => &mut t.toast_border,
            "copy_bg" => &mut t.copy_bg,
            "copy_fg" => &mut t.copy_fg,
            "tag_bg" => &mut t.tag_bg,
            "tag_fg" => &mut t.tag_fg,
            other => {
                warnings.push(format!("theme_colors: unknown role {other:?}"));
                continue;
            }
        };
        *slot = color;
    }
    (t, warnings)
}

/// Set `theme = "<name>"` in a config file's text: replace a top-level `theme =`
/// line, or insert one before the first `[table]` (keeping everything else).
pub fn set_theme_line(config: &str, name: &str) -> String {
    let line = format!("theme = \"{name}\"");
    let mut out: Vec<String> = Vec::new();
    let mut done = false;
    let mut in_table = false;
    for l in config.lines() {
        let trimmed = l.trim_start();
        if trimmed.starts_with('[') {
            if !done {
                // With the other top-level settings, before the blank line(s) above the table.
                let blanks = out.iter().rev().take_while(|l| l.trim().is_empty()).count();
                out.insert(out.len() - blanks, line.clone());
                done = true;
            }
            in_table = true;
        }
        let is_theme = !in_table && trimmed.strip_prefix("theme").is_some_and(|r| r.trim_start().starts_with('='));
        if is_theme && !done {
            out.push(line.clone());
            done = true;
        } else if !is_theme {
            out.push(l.to_string());
        }
    }
    if !done {
        out.push(line);
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_resolves() {
        for n in NAMES {
            assert_eq!(builtin(n).unwrap().name, *n);
        }
        assert!(builtin("nope").is_none());
    }

    #[test]
    fn parses_colours() {
        assert_eq!(parse_color("#c4a7e7").unwrap(), Color::Rgb(0xc4, 0xa7, 0xe7));
        assert_eq!(parse_color("39").unwrap(), Color::Indexed(39));
        assert_eq!(parse_color("Dark-Gray").unwrap(), Color::DarkGray);
        assert!(parse_color("#12345").is_err());
        assert!(parse_color("chartreuse").is_err());
    }

    #[test]
    fn resolves_overrides_and_warns() {
        let mut o = BTreeMap::new();
        o.insert("accent".to_string(), "#ff0000".to_string());
        o.insert("bogus".to_string(), "red".to_string());
        o.insert("dim".to_string(), "not-a-colour".to_string());
        let (t, w) = resolve(Some("rose-pine"), &o);
        assert_eq!(t.accent, Color::Rgb(255, 0, 0));
        assert_eq!(t.dim, builtin("rose-pine").unwrap().dim);
        assert_eq!(w.len(), 2, "{w:?}");
        let (t, w) = resolve(Some("missing"), &BTreeMap::new());
        assert_eq!((t.name.as_str(), w.len()), ("default", 1));
    }

    #[test]
    fn writes_the_theme_line_in_place() {
        let cfg = "# top\nprefix = \"C-a\"\ntheme = \"nord\"\n\n[keys]\ntheme = \"not-this\"\n";
        assert_eq!(
            set_theme_line(cfg, "rose-pine"),
            "# top\nprefix = \"C-a\"\ntheme = \"rose-pine\"\n\n[keys]\ntheme = \"not-this\"\n"
        );
        assert_eq!(set_theme_line("prefix = \"C-a\"\n[notify]\nsound = true\n", "gruvbox"),
            "prefix = \"C-a\"\ntheme = \"gruvbox\"\n[notify]\nsound = true\n");
        assert_eq!(set_theme_line("", "nord"), "theme = \"nord\"\n");
        assert_eq!(
            set_theme_line("prefix = \"C-a\"\n\n[notify]\n", "nord"),
            "prefix = \"C-a\"\ntheme = \"nord\"\n\n[notify]\n"
        );
    }
}
