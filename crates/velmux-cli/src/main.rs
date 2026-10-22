use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use velmux_pool::{ClaimOpts, Env, ParkedView, Pool, SlotView, Status};

/// Worktree pools: a fixed set of warm slots per repo, claimed and released.
#[derive(Parser)]
#[command(name = "velmux", version)]
struct Cli {
    /// Operate on the repo containing this path (default: cwd).
    #[arg(short = 'C', long, global = true)]
    repo: Option<PathBuf>,
    /// No subcommand: attach the TUI (starting its server if needed).
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the pool's slots (idempotent; grows to the configured count).
    Init,
    /// Claim a free slot and print its path: `cd "$(velmux claim feat/x)"`.
    Claim {
        /// Branch to check out: existing local/remote, else created from the base ref.
        branch: Option<String>,
        /// Who holds the slot (agent name, session id, …).
        #[arg(long)]
        by: Option<String>,
        /// Put the slot in this workstream (made if needed) rather than move
        /// this pane into it; in velmux only. In a workstream, it's that one.
        #[arg(long)]
        workstream: Option<String>,
        #[arg(long)]
        no_fetch: bool,
        #[arg(long)]
        no_install: bool,
        #[arg(long)]
        json: bool,
    },
    /// Park a slot's work (stash if dirty) and return it to the pool clean.
    Release {
        /// Slot id; defaults to the slot containing the current directory.
        slot: Option<String>,
        /// Release even if the slot is already marked free.
        #[arg(long)]
        force: bool,
    },
    /// Rebase (or merge, per `sync` in config) a slot's branch onto the base ref.
    Sync {
        /// Slot id; defaults to the slot containing the current directory.
        slot: Option<String>,
    },
    /// Print a slot's environment (ports, `VELMUX_SLOT`) as shell exports:
    /// `eval "$(velmux env)"`, or `velmux env >> .envrc` for direnv.
    Env {
        /// Slot id; defaults to the slot containing the current directory.
        slot: Option<String>,
    },
    /// Show every slot.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// List branches with parked work (released with changes or commits);
    /// `velmux claim <branch>` resumes one.
    Parked {
        #[arg(long)]
        json: bool,
    },
    /// Throw a parked branch's stashed changes away (the branch is kept).
    Discard { branch: String },
    /// Print a slot's path.
    Path { slot: String },
    /// Open a slot in your editor (`editor` in config, else $VISUAL / $EDITOR).
    Open { slot: Option<String> },
    /// Prune dead worktrees and recreate missing slots.
    Gc,
    /// Import a herdr session: workspaces in git repos become velmux layouts.
    ImportHerdr {
        /// herdr session file (default: ~/.config/herdr/session.json).
        #[arg(long)]
        file: Option<PathBuf>,
        /// A named herdr session (`herdr --session NAME`).
        #[arg(long, conflicts_with = "file")]
        session: Option<String>,
        /// For workspaces without a Claude pane, add one resuming the newest
        /// Claude conversation from that repo.
        #[arg(long)]
        with_latest_claude: bool,
        /// Show what would be imported without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Restart the TUI server in place (new binary), keeping every pane running.
    Restart,
    /// Stop the TUI server: everything is saved and restored next time.
    Stop,
    /// Print the focused pane as the server holds it (sizes and rows).
    #[command(hide = true)]
    Debug,
    /// Install the Claude Code skill that teaches Claude to work in velmux
    /// (claiming, releasing, parked work, sync, ports, PRs).
    Skill {
        #[command(subcommand)]
        action: SkillCmd,
    },
    /// Show key bindings; `--ghostty` prints the Ghostty keybinds for the root ones.
    Keys {
        #[arg(long)]
        ghostty: bool,
        /// Show what each key press sends and which binding it hits.
        #[arg(long, conflicts_with = "ghostty")]
        test: bool,
        /// Write the Ghostty keybinds into Ghostty's config (replacing velmux's
        /// block if it's there).
        #[arg(long, conflicts_with = "test")]
        install: bool,
    },
    /// Check velmux's setup (git, your agent, the Claude skill, gh, your
    /// terminal, this repo's pool) and say how to fix what isn't right.
    Doctor,
    /// Update velmux the way it was installed (Homebrew, a downloaded binary,
    /// cargo); `--check` only says whether there's a newer one.
    Update {
        #[arg(long)]
        check: bool,
    },
    /// A Claude Code PreToolUse hook: refuses file edits inside a repo's
    /// checkout (`$VELMUX_BASE`) so agents work in slots. Reads the hook's
    /// JSON on stdin; exit 2 blocks the edit and tells Claude why.
    #[command(hide = true)]
    Guard,
    /// An agent's session-start hook (installed by `velmux skill install`):
    /// reads the hook's JSON on stdin and tells velmux which session the
    /// pane's agent is in, so it's resumed after a restart.
    #[command(name = "agent-hook", hide = true)]
    AgentHook { agent: String },
    /// Run the TUI server in the foreground (normally started by attaching).
    #[command(name = "__server", hide = true)]
    Server,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("velmux: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    // `--repo` in a workstream: a path (a subfolder links to a slot) or a repo's name.
    let repo_arg = cli.repo.as_ref().map(|p| p.canonicalize().map(|c| c.display().to_string()).unwrap_or_else(|_| p.display().to_string()));
    let cwd = match cli.repo {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let cmd = match cli.cmd {
        None => return velmux_tui::attach(cwd),
        Some(Cmd::Server) => return velmux_tui::serve(),
        Some(Cmd::Restart) => return velmux_tui::restart_server(),
        Some(Cmd::Skill { action }) => return skill(action, &cwd),
        Some(Cmd::Stop) => return velmux_tui::stop_server(),
        Some(Cmd::Debug) => return velmux_tui::debug_server(),
        Some(Cmd::Keys { test: true, .. }) => return velmux_tui::key_test(),
        Some(Cmd::Keys { ghostty, install, .. }) => return velmux_tui::print_keys(ghostty, install),
        Some(Cmd::Doctor) => return doctor(&cwd),
        Some(Cmd::Update { check }) => return update(check),
        Some(Cmd::Guard) => guard(),
        Some(Cmd::AgentHook { agent }) => agent_hook(&agent),
        Some(Cmd::ImportHerdr { file, session, with_latest_claude, dry_run }) => {
            return velmux_tui::import_herdr(file, session.as_deref(), with_latest_claude, dry_run)
        }
        // Inside a velmux pane: the server claims (it knows the pane's place:
        // a workstream there, or a repo whose slot this pane moves into).
        Some(Cmd::Claim { branch, workstream, json: false, .. }) if velmux_tui::current_pane().is_some() => {
            let (path, ws) = velmux_tui::claim_into(velmux_tui::current_pane().unwrap(), branch, repo_arg, workstream)?;
            match ws {
                Some(ws) => eprintln!("claimed a slot into the workstream {ws}; it's linked in its folder"),
                None => eprintln!("claimed a slot; work in it from here (this pane now lives there in velmux)"),
            }
            println!("{path}");
            return Ok(());
        }
        Some(Cmd::Claim { workstream: Some(_), .. }) => bail!("--workstream works inside velmux (a pane), where workstreams live"),
        Some(cmd) => cmd,
    };
    let pool = Pool::discover(Env::from_process()?, &cwd)?;
    let report = |m: &str| eprintln!("{m}");

    match cmd {
        Cmd::Init => {
            eprintln!("pool {} → {}", pool.name, pool.dir.display());
            print_table(&pool, &pool.init(&report)?);
            let env = Env::from_process()?;
            if !skill_path(&env.claude_dir).exists() && !cwd.join(".claude/skills/velmux/SKILL.md").exists() {
                eprintln!("tip: `velmux skill install` teaches Claude to claim and release slots itself");
            }
        }
        Cmd::Release { slot: None, force: false } if velmux_tui::current_pane().is_some() => {
            eprintln!("{}", velmux_tui::release_for_pane(velmux_tui::current_pane().unwrap())?);
        }
        Cmd::Claim { branch, by, no_fetch, no_install, json, .. } => {
            let opts = ClaimOpts { branch, by, fetch: !no_fetch, install: !no_install };
            let slot = pool.claim(&opts, &report)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&slot)?);
            } else {
                eprintln!("claimed {}", slot.id);
                println!("{}", slot.path.display());
            }
        }
        Cmd::Release { slot, force } => {
            let id = resolve_slot(&pool, slot, &cwd)?;
            let r = pool.release(&id, force)?;
            if let Some(msg) = &r.stashed {
                eprintln!("stashed uncommitted work: {msg}");
            }
            if let Some(b) = &r.rescued {
                eprintln!("detached commits kept on branch {b}");
            }
            eprintln!(
                "released {}{}",
                r.id,
                r.branch.map(|b| format!(" (branch {b} parked)")).unwrap_or_default()
            );
        }
        Cmd::Status { json } => {
            let slots = pool.status()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&slots)?);
            } else if slots.is_empty() {
                eprintln!("no pool for {} yet; run `velmux init`", pool.name);
            } else {
                print_table(&pool, &slots);
                print_parked(&pool.parked()?);
            }
        }
        Cmd::Env { slot } => {
            let id = resolve_slot(&pool, slot, &cwd)?;
            for (k, v) in pool.slot_env(&id) {
                println!("export {k}={}", shell_quote(&v));
            }
        }
        Cmd::Sync { slot } => {
            let id = resolve_slot(&pool, slot, &cwd)?;
            println!("{}", pool.sync(&id, &|m| eprintln!("{m}"))?);
        }
        Cmd::Parked { json } => {
            let parked = pool.parked()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&parked)?);
            } else if parked.is_empty() {
                eprintln!("nothing parked in {}", pool.name);
            } else {
                print_parked(&parked);
            }
        }
        Cmd::Discard { branch } => {
            if !pool.parked()?.iter().any(|p| p.branch == branch) {
                bail!("{branch} isn't parked (see `velmux parked`)");
            }
            let n = pool.discard(&branch)?;
            println!("discarded {n} stash(es) for {branch}; the branch itself is kept");
        }
        Cmd::Path { slot } => println!("{}", pool.view(&slot)?.path.display()),
        Cmd::Open { slot } => {
            let id = resolve_slot(&pool, slot, &cwd)?;
            let path = pool.view(&id)?.path;
            let editor = pool
                .cfg
                .editor
                .clone()
                .or_else(|| std::env::var("VISUAL").ok())
                .or_else(|| std::env::var("EDITOR").ok())
                .ok_or_else(|| anyhow!("set `editor` in ~/.config/velmux/config.toml or $EDITOR"))?;
            let status = Command::new("sh")
                .arg("-c")
                .arg(format!("{editor} \"$1\""))
                .arg("sh")
                .arg(&path)
                .status()
                .context("launching editor")?;
            if !status.success() {
                bail!("editor exited with {status}");
            }
        }
        Cmd::Gc => {
            pool.gc(&report)?;
            print_table(&pool, &pool.status()?);
        }
        Cmd::Server | Cmd::ImportHerdr { .. } | Cmd::Keys { .. } | Cmd::Restart | Cmd::Stop | Cmd::Debug | Cmd::Skill { .. } | Cmd::Doctor | Cmd::Guard | Cmd::AgentHook { .. } | Cmd::Update { .. } => unreachable!(),
    }
    Ok(())
}

