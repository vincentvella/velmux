//! The coding agents velmux knows how to run and follow: how to start one
//! (new or resuming a session, with velmux's instructions and the pool's
//! slots in reach), which session a running one is in, and whether a
//! session still exists to resume. Claude Code is the reference; Codex,
//! Gemini CLI and opencode each do it their own way.
//!
//! A pane's session is kept as a reference that names its agent:
//! `codex:<id>`, `gemini:<id>`, `opencode:<id>`, or a bare id for Claude
//! (what velmux always stored, so older saves keep working).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Agent {
    Claude,
    Codex,
    Gemini,
    Opencode,
}

pub const ALL: [Agent; 4] = [Agent::Claude, Agent::Codex, Agent::Gemini, Agent::Opencode];

impl Agent {
    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Gemini => "gemini",
            Agent::Opencode => "opencode",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
            Agent::Gemini => "Gemini CLI",
            Agent::Opencode => "opencode",
        }
    }

    pub fn from_name(name: &str) -> Option<Agent> {
        ALL.into_iter().find(|a| a.name() == name)
    }

    /// The agent a command line runs (`claude --model opus`, `/opt/bin/codex`).
    pub fn of_command(command: &str) -> Option<Agent> {
        let program = command.split_whitespace().next()?;
        Agent::from_name(Path::new(program).file_name()?.to_str()?)
    }

    /// Installed: on `PATH`.
    pub fn installed(self) -> bool {
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path).any(|d| d.join(self.name()).is_file())
    }

    /// The session reference for `id` (see the module docs).
    pub fn session_ref(self, id: &str) -> String {
        match self {
            Agent::Claude => id.to_string(),
            a => format!("{}:{id}", a.name()),
        }
    }
}

/// A session reference's agent and id.
pub fn parse_ref(session: &str) -> (Agent, &str) {
    match session.split_once(':') {
        Some((name, id)) => match Agent::from_name(name) {
            Some(a) => (a, id),
            None => (Agent::Claude, session),
        },
        None => (Agent::Claude, session),
    }
}

/// What velmux adds when it starts an agent in a repo.
#[derive(Default)]
pub struct Context {
    pub claude_dir: PathBuf,
    /// The pool's folder (the slots), to put in the agent's reach.
    pub pool_dir: Option<PathBuf>,
    /// velmux's instructions for an agent in the repo's checkout.
    pub prompt: Option<String>,
    /// Claude Code `--settings` (the checkout guard).
    pub claude_settings: Option<PathBuf>,
    /// Gemini CLI `--policy` (the checkout guard).
    pub gemini_policy: Option<PathBuf>,
    /// A file holding `prompt`, for agents that read instructions from files.
    pub prompt_file: Option<PathBuf>,
}

/// How to start an agent: its command line, the session it will run when
/// that's known up front (a reference), and environment to add.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Launch {
    pub command: String,
    pub session: Option<String>,
    pub env: Vec<(String, String)>,
}

pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A TOML basic string (for `codex -c key=value`).
fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))
}

/// The command line for `agent`, starting from `base` (your configured
/// command, flags and all), new or resuming session `resume` (an id).
pub fn launch(agent: Agent, base: &str, resume: Option<&str>, ctx: &Context) -> Launch {
    let mut cmd = base.to_string();
    let mut env = Vec::new();
    let pool = ctx.pool_dir.as_ref().map(|p| quote(&p.display().to_string()));
    let session;
    match agent {
        Agent::Claude => {
            if let Some(s) = &ctx.claude_settings {
                cmd += &format!(" --settings {}", quote(&s.display().to_string()));
            }
            if let (Some(pool), Some(prompt)) = (&pool, &ctx.prompt) {
                cmd += &format!(" --add-dir {pool} --append-system-prompt {}", quote(prompt));
            }
            let id = resume.map(String::from).unwrap_or_else(crate::session::new_session_id);
            cmd += &if resume.is_some() { format!(" --resume {id}") } else { format!(" --session-id {id}") };
            session = Some(id);
        }
        Agent::Codex => match resume {
            // A resumed session keeps the instructions it started with.
            Some(id) => {
                cmd += &format!(" resume {id}");
                session = Some(id.to_string());
            }
            None => {
                if let Some(pool) = &pool {
                    cmd += &format!(" --add-dir {pool}");
                }
                if let Some(prompt) = &ctx.prompt {
                    cmd += &format!(" -c {}", quote(&format!("developer_instructions={}", toml_string(prompt))));
                }
                // Codex picks its own id: found once its transcript appears.
                session = None;
            }
        },
        Agent::Gemini => {
            if let Some(pool) = &pool {
                cmd += &format!(" --include-directories {pool}");
            }
            if let Some(policy) = &ctx.gemini_policy {
                cmd += &format!(" --policy {}", quote(&policy.display().to_string()));
            }
            let id = resume.map(String::from).unwrap_or_else(crate::session::new_session_id);
            cmd += &if resume.is_some() { format!(" --resume {id}") } else { format!(" --session-id {id}") };
            session = Some(id);
        }
        Agent::Opencode => {
            if let Some(file) = &ctx.prompt_file {
                let config = serde_json::json!({ "instructions": [file] }).to_string();
                env.push(("OPENCODE_CONFIG_CONTENT".to_string(), config));
            }
            match resume {
                Some(id) => {
                    cmd += &format!(" -s {id}");
                    session = Some(id.to_string());
                }
                None => session = None,
            }
        }
    }
    Launch { command: cmd, session: session.map(|id| agent.session_ref(&id)), env }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

pub fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".codex"))
}

