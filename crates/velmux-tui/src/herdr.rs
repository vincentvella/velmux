//! Import a herdr session (github.com/herdrdev/herdr): each workspace whose
//! folder is in a git repo becomes that repo's base (or velmux slot) view, with
//! its active tab's split layout and panes. Panes keep their working directory;
//! agent panes come back as agents, and Claude resumes when herdr recorded the
//! session. The schema mirrors herdr's `src/persist/snapshot.rs` (format v3,
//! plus the older single-layout workspaces), read leniently.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use velmux_pool::{claude, Env, Pool};

use crate::layout::{Dir, Layout};
use crate::pane::Kind;

#[derive(Deserialize)]
struct Snapshot {
    #[serde(default)]
    workspaces: Vec<Workspace>,
}

#[derive(Deserialize)]
struct Workspace {
    #[serde(default)]
    custom_name: Option<String>,
    #[serde(default)]
    identity_cwd: Option<PathBuf>,
    #[serde(default)]
    worktree_space: Option<WorktreeSpace>,
    #[serde(default)]
    tabs: Vec<Tab>,
    #[serde(default)]
    active_tab: usize,
    // Pre-tabs workspaces carried one tab's fields inline.
    #[serde(default)]
    layout: Option<HLayout>,
    #[serde(default)]
    panes: Option<HashMap<u32, HPane>>,
    #[serde(default)]
    zoomed: bool,
    #[serde(default)]
    focused: Option<u32>,
}

#[derive(Deserialize)]
struct WorktreeSpace {
    checkout_path: PathBuf,
}

#[derive(Deserialize)]
struct Tab {
    layout: HLayout,
    panes: HashMap<u32, HPane>,
    #[serde(default)]
    zoomed: bool,
    #[serde(default)]
    focused: Option<u32>,
}

#[derive(Deserialize)]
enum HLayout {
    Pane(u32),
    Split { direction: HDir, ratio: f32, first: Box<HLayout>, second: Box<HLayout> },
}

#[derive(Deserialize)]
enum HDir {
    Horizontal,
    Vertical,
}

#[derive(Deserialize)]
struct HPane {
    cwd: PathBuf,
    #[serde(default)]
    agent_name: Option<String>,
    #[serde(default)]
    managed_agent_kind: Option<String>,
    #[serde(default)]
    agent_session: Option<AgentSession>,
    #[serde(default)]
    agent_resume: Option<AgentResume>,
}

#[derive(Deserialize)]
struct AgentSession {
    agent: String,
    kind: String,
    value: String,
}

#[derive(Deserialize)]
struct AgentResume {
    agent: String,
    argv: Vec<String>,
}

/// What to create, computed without touching the server (also used for `--dry-run`).
#[derive(Debug)]
pub struct Plan {
    pub views: Vec<PlanView>,
    /// `(workspace, reason)` for everything left out.
    pub skipped: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct PlanView {
    pub label: String,
    pub base: PathBuf,
    pub slot: Option<String>,
    /// The location's root folder (base checkout or slot).
    pub root: PathBuf,
    /// Leaves are herdr pane ids, remapped when the panes are created.
    pub layout: Layout,
    pub zoomed: Option<u64>,
    pub panes: Vec<PlanPane>,
}

#[derive(Debug, Clone)]
pub struct PlanPane {
    pub id: u64,
    pub kind: Kind,
    /// Working directory when it differs from the location's root.
    pub cwd: Option<PathBuf>,
    /// Claude session to resume.
    pub resume: Option<String>,
    /// Exact command to run instead of the configured agent (non-Claude agents).
    pub command: Option<String>,
    /// Why this pane exists, for the summary (e.g. the conversation it resumes).
    pub note: Option<String>,
}

/// herdr's session file: `$XDG_CONFIG_HOME/herdr` or `~/.config/herdr`, or a named
/// session under `sessions/<name>/`.
pub fn default_path(session: Option<&str>) -> Result<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir).join("herdr"),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config/herdr"),
    };
    let dir = match session {
        Some(name) => config.join("sessions").join(name),
        None => config,
    };
    Ok(dir.join("session.json"))
}

