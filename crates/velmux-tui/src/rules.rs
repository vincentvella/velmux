//! Agent state from herdr's detection rules (`agent-rules/*.toml`, from
//! herdr's distribution/agent-detection, Apache-2.0; see NOTICE), for the
//! agents velmux has no hand-written rules for.
//!
//! Each rule names a state, a priority and a region of the agent's output (its
//! window title, its terminal progress report, the screen, or its last few
//! non-empty lines), and conditions on it: `contains` (every needle,
//! ignoring case), `regex` (every pattern, anywhere), `line_regex` (every
//! pattern, on some line), and nested `all` / `any` / `not` groups of the
//! same. The highest-priority rule that matches wins; none means idle. This is
//! herdr's engine, for the regions these files use.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;

use crate::detect::AgentState;

/// velmux's name for an agent → its rules.
const FILES: &[(&str, &str)] = &[
    ("copilot", include_str!("../agent-rules/github-copilot.toml")),
    ("cursor-agent", include_str!("../agent-rules/cursor.toml")),
    ("droid", include_str!("../agent-rules/droid.toml")),
    ("devin", include_str!("../agent-rules/devin.toml")),
    ("qwen", include_str!("../agent-rules/qwen.toml")),
    ("grok", include_str!("../agent-rules/grok.toml")),
    ("qodercli", include_str!("../agent-rules/qodercli.toml")),
    ("kimi", include_str!("../agent-rules/kimi.toml")),
    ("letta", include_str!("../agent-rules/letta.toml")),
    ("agy", include_str!("../agent-rules/antigravity.toml")),
    ("kilo", include_str!("../agent-rules/kilo.toml")),
    ("pi", include_str!("../agent-rules/pi.toml")),
    // oh-my-pi is pi's fork, with its TUI.
    ("omp", include_str!("../agent-rules/pi.toml")),
    ("hermes", include_str!("../agent-rules/hermes.toml")),
];

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    rules: Vec<RuleSpec>,
}

#[derive(Deserialize)]
struct RuleSpec {
    state: Option<String>,
    #[serde(default)]
    priority: i64,
    #[serde(default = "whole_recent")]
    region: String,
    #[serde(flatten)]
    gate: GateSpec,
}

fn whole_recent() -> String {
    "whole_recent".into()
}

#[derive(Deserialize, Default)]
struct GateSpec {
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
    #[serde(default)]
    all: Vec<GateSpec>,
    #[serde(default)]
    any: Vec<GateSpec>,
    #[serde(default)]
    not: Vec<GateSpec>,
}

struct Gate {
    contains: Vec<String>,
    regex: Vec<Regex>,
    line_regex: Vec<Regex>,
    all: Vec<Gate>,
    any: Vec<Gate>,
    not: Vec<Gate>,
}