fn resolve_slot(pool: &Pool, slot: Option<String>, cwd: &std::path::Path) -> Result<String> {
    match slot {
        Some(s) => Ok(s),
        None => pool
            .slot_of(cwd)?
            .ok_or_else(|| anyhow!("not inside a slot; pass a slot id (see `velmux status`)")),
    }
}

#[derive(clap::Subcommand)]
enum SkillCmd {
    /// Write (or update) it: in your Claude config (every repo), or with
    /// `--project` in this repo's `.claude/skills/` (to commit and share).
    Install {
        #[arg(long)]
        project: bool,
    },
    /// Remove it.
    Uninstall {
        #[arg(long)]
        project: bool,
    },
    /// Print it.
    Show,
}

use velmux_tui::skill as skills;

fn skill_path(claude_dir: &std::path::Path) -> std::path::PathBuf {
    skills::user_path(claude_dir)
}

fn skill(action: SkillCmd, cwd: &std::path::Path) -> Result<()> {
    let target = |project: bool| -> Result<std::path::PathBuf> {
        Ok(if project { skills::project_path(&velmux_pool::git::base_of(cwd)?) } else { skill_path(&Env::from_process()?.claude_dir) })
    };
    match action {
        SkillCmd::Show => print!("{}", skills::SKILL),
        SkillCmd::Install { project } => {
            let path = target(project)?;
            let verb = skills::install(&path)?;
            println!("{verb} {}", path.display());
            // The other agents: instruction files and session hooks.
            if !project {
                let (done, problems) = skills::install_others();
                for line in done {
                    println!("{line}");
                }
                for p in problems {
                    eprintln!("velmux: {p}");
                }
            }
            println!("new Claude sessions pick it up; it's used whenever Claude runs in velmux");
        }
        SkillCmd::Uninstall { project } => {
            let path = target(project)?;
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    let _ = std::fs::remove_dir(path.parent().unwrap());
                    println!("removed {}", path.display());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("not installed at {}", path.display()),
                Err(e) => return Err(e.into()),
            }
            if !project {
                for (agent, file) in skills::agent_files() {
                    let text = std::fs::read_to_string(&file).unwrap_or_default();
                    let new = skills::without_block(&text);
                    if new != text {
                        if new.is_empty() { std::fs::remove_file(&file)? } else { std::fs::write(&file, new)? }
                        println!("removed velmux's instructions for {agent} from {}", file.display());
                    }
                }
                let (changed, problems) = velmux_tui::hooks::sync(false);
                for (agent, file) in changed {
                    println!("removed velmux's session hook for {agent} from {}", file.display());
                }
                for p in problems {
                    eprintln!("velmux: {p}");
                }
            }
        }
    }
    Ok(())
}