/// `with_latest_claude`: for workspaces without a Claude pane, add one resuming
/// the newest conversation that ran in that repo (herdr only records sessions
/// for panes that were running Claude when it saved).
pub fn plan(path: &Path, env: &Env, with_latest_claude: bool) -> Result<Plan> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let snapshot: Snapshot = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let mut plan = Plan { views: Vec::new(), skipped: Vec::new() };
    let mut seen: Vec<(PathBuf, Option<String>)> = Vec::new();

    for (n, ws) in snapshot.workspaces.into_iter().enumerate() {
        let Workspace { custom_name, identity_cwd, worktree_space, mut tabs, active_tab, layout, panes, zoomed, focused } = ws;
        if let (true, Some(layout), Some(panes)) = (tabs.is_empty(), layout, panes) {
            tabs.push(Tab { layout, panes, zoomed, focused });
        }
        let extra_tabs = tabs.len().saturating_sub(1);
        let Some(tab) = (active_tab < tabs.len()).then(|| tabs.swap_remove(active_tab)).or_else(|| tabs.pop()) else {
            plan.skipped.push((format!("workspace {}", n + 1), "no tabs".into()));
            continue;
        };
        let anchor = worktree_space
            .map(|w| w.checkout_path)
            .or(identity_cwd)
            .or_else(|| tab.panes.values().next().map(|p| p.cwd.clone()));
        let label = custom_name.unwrap_or_else(|| {
            anchor.as_ref().and_then(|a| a.file_name()).map_or(format!("workspace {}", n + 1), |f| f.to_string_lossy().into_owned())
        });
        let Some(anchor) = anchor else {
            plan.skipped.push((label, "no folder".into()));
            continue;
        };
        let Ok(pool) = Pool::discover(env.clone(), &anchor) else {
            plan.skipped.push((label, format!("{} is not in a git repo", anchor.display())));
            continue;
        };
        let slot = pool.slot_of(&anchor).ok().flatten();
        let key = (pool.base.clone(), slot.clone());
        if seen.contains(&key) {
            plan.skipped.push((label, "another herdr workspace already maps to this location".into()));
            continue;
        }
        seen.push(key);
        let root = match &slot {
            Some(id) => pool.status()?.into_iter().find(|s| &s.id == id).map(|s| s.path).unwrap_or_else(|| pool.base.clone()),
            None => pool.base.clone(),
        };

        let leaves = leaves(&tab.layout);
        let panes = leaves
            .iter()
            .filter_map(|id| tab.panes.get(id).map(|p| plan_pane(*id as u64, p, &root)))
            .collect::<Vec<_>>();
        if panes.is_empty() {
            plan.skipped.push((label, "no panes".into()));
            continue;
        }
        let zoomed = tab.zoomed.then_some(tab.focused).flatten().map(u64::from);
        if extra_tabs > 0 {
            plan.skipped.push((label.clone(), format!("{extra_tabs} other tab(s); velmux has one layout per location")));
        }
        plan.views.push(PlanView { label, base: pool.base.clone(), slot, root, layout: convert(&tab.layout), zoomed, panes });
    }
    if with_latest_claude {
        add_latest_claude(&mut plan, env);
    }
    Ok(plan)
}

fn add_latest_claude(plan: &mut Plan, env: &Env) {
    let mut taken: Vec<String> = plan.views.iter().flat_map(|v| &v.panes).filter_map(|p| p.resume.clone()).collect();
    for v in &mut plan.views {
        if v.panes.iter().any(|p| p.kind == Kind::Agent && p.command.is_none()) {
            continue;
        }
        let Some(found) = claude::newest_under(&env.claude_dir, &v.root, &taken) else { continue };
        taken.push(found.id.clone());
        let id = v.layout.leaves().into_iter().max().unwrap_or(0) + 1;
        let old = std::mem::replace(&mut v.layout, Layout::Leaf(0));
        v.layout = Layout::Split { dir: Dir::Right, ratio: 0.5, a: Box::new(old), b: Box::new(Layout::Leaf(id)) };
        let title: String = found.title.unwrap_or_default().chars().take(48).collect();
        let note = format!("latest Claude: \"{title}\" ({} ago)", age(found.modified));
        v.panes.push(PlanPane {
            id,
            kind: Kind::Agent,
            cwd: (found.cwd != v.root).then_some(found.cwd),
            resume: Some(found.id),
            command: None,
            note: Some(note),
        });
    }
}