pub fn gemini_home() -> PathBuf {
    std::env::var_os("GEMINI_CLI_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".gemini"))
}

pub fn opencode_db() -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/share"));
    data.join("opencode/opencode.db")
}

fn millis(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    real(a) == real(b)
}

/// The session (a reference) the agent in a pane is running: process `pid`,
/// working in `cwd`, started at `since`. For agents without a per-process
/// record: the newest session begun in that folder since the pane started.
pub fn running_session(agent: Agent, pid: i32, cwd: Option<&Path>, since: SystemTime, claude_dir: &Path) -> Option<String> {
    match agent {
        Agent::Claude => velmux_pool::claude::running_session(claude_dir, pid),
        Agent::Codex => codex_session_since(&codex_home(), cwd?, since).map(|id| agent.session_ref(&id)),
        Agent::Opencode => opencode_session_since(&opencode_db(), cwd?, since).map(|id| agent.session_ref(&id)),
        // Started with `--session-id`; one started by hand isn't followed.
        Agent::Gemini => None,
    }
}

/// Codex rollouts: `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`, the first
/// line a `session_meta` with the session's `cwd` and `id`.
pub(crate) fn codex_rollouts(home: &Path, newer_than: Option<SystemTime>) -> Vec<(SystemTime, PathBuf)> {
    let mut out = Vec::new();
    let dirs = |p: &Path| std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect::<Vec<_>>();
    for year in dirs(&home.join("sessions")) {
        for month in dirs(&year) {
            for day in dirs(&month) {
                let modified = std::fs::metadata(&day).and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH);
                if newer_than.is_some_and(|t| modified < t) {
                    continue;
                }
                for f in std::fs::read_dir(&day).into_iter().flatten().flatten() {
                    let p = f.path();
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                        let m = f.metadata().and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH);
                        if newer_than.is_none_or(|t| m >= t) {
                            out.push((m, p));
                        }
                    }
                }
            }
        }
    }
    out.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    out
}

