use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use velmux_pool::{ClaimOpts, Env, Pool, SlotView, Status};

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
    /// Show every slot.
    Status {
        #[arg(long)]
        json: bool,
    },
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
    /// Show key bindings; `--ghostty` prints the Ghostty keybinds for the root ones.
    Keys {
        #[arg(long)]
        ghostty: bool,
        /// Show what each key press sends and which binding it hits.
        #[arg(long, conflicts_with = "ghostty")]
        test: bool,
    },
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
        Some(Cmd::Keys { test: true, .. }) => return velmux_tui::key_test(),
        Some(Cmd::Keys { ghostty, .. }) => return velmux_tui::print_keys(ghostty),
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
            }
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
        Cmd::Server | Cmd::ImportHerdr { .. } | Cmd::Keys { .. } | Cmd::Restart => unreachable!(),
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

fn print_table(pool: &Pool, slots: &[SlotView]) {
    println!("{}  ({}, base ref {})", pool.name, pool.base.display(), pool.base_ref());
    for s in slots {
        let state = match s.status {
            Status::Free => "free  ",
            Status::InUse => "in-use",
        };
        let branch = s.branch.as_deref().unwrap_or("-");
        let dirty = if s.dirty { " *" } else { "" };
        let by = s.claimed_by.as_deref().map(|b| format!("  [{b}]")).unwrap_or_default();
        println!("  {:<8} {state}  {branch}{dirty}{by}", s.id);
    }
}
