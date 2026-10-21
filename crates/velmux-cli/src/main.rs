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
        Some(Cmd::ImportHerdr { file, session, with_latest_claude, dry_run }) => {
            return velmux_tui::import_herdr(file, session.as_deref(), with_latest_claude, dry_run)
        }
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
        // Inside a velmux pane: let the server claim, so this pane moves to the slot.
        Cmd::Claim { branch, json: false, .. } if velmux_tui::current_pane().is_some() => {
            let path = velmux_tui::claim_for_pane(velmux_tui::current_pane().unwrap(), branch)?;
            eprintln!("claimed a slot; work in it from here (this pane now lives there in velmux)");
            println!("{path}");
        }
        Cmd::Release { slot: None, force: false } if velmux_tui::current_pane().is_some() => {
            eprintln!("{}", velmux_tui::release_for_pane(velmux_tui::current_pane().unwrap())?);
        }
        Cmd::Claim { branch, by, no_fetch, no_install, json } => {
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
        Cmd::Server | Cmd::ImportHerdr { .. } | Cmd::Keys { .. } | Cmd::Restart | Cmd::Stop | Cmd::Debug | Cmd::Skill { .. } | Cmd::Doctor => unreachable!(),
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
        }
    }
    Ok(())
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
                    line(Some(true), &format!("{}: {} slots in {}", pool.name, slots.len(), pool.dir.display()), "");
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
