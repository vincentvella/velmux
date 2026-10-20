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
    #[serde(default)]
    custom_name: Option<String>,
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
    /// herdr's custom workspace name, kept as the location's name (repo targets;
    /// folders already use it as their id).
    pub name: Option<String>,
    pub target: Target,
    /// The location's root folder (base checkout, slot, or plain folder).
    pub root: PathBuf,
    pub tabs: Vec<PlanTab>,
    pub active_tab: usize,
}

#[derive(Debug)]
pub struct PlanTab {
    pub name: Option<String>,
    /// Leaves are herdr pane ids, remapped when the panes are created.
    pub layout: Layout,
    pub zoomed: Option<u64>,
    pub panes: Vec<PlanPane>,
}

impl PlanView {
    pub fn panes(&self) -> impl Iterator<Item = &PlanPane> {
        self.tabs.iter().flat_map(|t| &t.panes)
    }
}

/// Where an imported workspace lands.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// A git repo's base checkout or one of its slots.
    Repo { base: PathBuf, slot: Option<String> },
    /// A folder outside any repo, grouped under "unclassified" by `id`.
    Folder { id: String },
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
    let mut folder_ids: Vec<String> = Vec::new();

    for (n, ws) in snapshot.workspaces.into_iter().enumerate() {
        let Workspace { custom_name, identity_cwd, worktree_space, mut tabs, active_tab, layout, panes, zoomed, focused } = ws;
        if let (true, Some(layout), Some(panes)) = (tabs.is_empty(), layout, panes) {
            tabs.push(Tab { custom_name: None, layout, panes, zoomed, focused });
        }
        if tabs.is_empty() {
            plan.skipped.push((format!("workspace {}", n + 1), "no tabs".into()));
            continue;
        }
        let active_tab = active_tab.min(tabs.len() - 1);
        let anchor = worktree_space
            .map(|w| w.checkout_path)
            .or(identity_cwd)
            .or_else(|| tabs[active_tab].panes.values().next().map(|p| p.cwd.clone()));
        let custom = custom_name.clone();
        let label = custom_name.unwrap_or_else(|| {
            anchor.as_ref().and_then(|a| a.file_name()).map_or(format!("workspace {}", n + 1), |f| f.to_string_lossy().into_owned())
        });
        let Some(anchor) = anchor else {
            plan.skipped.push((label, "no folder".into()));
            continue;
        };
        let (target, root) = match Pool::discover(env.clone(), &anchor) {
            Ok(pool) => {
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
                (Target::Repo { base: pool.base.clone(), slot }, root)
            }
            Err(_) if anchor.is_dir() => {
                // Not a repo: keep it, grouped under "unclassified" by a unique name.
                let mut id = label.clone();
                let mut n = 2;
                while folder_ids.contains(&id) {
                    id = format!("{label}-{n}");
                    n += 1;
                }
                folder_ids.push(id.clone());
                (Target::Folder { id }, anchor.clone())
            }
            Err(_) => {
                plan.skipped.push((label, format!("{} no longer exists", anchor.display())));
                continue;
            }
        };

        let mut plan_tabs = Vec::new();
        let mut active = 0;
        for (i, tab) in tabs.iter().enumerate() {
            let mut panes = leaves(&tab.layout)
                .iter()
                .filter_map(|id| tab.panes.get(id).map(|p| plan_pane(*id as u64, p, &root)))
                .collect::<Vec<_>>();
            for p in &mut panes {
                if p.resume.as_ref().is_some_and(|id| !claude::session_exists(&env.claude_dir, id)) {
                    p.resume = None;
                    p.note = Some("recorded Claude session no longer exists; starts fresh".into());
                }
            }
            if panes.is_empty() {
                continue;
            }
            if i == active_tab {
                active = plan_tabs.len();
            }
            let zoomed = tab.zoomed.then_some(tab.focused).flatten().map(u64::from);
            plan_tabs.push(PlanTab { name: tab.custom_name.clone(), layout: convert(&tab.layout), zoomed, panes });
        }
        if plan_tabs.is_empty() {
            plan.skipped.push((label, "no panes".into()));
            continue;
        }
        let name = custom.filter(|_| matches!(target, Target::Repo { .. }));
        plan.views.push(PlanView { label, name, target, root, tabs: plan_tabs, active_tab: active });
    }
    if with_latest_claude {
        add_latest_claude(&mut plan, env);
    }
    Ok(plan)
}