struct Rule {
    state: Option<AgentState>,
    priority: i64,
    region: Region,
    gate: Gate,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Region {
    Title,
    Progress,
    Screen,
    Bottom(usize),
    Top(usize),
    /// One these files don't use (herdr's Codex-specific ones): never matches.
    Unknown,
}

fn region(spec: &str) -> Region {
    let count = |name: &str| spec.strip_prefix(name)?.strip_prefix('(')?.strip_suffix(')')?.parse().ok();
    match spec.trim() {
        "osc_title" => Region::Title,
        "osc_progress" => Region::Progress,
        "whole_recent" => Region::Screen,
        _ => match (count("bottom_non_empty_lines"), count("top_non_empty_lines")) {
            (Some(n), _) => Region::Bottom(n),
            (_, Some(n)) => Region::Top(n),
            _ => Region::Unknown,
        },
    }
}

fn compile(g: GateSpec) -> Result<Gate, regex::Error> {
    Ok(Gate {
        contains: g.contains.into_iter().map(|n| n.to_lowercase()).collect(),
        regex: g.regex.iter().map(|p| Regex::new(p)).collect::<Result<_, _>>()?,
        line_regex: g.line_regex.iter().map(|p| Regex::new(p)).collect::<Result<_, _>>()?,
        all: g.all.into_iter().map(compile).collect::<Result<_, _>>()?,
        any: g.any.into_iter().map(compile).collect::<Result<_, _>>()?,
        not: g.not.into_iter().map(compile).collect::<Result<_, _>>()?,
    })
}

fn state(name: &str) -> Option<AgentState> {
    match name {
        "working" => Some(AgentState::Working),
        "blocked" => Some(AgentState::Blocked),
        "idle" => Some(AgentState::Idle),
        _ => None,
    }
}

/// Every agent's rules, parsed and compiled once (a file that doesn't is
/// skipped, and that agent falls back to "working while output flows").
fn all() -> &'static HashMap<&'static str, Vec<Rule>> {
    static RULES: OnceLock<HashMap<&'static str, Vec<Rule>>> = OnceLock::new();
    RULES.get_or_init(|| {
        FILES
            .iter()
            .filter_map(|(agent, text)| {
                let file: File = toml::from_str(text).ok()?;
                let rules = file
                    .rules
                    .into_iter()
                    .map(|r| Ok(Rule { state: r.state.as_deref().and_then(state), priority: r.priority, region: region(&r.region), gate: compile(r.gate)? }))
                    .collect::<Result<Vec<_>, regex::Error>>()
                    .ok()?;
                Some((*agent, rules))
            })
            .collect()
    })
}

/// `agent` has rules here.
#[cfg(test)]
fn known(agent: &str) -> bool {
    all().contains_key(agent)
}

fn matches(g: &Gate, text: &str, lower: &str) -> bool {
    g.contains.iter().all(|n| lower.contains(n.as_str()))
        && g.regex.iter().all(|r| r.is_match(text))
        && g.line_regex.iter().all(|r| text.lines().any(|l| r.is_match(l)))
        && g.all.iter().all(|n| matches(n, text, lower))
        && (g.any.is_empty() || g.any.iter().any(|n| matches(n, text, lower)))
        && !g.not.iter().any(|n| matches(n, text, lower))
}

/// The text of `region`: the title, the progress report, or the screen
/// (`lines`, top to bottom, trailing spaces trimmed), whole or in part.
fn text_of(region: Region, title: &str, progress: &str, lines: &[&str]) -> String {
    let non_empty = || lines.iter().copied().filter(|l| !l.trim().is_empty());
    match region {
        Region::Title => title.to_string(),
        Region::Progress => progress.to_string(),
        Region::Screen => lines.join("\n"),
        Region::Bottom(n) => {
            let mut tail: Vec<&str> = non_empty().rev().take(n).collect();
            tail.reverse();
            // From the first of those lines down, blank lines included.
            match tail.first().and_then(|first| lines.iter().position(|l| std::ptr::eq(*l, *first))) {
                Some(at) => lines[at..].join("\n"),
                None => String::new(),
            }
        }
        Region::Top(n) => {
            let last = non_empty().take(n).last();
            match last.and_then(|l| lines.iter().position(|x| std::ptr::eq(*x, l))) {
                Some(at) => lines[..=at].join("\n"),
                None => String::new(),
            }
        }
        Region::Unknown => String::new(),
    }
}

/// `agent`'s state from its rules, or `None` when it has none.
pub fn classify(agent: &str, title: &str, progress: &str, lines: &[&str]) -> Option<AgentState> {
    let rules = all().get(agent)?;
    let mut texts: HashMap<Region, (String, String)> = HashMap::new();
    let mut best: Option<&Rule> = None;
    for rule in rules {
        if rule.region == Region::Unknown || best.is_some_and(|b| b.priority >= rule.priority) {
            continue;
        }
        let (text, lower) = texts.entry(rule.region).or_insert_with(|| {
            let t = text_of(rule.region, title, progress, lines);
            let l = t.to_lowercase();
            (t, l)
        });
        if matches(&rule.gate, text, lower) {
            best = Some(rule);
        }
    }
    Some(best.and_then(|r| r.state).unwrap_or(AgentState::Idle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<&str> {
        s.lines().collect()
    }

    #[test]
    fn every_file_parses_and_compiles() {
        for (agent, _) in FILES {
            assert!(known(agent), "{agent}'s rules didn't load");
            assert!(!all()[agent].is_empty(), "{agent} has no rules");
        }
    }

    #[test]
    fn reads_each_agents_prompts() {
        // Copilot: a picker is blocked, the cancel hint alone is working.
        assert_eq!(classify("copilot", "", "", &lines("Allow this?\n> Yes\n  enter to confirm · esc to cancel")), Some(AgentState::Blocked));
        assert_eq!(classify("copilot", "", "", &lines("◎ Thinking (esc to cancel)")), Some(AgentState::Working));
        assert_eq!(classify("copilot", "", "", &lines("> ")), Some(AgentState::Idle));
        // Droid: its permission menu, and its working spinner.
        let ask = "Run command: rm -rf dist\n> Yes, allow\n  No, cancel\nUse ↑↓ to navigate, Enter to select, Esc to cancel";
        assert_eq!(classify("droid", "", "", &lines(ask)), Some(AgentState::Blocked));
        assert_eq!(classify("droid", "", "", &lines("⠋ Thinking… (esc to stop)")), Some(AgentState::Working));
        // Cursor Agent: its run-this-command prompt.
        let run = "Run this command?\n  npm test\n  Run (once) (y)\n  Skip (esc or n)\nWaiting for approval";
        assert_eq!(classify("cursor-agent", "", "", &lines(run)), Some(AgentState::Blocked));
        assert_eq!(classify("cursor-agent", "", "", &lines("⬢ Thinking\n  ctrl+c to stop")), Some(AgentState::Working));
        // Qwen Code: its window title says blocked / working.
        assert_eq!(classify("qwen", "✳ Qwen", "", &lines("")), Some(AgentState::Blocked));
        assert_eq!(classify("qwen", "◐ Qwen", "", &lines("")), Some(AgentState::Working));
        // Grok: progress reports (OSC 9;4) working.
        assert_eq!(classify("grok", "", "4;1;-1", &lines("")), Some(AgentState::Working));
        // Agents without rules aren't decided here.
        assert_eq!(classify("aider", "", "", &lines("")), None);
    }

    #[test]
    fn regions_cut_the_screen_like_herdr() {
        let screen = lines("top\n\nmiddle\n\nlast one\nlast two\n");
        assert_eq!(text_of(Region::Bottom(2), "", "", &screen), "last one\nlast two");
        assert_eq!(text_of(Region::Bottom(3), "", "", &screen), "middle\n\nlast one\nlast two");
        assert_eq!(text_of(Region::Top(2), "", "", &screen), "top\n\nmiddle");
        assert_eq!(region("bottom_non_empty_lines(8)"), Region::Bottom(8));
        assert_eq!(region("after_last_prompt_marker"), Region::Unknown);
    }
}