/// `velmux update`.
fn update(check: bool) -> Result<()> {
    use velmux_tui::update::{self, Install};
    let latest = update::fetch()?;
    if !update::newer(&latest.version, update::CURRENT) {
        println!("velmux {} is the newest", update::CURRENT);
        return Ok(());
    }
    println!("velmux {} is out (you have {}): {}", latest.version, update::CURRENT, latest.url);
    if check {
        return Ok(());
    }
    match Install::detect() {
        Install::Dev => println!("this velmux is built from source: update it with git pull and cargo build --release"),
        Install::Binary(path) => {
            update::apply(&latest, &path)?;
            println!("updated {} to {}; a running velmux restarts into it by itself", path.display(), latest.version);
        }
        install => {
            let cmd = install.command().unwrap_or_default();
            println!("running: {cmd}");
            let ok = Command::new("sh").arg("-c").arg(cmd).status()?.success();
            if !ok {
                bail!("`{cmd}` failed");
            }
        }
    }
    Ok(())
}

/// `velmux guard`: see [`Cmd::Guard`]. It fails open: if
/// anything is unclear (no checkout set, unreadable input) the edit goes ahead.
fn guard() -> ! {
    use std::io::Read;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let verdict = (|| {
        let base = std::path::PathBuf::from(std::env::var_os("VELMUX_BASE")?);
        let hook: serde_json::Value = serde_json::from_str(&input).ok()?;
        let tool = &hook["tool_input"];
        let file = tool["file_path"].as_str().or(tool["notebook_path"].as_str())?;
        let cwd = hook["cwd"].as_str().map(std::path::PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let path = cwd.join(file);
        // Compare real paths (the file may not exist yet: resolve its folder).
        let real = |p: &std::path::Path| -> std::path::PathBuf {
            p.canonicalize().ok().or_else(|| Some(p.parent()?.canonicalize().ok()?.join(p.file_name()?))).unwrap_or_else(|| p.to_path_buf())
        };
        let (path, base) = (real(&path), real(&base));
        let pool = std::env::var_os("VELMUX_POOL_DIR").map(|p| real(std::path::Path::new(&p)));
        let inside = path.starts_with(&base) && !pool.is_some_and(|p| path.starts_with(p));
        inside.then_some((path, base))
    })();
    match verdict {
        Some((path, base)) => {
            let repo = base.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            eprintln!(
                "velmux: {} is in {repo}'s checkout ({}), which stays clean: changes happen in slots. \
                 Run `velmux claim <short-branch-name>`, then make this edit under the path it prints \
                 (the same file, relative to the slot).",
                path.display(),
                base.display()
            );
            std::process::exit(2)
        }
        None => std::process::exit(0),
    }
}

/// `velmux agent-hook <agent>`: see [`Cmd::AgentHook`]. Never fails the
/// agent: outside velmux, or without a session id, it quietly does nothing.
fn agent_hook(agent: &str) -> ! {
    use std::io::Read;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    if let (Some(agent), Some(pane)) = (velmux_tui::agents::Agent::from_name(agent), velmux_tui::current_pane()) {
        if let Some(id) = velmux_tui::hooks::session_id(agent, &input) {
            let _ = velmux_tui::report_session(pane, agent.session_ref(&id));
        }
    }
    if let Some(agent) = velmux_tui::agents::Agent::from_name(agent) {
        print!("{}", velmux_tui::hooks::hook_output(agent));
    }
    std::process::exit(0)
}

/// `velmux doctor`: each check as `✓` / `✗` (with how to fix it) / `·` (optional).
fn doctor(cwd: &std::path::Path) -> Result<()> {
    let mut failed = 0;
    let mut line = |ok: Option<bool>, what: &str, fix: &str| {
        let mark = match ok {
            Some(true) => "✓",
            Some(false) => {
                failed += 1;
                "✗"
            }
            None => "·",
        };
        println!("{mark} {what}");
        if !fix.is_empty() && ok != Some(true) {
            println!("    {fix}");
        }
    };
    let run = |cmd: &str, args: &[&str]| Command::new(cmd).args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let on_path = |prog: &str| prog.contains('/') && std::path::Path::new(prog).exists() || std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).any(|d| d.join(prog).is_file());

    let env = Env::from_process()?;
    let base = velmux_pool::git::base_of(cwd).ok();
    match velmux_pool::Config::load(&env, base.as_deref().unwrap_or(cwd)) {
        Ok(cfg) => {
            line(Some(true), &format!("config ({})", env.global_config.display()), "");
            match run("git", &["--version"]) {
                Some(v) => line(Some(true), &v, ""),
                None => line(Some(false), "git not found", "install git"),
            }
            let agent = cfg.agent.split_whitespace().next().unwrap_or("claude").to_string();
            if on_path(&agent) {
                line(Some(true), &format!("agent `{agent}` is on PATH"), "");
            } else {
                line(Some(false), &format!("agent `{agent}` isn't on PATH"), "install Claude Code (https://claude.com/claude-code), or set `agent` in the config");
            }
        }
        Err(e) => line(Some(false), &format!("config doesn't parse: {e:#}"), "fix it, or run `velmux` and press <prefix> s to edit it"),
    }
    if skills::installed(&env.claude_dir, base.as_deref()) {
        line(Some(true), "Claude skill installed", "");
    } else {
        line(Some(false), "Claude skill not installed", "velmux skill install (teaches Claude to claim and release slots)");
    }
    let unhooked = velmux_tui::hooks::missing();
    if !unhooked.is_empty() {
        line(None, &format!("{} won't be resumed after a restart (no session hook)", unhooked.join(", ")), "velmux skill install");
    }
    match velmux_tui::update::fetch() {
        Ok(l) if velmux_tui::update::newer(&l.version, velmux_tui::update::CURRENT) => {
            line(None, &format!("velmux {} (version {} is out)", velmux_tui::update::CURRENT, l.version), "velmux update")
        }
        Ok(_) => line(Some(true), &format!("velmux {} is the newest", velmux_tui::update::CURRENT), ""),
        Err(_) => line(None, &format!("velmux {} (couldn't check for updates)", velmux_tui::update::CURRENT), ""),
    }
    // Keeping velmux-started Claudes out of the checkout: a hook, and a deny rule.
    let guard_on = velmux_pool::Config::load(&env, base.as_deref().unwrap_or(cwd)).map(|c| c.guard).unwrap_or(true);
    let policy = velmux_tui::guard::policy(&env.claude_dir);
    match (guard_on, &policy.hooks_off, &policy.rules_off) {
        (false, _, _) => line(None, "checkout guard is off (guard = false): agents are only asked to work in slots", ""),
        (true, None, None) => line(Some(true), "checkout guard: Claudes velmux starts can't edit a repo's checkout", ""),
        (true, Some(h), None) => line(None, &format!("checkout guard: its hook is off ({h}); the deny rule still holds"), ""),
        (true, None, Some(r)) => line(None, &format!("checkout guard: its deny rule is ignored ({r}); the hook still holds"), ""),
        (true, Some(h), Some(r)) => line(
            Some(false),
            &format!("checkout guard can't work here ({h}; {r})"),
            "agents are still asked (prompt, skill) to work in slots, and the sidebar flags changes in a checkout (✎)",
        ),
    }
    if !velmux_pool::gh::available() {
        line(None, "gh not installed (optional: pull request status)", "brew install gh && gh auth login");
    } else if run("gh", &["auth", "status"]).is_some() {
        line(Some(true), "gh signed in", "");
    } else {
        line(None, "gh isn't signed in (optional: pull request status)", "gh auth login");
    }
    match velmux_tui::terminal_check() {
        (true, true) => line(Some(true), "Ghostty sends velmux's ⌘ shortcuts", ""),
        (true, false) => line(None, "Ghostty isn't set up for the ⌘ shortcuts (optional)", "velmux keys --ghostty --install"),
        (false, _) => line(None, "not in Ghostty: ⌘ shortcuts need your terminal to send them (optional)", "velmux keys --test shows what it sends; the prefix keys always work"),
    }
    if let Some(base) = &base {
        match Pool::discover(env.clone(), base).and_then(|p| p.status().map(|s| (p, s))) {
            Ok((pool, slots)) if slots.is_empty() => line(None, &format!("{}: no slots yet (made on the first claim)", pool.name), "velmux init"),
            Ok((pool, slots)) => {
                let missing: Vec<&str> = slots.iter().filter(|s| !s.path.exists()).map(|s| s.id.as_str()).collect();
                if missing.is_empty() {
                    let at = if slots.iter().all(|s| s.path.starts_with(&pool.dir)) {
                        pool.dir.display().to_string()
                    } else {
                        format!("{}-pool-N, beside the repo", pool.base.display())
                    };
                    line(Some(true), &format!("{}: {} slots in {at}", pool.name, slots.len()), "");
                } else {
                    line(Some(false), &format!("{}: missing slots {}", pool.name, missing.join(", ")), "velmux gc");
                }
            }
            Err(e) => line(Some(false), &format!("this repo's pool: {e:#}"), ""),
        }
    }
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-:".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn print_parked(parked: &[ParkedView]) {
    if parked.is_empty() {
        return;
    }
    println!("parked (resume with `velmux claim <branch>`)");
    let now = velmux_pool::state::now();
    for p in parked {
        let age = match p.age(now).as_str() {
            "now" => "just now".to_string(),
            a => format!("{a} ago"),
        };
        println!("  {:<28} {:<20} {age}, from {}", p.branch, p.what(), p.slot);
    }
}

fn print_table(pool: &Pool, slots: &[SlotView]) {
    println!("{}  ({}, base ref {})", pool.name, pool.base.display(), pool.base_ref());
    for s in slots {
        let state = match s.status {
            Status::Free => "free  ",
            Status::InUse => "in-use",
        };
        let branch = s.branch.as_deref().unwrap_or("-");
        let mut dirty = if s.dirty { " *".to_string() } else { String::new() };
        if s.behind > 0 {
            dirty.push_str(&format!(" ↓{}", s.behind));
        }
        if s.ahead > 0 {
            dirty.push_str(&format!(" ↑{}", s.ahead));
        }
        if let Some(op) = &s.conflict {
            dirty.push_str(&format!(" !{op} in progress"));
        }
        let by = s.claimed_by.as_deref().map(|b| format!("  [{b}]")).unwrap_or_default();
        let port = pool.slot_port(&s.id).map(|p| format!("  :{p}")).unwrap_or_default();
        println!("  {:<8} {state}  {branch}{dirty}{by}{port}", s.id);
    }
}