fn add_latest_claude(plan: &mut Plan, env: &Env) {
    let mut taken: Vec<String> = plan.views.iter().flat_map(|v| v.panes()).filter_map(|p| p.resume.clone()).collect();
    let before = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    for v in &mut plan.views {
        if v.panes().any(|p| p.kind == Kind::Agent && p.command.is_none() && p.resume.is_some()) {
            continue;
        }
        let search = claude::Search {
            root: &v.root,
            // A plain folder like `~` contains everything; only take conversations held there.
            subfolders: matches!(v.target, Target::Repo { .. }),
            before,
            exclude: &taken,
        };
        let Some(found) = claude::newest_under(&env.claude_dir, &search) else { continue };
        taken.push(found.id.clone());
        let title: String = found.title.clone().unwrap_or_default().chars().take(48).collect();
        let ago = age(found.modified);
        // A Claude pane with nothing to resume picks the conversation up itself.
        let idle = v.tabs.iter_mut().flat_map(|t| &mut t.panes).find(|p| p.kind == Kind::Agent && p.command.is_none());
        if let Some(p) = idle {
            p.resume = Some(found.id);
            p.note = Some(format!("resumes latest Claude: \"{title}\" ({ago} ago)"));
            continue;
        }
        let id = v.tabs.iter().flat_map(|t| t.layout.leaves()).max().unwrap_or(0) + 1;
        let tab = &mut v.tabs[v.active_tab];
        let old = std::mem::replace(&mut tab.layout, Layout::Leaf(0));
        tab.layout = Layout::Split { dir: Dir::Right, ratio: 0.5, a: Box::new(old), b: Box::new(Layout::Leaf(id)) };
        let note = format!("adds latest Claude: \"{title}\" ({ago} ago)");
        tab.panes.push(PlanPane {
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
            let place = match &v.target {
                Target::Repo { base, slot } => format!(
                    "{} › {}",
                    base.file_name().map(|f| f.to_string_lossy()).unwrap_or_default(),
                    slot.as_deref().unwrap_or("base")
                ),
                Target::Folder { id } => format!("unclassified › {id} ({})", v.root.display()),
            };
            let agents = v.panes().filter(|p| p.kind == Kind::Agent).count();
            let resumes = v.panes().filter(|p| p.resume.is_some()).count();
            let tabs = if v.tabs.len() > 1 { format!("{} tabs, ", v.tabs.len()) } else { String::new() };
            out.push_str(&format!(
                "  + {} → {place}: {tabs}{} pane(s), {agents} agent(s), {resumes} Claude resume(s)\n",
                v.label,
                v.panes().count()
            ));
            for note in v.panes().filter_map(|p| p.note.as_ref()) {
                out.push_str(&format!("      ↳ {note}\n"));
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
        // herdr's recorded session must still exist for it to be resumed.
        let pdir = claude::project_dir(&env.claude_dir, &repo);
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(pdir.join("abc-123.jsonl"), "{}\n").unwrap();

        let plan = plan(&file, &env, false).unwrap();
        assert_eq!(plan.views.len(), 2, "{plan:?}");
        let v = &plan.views[0];
        assert_eq!(v.label, "App");
        assert_eq!(v.target, Target::Repo { base: repo.clone(), slot: None });
        // The folder outside any repo is kept, as unclassified.
        assert_eq!(plan.views[1].target, Target::Folder { id: "notes".into() });
        assert_eq!(plan.views[1].root, plain);
        // Both herdr tabs come over; the first is active.
        assert_eq!((v.tabs.len(), v.active_tab), (2, 0));
        let t = &v.tabs[0];
        assert_eq!(t.zoomed, Some(2));
        let Layout::Split { dir: Dir::Right, ratio, b, .. } = &t.layout else { panic!("{:?}", t.layout) };
        assert!((ratio - 0.6).abs() < 1e-6);
        assert!(matches!(b.as_ref(), Layout::Split { dir: Dir::Down, .. }));

        let p: HashMap<u64, &PlanPane> = t.panes.iter().map(|p| (p.id, p)).collect();
        assert_eq!((p[&1].kind, p[&1].resume.as_deref(), p[&1].cwd.as_ref()), (Kind::Agent, Some("abc-123"), None));
        assert_eq!((p[&2].kind, p[&2].cwd.as_deref()), (Kind::Shell, Some(repo.join("packages/web").as_path())));
        assert_eq!((p[&3].kind, p[&3].command.as_deref()), (Kind::Agent, Some(r"codex resume 'it'\''s'")));

        let reasons: Vec<&str> = plan.skipped.iter().map(|(_, r)| r.as_str()).collect();
        assert_eq!(reasons.len(), 2, "{reasons:?}");
        assert!(reasons[0].contains("already maps") && reasons[1].contains("already maps"), "{reasons:?}");
    }

    #[test]
    fn unclassified_folders_resume_only_their_own_conversations() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        let deep = home.join("Downloads/thing");
        std::fs::create_dir_all(&deep).unwrap();
        let env = Env { global_config: root.join("none.toml"), claude_dir: root.join("claude") };
        let convo = |cwd: &Path, id: &str, prompt: &str| {
            let dir = claude::project_dir(&env.claude_dir, cwd);
            std::fs::create_dir_all(&dir).unwrap();
            let line = serde_json::json!({"type": "user", "cwd": cwd, "message": {"content": prompt}});
            std::fs::write(dir.join(format!("{id}.jsonl")), format!("{line}\n")).unwrap();
            // Old enough not to count as an open conversation.
            let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
            std::fs::File::options().write(true).open(dir.join(format!("{id}.jsonl"))).unwrap().set_modified(old).unwrap();
        };
        convo(&home, "home-1", "plan my week");
        convo(&deep, "deep-1", "something nested");
        // Two herdr workspaces at the same non-repo folder, one with a stale Claude pane.
        let session = serde_json::json!({"workspaces": [
            {"custom_name": "Notes", "identity_cwd": home, "tabs": [{"layout": {"Pane": 1},
                "panes": {"1": {"cwd": home, "agent_name": "claude",
                    "agent_session": {"source": "s", "agent": "claude", "kind": "Id", "value": "gone"}}}, "zoomed": false}]},
            {"custom_name": "Notes", "identity_cwd": home, "tabs": [{"layout": {"Pane": 1},
                "panes": {"1": {"cwd": home}}, "zoomed": false}]}
        ]});
        let file = root.join("session.json");
        std::fs::write(&file, session.to_string()).unwrap();

        let plan = plan(&file, &env, true).unwrap();
        let ids: Vec<_> = plan.views.iter().map(|v| v.target.clone()).collect();
        assert_eq!(ids, [Target::Folder { id: "Notes".into() }, Target::Folder { id: "Notes-2".into() }]);
        // The stale pane picks up the folder's own conversation, never the nested one.
        let first = &plan.views[0].tabs[0].panes[0];
        assert_eq!((first.resume.as_deref(), plan.views[0].panes().count()), (Some("home-1"), 1));
        // The second workspace finds nothing left at exactly `home`.
        assert_eq!(plan.views[1].panes().count(), 1);
        assert_eq!(plan.views[1].tabs[0].panes[0].resume, None);
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
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options().write(true).open(pdir.join("sess-1.jsonl")).unwrap().set_modified(old).unwrap();
        let session = serde_json::json!({"workspaces": [{"identity_cwd": repo, "tabs": [{
            "layout": {"Pane": 1}, "panes": {"1": {"cwd": repo}}, "zoomed": false}]}]});
        let file = root.join("session.json");
        std::fs::write(&file, session.to_string()).unwrap();

        let without = plan(&file, &env, false).unwrap();
        assert_eq!(without.views[0].panes().count(), 1);
        let with = plan(&file, &env, true).unwrap();
        let v = &with.views[0].tabs[0];
        assert_eq!(v.layout.leaves(), vec![1, 2]);
        let added = &v.panes[1];
        assert_eq!((added.kind, added.resume.as_deref(), added.cwd.as_deref()), (Kind::Agent, Some("sess-1"), Some(web.as_path())));
        assert!(with.describe().contains("ship the web build"), "{}", with.describe());
    }
}