fn first_line(path: &Path) -> Option<serde_json::Value> {
    use std::io::BufRead;
    let mut line = String::new();
    std::io::BufReader::new(std::fs::File::open(path).ok()?).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

pub fn codex_session_since(home: &Path, cwd: &Path, since: SystemTime) -> Option<String> {
    let since = since.checked_sub(std::time::Duration::from_secs(5)).unwrap_or(since);
    codex_rollouts(home, Some(since)).into_iter().find_map(|(_, path)| {
        let meta = first_line(&path)?;
        let p = &meta["payload"];
        (meta["type"] == "session_meta" && p["cwd"].as_str().is_some_and(|c| same_dir(Path::new(c), cwd)))
            .then(|| p["id"].as_str().or(p["session_id"].as_str()).map(String::from))?
    })
}

fn sqlite(db: &Path, sql: &str) -> Option<String> {
    let out = Command::new("sqlite3").arg("-readonly").arg(db).arg(sql).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn sql_text(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn opencode_session_since(db: &Path, cwd: &Path, since: SystemTime) -> Option<String> {
    let dir = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let since = millis(since).saturating_sub(5000);
    let sql = format!(
        "select id from session where directory = {} and parent_id is null and time_created >= {since} order by time_created desc limit 1",
        sql_text(&dir.display().to_string())
    );
    sqlite(db, &sql).filter(|id| !id.is_empty())
}

/// Session `id` of `agent` still exists to resume.
pub fn session_exists(agent: Agent, id: &str, claude_dir: &Path) -> bool {
    match agent {
        Agent::Claude => velmux_pool::claude::session_exists(claude_dir, id),
        Agent::Codex => codex_rollouts(&codex_home(), None).iter().any(|(_, p)| p.to_string_lossy().contains(id)),
        Agent::Gemini => {
            let short: String = id.chars().take(8).collect();
            std::fs::read_dir(gemini_home().join("tmp")).into_iter().flatten().flatten().any(|project| {
                std::fs::read_dir(project.path().join("chats")).into_iter().flatten().flatten().any(|f| {
                    let name = f.file_name().to_string_lossy().into_owned();
                    name.starts_with("session-") && name.ends_with(&format!("-{short}.jsonl"))
                })
            })
        }
        // Without sqlite3 to look, try resuming: opencode says if it's gone.
        Agent::Opencode => sqlite(&opencode_db(), &format!("select count(*) from session where id = {}", sql_text(id))).is_none_or(|n| n != "0"),
    }
}

/// The session a reference names still exists to resume.
pub fn exists(session: &str, claude_dir: &Path) -> bool {
    let (agent, id) = parse_ref(session);
    session_exists(agent, id, claude_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context { pool_dir: Some("/w/.pools/app".into()), prompt: Some("plan here; don't edit".into()), ..Context::default() }
    }

    #[test]
    fn session_refs_name_their_agent() {
        assert_eq!(parse_ref("8f3a-uuid"), (Agent::Claude, "8f3a-uuid"));
        assert_eq!(parse_ref("codex:019f-uuid"), (Agent::Codex, "019f-uuid"));
        assert_eq!(parse_ref("opencode:ses_abc"), (Agent::Opencode, "ses_abc"));
        assert_eq!(Agent::Gemini.session_ref("x"), "gemini:x");
        assert_eq!(Agent::Claude.session_ref("x"), "x");
        assert_eq!(Agent::of_command("/opt/homebrew/bin/codex --model o4"), Some(Agent::Codex));
        assert_eq!(Agent::of_command("claude-wrapper"), None);
    }

    #[test]
    fn launches_each_agent_its_way() {
        let c = launch(Agent::Codex, "codex", None, &ctx());
        assert_eq!(c.command, "codex --add-dir '/w/.pools/app' -c 'developer_instructions=\"plan here; don'\\''t edit\"'");
        assert_eq!(c.session, None, "codex picks its own id");
        assert_eq!(launch(Agent::Codex, "codex", Some("019f"), &ctx()).command, "codex resume 019f");

        let g = launch(Agent::Gemini, "gemini", None, &Context { gemini_policy: Some("/s/p.toml".into()), ..ctx() });
        assert!(g.command.starts_with("gemini --include-directories '/w/.pools/app' --policy '/s/p.toml' --session-id "), "{}", g.command);
        assert!(g.session.as_deref().is_some_and(|s| s.starts_with("gemini:")));
        assert_eq!(launch(Agent::Gemini, "gemini", Some("abc"), &Context::default()).command, "gemini --resume abc");

        let o = launch(Agent::Opencode, "opencode", Some("ses_1"), &Context { prompt_file: Some("/s/velmux.md".into()), ..Context::default() });
        assert_eq!(o.command, "opencode -s ses_1");
        assert_eq!(o.env[0].0, "OPENCODE_CONFIG_CONTENT");
        assert!(o.env[0].1.contains("/s/velmux.md"));

        let claude = launch(Agent::Claude, "claude", Some("id1"), &ctx());
        assert_eq!(claude.command, "claude --add-dir '/w/.pools/app' --append-system-prompt 'plan here; don'\\''t edit' --resume id1");
    }

    #[test]
    fn finds_a_codex_session_by_folder_and_time() {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path().join("sessions/2026/10/21");
        std::fs::create_dir_all(&day).unwrap();
        let meta = |id: &str, cwd: &str| format!("{{\"timestamp\":\"x\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}}}\n");
        let started = SystemTime::now() - std::time::Duration::from_secs(1);
        std::fs::write(day.join("rollout-2026-10-21T10-00-00-aaa.jsonl"), meta("aaa", "/elsewhere")).unwrap();
        std::fs::write(day.join("rollout-2026-10-21T10-00-01-bbb.jsonl"), meta("bbb", tmp.path().to_str().unwrap())).unwrap();
        assert_eq!(codex_session_since(tmp.path(), tmp.path(), started).as_deref(), Some("bbb"));
        let later = SystemTime::now() + std::time::Duration::from_secs(60);
        assert_eq!(codex_session_since(tmp.path(), tmp.path(), later), None, "older than the pane: someone else's");
    }
}