fn age(t: std::time::SystemTime) -> String {
    let secs = t.elapsed().map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 48 * 3600 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

fn plan_pane(id: u64, p: &HPane, root: &Path) -> PlanPane {
    let cwd = (p.cwd != root && p.cwd.is_dir()).then(|| p.cwd.clone());
    let is_claude = |a: &str| a.eq_ignore_ascii_case("claude") || a.eq_ignore_ascii_case("claude-code");
    let session = p
        .agent_session
        .as_ref()
        .filter(|s| is_claude(&s.agent) && s.kind.eq_ignore_ascii_case("id"))
        .map(|s| s.value.clone());
    let resume_flag = p.agent_resume.as_ref().filter(|r| is_claude(&r.agent)).and_then(|r| {
        let at = r.argv.iter().position(|a| a == "--resume" || a == "-r")?;
        r.argv.get(at + 1).cloned()
    });
    let agent = p.agent_name.as_deref().or(p.managed_agent_kind.as_deref());
    let claude = session.is_some() || resume_flag.is_some() || agent.is_some_and(is_claude);
    let other_agent = p.agent_resume.as_ref().filter(|r| !is_claude(&r.agent));
    let (kind, command) = match (claude, other_agent, agent) {
        (true, _, _) => (Kind::Agent, None),
        // Non-Claude agents: rerun herdr's recorded resume command, or the agent itself.
        (false, Some(r), _) => (Kind::Agent, Some(shell_join(&r.argv))),
        (false, None, Some(a)) => (Kind::Agent, Some(a.to_string())),
        (false, None, None) => (Kind::Shell, None),
    };
    PlanPane { id, kind, cwd, resume: session.or(resume_flag), command, note: None }
}

fn leaves(l: &HLayout) -> Vec<u32> {
    match l {
        HLayout::Pane(id) => vec![*id],
        HLayout::Split { first, second, .. } => {
            let mut v = leaves(first);
            v.extend(leaves(second));
            v
        }
    }
}

/// herdr splits like ratatui: `Horizontal` lays children side by side.
fn convert(l: &HLayout) -> Layout {
    match l {
        HLayout::Pane(id) => Layout::Leaf(*id as u64),
        HLayout::Split { direction, ratio, first, second } => Layout::Split {
            dir: match direction {
                HDir::Horizontal => Dir::Right,
                HDir::Vertical => Dir::Down,
            },
            ratio: ratio.clamp(0.1, 0.9),
            a: Box::new(convert(first)),
            b: Box::new(convert(second)),
        },
    }
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c)) {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Plan {
    /// Human summary, for `--dry-run` and the import reply.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        for v in &self.views {
            let place = v.slot.as_deref().unwrap_or("base");
            let agents = v.panes.iter().filter(|p| p.kind == Kind::Agent).count();
            let resumes = v.panes.iter().filter(|p| p.resume.is_some()).count();
            out.push_str(&format!(
                "  + {} → {} › {place}: {} pane(s), {agents} agent(s), {resumes} Claude resume(s)\n",
                v.label,
                v.base.file_name().map(|f| f.to_string_lossy()).unwrap_or_default(),
                v.panes.len()
            ));
            for note in v.panes.iter().filter_map(|p| p.note.as_ref()) {
                out.push_str(&format!("      ↳ adds {note}\n"));
            }
        }
        for (label, why) in &self.skipped {
            out.push_str(&format!("  - {label}: {why}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git_repo(dir: &Path) {
        std::fs::create_dir_all(dir.join("packages/web")).unwrap();
        let ok = Command::new("sh")
            .arg("-c")
            .arg("git init -q -b main && git -c user.email=t@t -c user.name=t commit -q --allow-empty -m init")
            .current_dir(dir)
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }

    #[test]
    fn plans_layouts_agents_and_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo = root.join("app");
        git_repo(&repo);
        let plain = root.join("notes");
        std::fs::create_dir_all(&plain).unwrap();
        let session = serde_json::json!({
            "version": 3,
            "workspaces": [
                {
                    "custom_name": "App", "identity_cwd": repo, "active_tab": 0,
                    "tabs": [
                        {
                            "layout": {"Split": {"direction": "Horizontal", "ratio": 0.6,
                                "first": {"Pane": 1},
                                "second": {"Split": {"direction": "Vertical", "ratio": 0.5,
                                    "first": {"Pane": 2}, "second": {"Pane": 3}}}}},
                            "panes": {
                                "1": {"cwd": repo, "agent_name": "claude",
                                      "agent_session": {"source": "herdr:claude", "agent": "claude", "kind": "Id", "value": "abc-123"}},
                                "2": {"cwd": repo.join("packages/web")},
                                "3": {"cwd": repo, "agent_resume": {"source": "x", "agent": "codex", "argv": ["codex", "resume", "it's"]}}
                            },
                            "zoomed": true, "focused": 2
                        },
                        {"layout": {"Pane": 9}, "panes": {"9": {"cwd": repo}}, "zoomed": false}
                    ]
                },
                {"identity_cwd": plain, "tabs": [{"layout": {"Pane": 1}, "panes": {"1": {"cwd": plain}}, "zoomed": false}]},
                {"identity_cwd": repo, "tabs": [{"layout": {"Pane": 1}, "panes": {"1": {"cwd": repo}}, "zoomed": false}]},
                // Pre-tabs format.
                {"identity_cwd": repo.join("packages/web"), "layout": {"Pane": 4}, "panes": {"4": {"cwd": repo}}, "zoomed": false}
            ]
        });
        let file = root.join("session.json");
        std::fs::write(&file, session.to_string()).unwrap();
        let env = Env { global_config: root.join("none.toml"), claude_dir: root.join("claude") };

        let plan = plan(&file, &env, false).unwrap();
        assert_eq!(plan.views.len(), 1, "{plan:?}");
        let v = &plan.views[0];
        assert_eq!((v.label.as_str(), v.base.as_path(), v.slot.as_deref()), ("App", repo.as_path(), None));
        assert_eq!(v.zoomed, Some(2));
        let Layout::Split { dir: Dir::Right, ratio, b, .. } = &v.layout else { panic!("{:?}", v.layout) };
        assert!((ratio - 0.6).abs() < 1e-6);
        assert!(matches!(b.as_ref(), Layout::Split { dir: Dir::Down, .. }));

        let p: HashMap<u64, &PlanPane> = v.panes.iter().map(|p| (p.id, p)).collect();
        assert_eq!((p[&1].kind, p[&1].resume.as_deref(), p[&1].cwd.as_ref()), (Kind::Agent, Some("abc-123"), None));
        assert_eq!((p[&2].kind, p[&2].cwd.as_deref()), (Kind::Shell, Some(repo.join("packages/web").as_path())));
        assert_eq!((p[&3].kind, p[&3].command.as_deref()), (Kind::Agent, Some(r"codex resume 'it'\''s'")));

        let reasons: Vec<&str> = plan.skipped.iter().map(|(_, r)| r.as_str()).collect();
        assert_eq!(reasons.len(), 4, "{reasons:?}");
        assert!(reasons[0].contains("other tab"));
        assert!(reasons[1].contains("not in a git repo"));
        assert!(reasons[2].contains("already maps") && reasons[3].contains("already maps"), "{reasons:?}");
    }

    #[test]
    fn with_latest_claude_adds_a_resuming_pane() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo = root.join("app");
        git_repo(&repo);
        let env = Env { global_config: root.join("none.toml"), claude_dir: root.join("claude") };
        // A conversation that ran in a subfolder of the repo.
        let web = repo.join("packages/web");
        let pdir = claude::project_dir(&env.claude_dir, &web);
        std::fs::create_dir_all(&pdir).unwrap();
        let line = serde_json::json!({"type": "user", "cwd": web, "message": {"content": "ship the web build"}});
        std::fs::write(pdir.join("sess-1.jsonl"), format!("{line}\n")).unwrap();
        let session = serde_json::json!({"workspaces": [{"identity_cwd": repo, "tabs": [{
            "layout": {"Pane": 1}, "panes": {"1": {"cwd": repo}}, "zoomed": false}]}]});
        let file = root.join("session.json");
        std::fs::write(&file, session.to_string()).unwrap();

        let without = plan(&file, &env, false).unwrap();
        assert_eq!(without.views[0].panes.len(), 1);
        let with = plan(&file, &env, true).unwrap();
        let v = &with.views[0];
        assert_eq!(v.layout.leaves(), vec![1, 2]);
        let added = &v.panes[1];
        assert_eq!((added.kind, added.resume.as_deref(), added.cwd.as_deref()), (Kind::Agent, Some("sess-1"), Some(web.as_path())));
        assert!(with.describe().contains("ship the web build"), "{}", with.describe());
    }
}
