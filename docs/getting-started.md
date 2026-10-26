# Getting started

## Install

velmux is a single binary for macOS, Linux and [Windows](#windows). You need git.

```sh
brew install vincentvella/tap/velmux
```

Or download it for your platform from the
[releases](https://github.com/vincentvella/velmux/releases) (Apple Silicon and
Intel Macs; static x86_64 and arm64 Linux builds) and put `velmux` on your
`PATH`. Or build it with Rust:
`cargo install --git https://github.com/vincentvella/velmux velmux-cli`.

A running velmux restarts into a new version by itself when you upgrade.

Optional, but worth it:

- **[Claude Code](https://claude.com/claude-code)** (or another agent CLI: set
  `agent` in the [config](configuration.md)).
- **The GitHub CLI (`gh`)**, signed in, for [pull request status](slots.md#pull-requests).
- **[Ghostty](https://ghostty.org)** (or another terminal you can bind keys in)
  for the prefix-less ⌘ shortcuts.

### Windows

velmux runs on Windows 10 (1803 or later) and 11, in Windows Terminal or any
terminal that speaks VT sequences. In PowerShell:

```powershell
irm https://velmux.com/install.ps1 | iex
```

That checks the download against its checksum, puts `velmux.exe` in
`~\.local\bin` and adds it to your `PATH`. Or build it with Rust
(`cargo install`, above), or download `velmux-x86_64-pc-windows-msvc.tar.gz`
from the releases yourself. You need [Git for Windows](https://git-scm.com).

What's different there:

- **Panes run PowerShell**: PowerShell 7 (`pwsh`) when it's installed, else
  Windows PowerShell.
- **Repo hooks still run in `sh`**: `install_cmd`, `post_claim` and the rest
  run in the `sh` that comes with Git for Windows, so a repo's `.velmux.toml`
  works the same for everyone working on it.
- **Restarts start panes again.** Windows can't hand a running pane to a new
  process, so `velmux restart` saves everything, stops the panes and brings
  them back, with agents resuming their conversations. For the same reason a
  new velmux binary doesn't restart velmux by itself: it tells you, and you run
  `velmux restart` when it suits you.
- **A running `velmux.exe` can't be replaced.** The server runs from a copy of
  itself so it doesn't hold the file, but an attached client does: detach
  (`ctrl+b d`) before rebuilding or reinstalling velmux.
- **Links**: without Developer Mode, Windows won't make symlinks, so a slot's
  shared `.env` files are hard links and Claude's grouped transcripts a
  junction.
- `velmux env` prints PowerShell: `velmux env | Invoke-Expression`.
- There are no ⌘ shortcuts: the prefix keys (`ctrl+b …`) do everything.

The config and state live where they do elsewhere, under your user folder
(`~\.config\velmux\config.toml`, `~\.local\state\velmux`).

## Teach Claude about velmux

```sh
velmux skill install
```

This writes a Claude Code skill to `~/.claude/skills/velmux/` telling any Claude
how to work here: to plan in the repo's own checkout, `velmux claim` a slot
before changing code, `velmux release` it when done, and how parked work,
syncing, ports and PRs work. `--project` puts it in the repo's
`.claude/skills/` instead, to commit and share. Rerun it after upgrading velmux.

## First run

```sh
cd ~/Workspace/your-repo
velmux
```

This opens velmux (starting its background server) with your repo in the
sidebar. (Run it anywhere else and you get a welcome screen that adds one.)
Until you're set up, the empty screens list what's left (installing the Claude
skill, Ghostty's ⌘ keys), a key each, and `velmux doctor` checks everything. Its pool of slots is created the first time one is claimed (or run
`velmux init`).

The prefix key is `ctrl+b` (set `prefix = "C-a"` or anything else in the
[config](configuration.md)). Press it and a popup shows every key you can press
next.

## The loop

1. **Start a conversation in the repo:** `ctrl+b a` (or right-click the repo).
   Claude reads, explores and plans in your checkout without touching it.
2. **Claude claims a slot** when it's ready to change code: it runs
   `velmux claim feat/short-name`, gets a warm worktree on that branch, and its
   pane moves under that slot in the sidebar.
3. **It works there**: builds, tests and commits in the slot, with its own
   `$PORT` for dev servers. You see its state in the sidebar and get a
   notification when it needs you or finishes.
4. **Hand it back** with `velmux release` (Claude does this itself, or you do
   from the slot's right-click menu). Uncommitted work is parked, the slot is
   reset with its dependencies kept, and the conversation returns to your repo.

Run several of these at once: that's the point.

To claim a slot yourself, use `ctrl+b n` (it asks for a branch) or the repo's
right-click menu.

## ⌘ shortcuts

velmux can also take shortcuts with no prefix: `⌘N` new conversation, `⌘P` go
to any place, `⌘1`–`9` places, `⌃1`–`9` tabs, `⌘⇧F` find a conversation, and
more. Your terminal has to be told to send them:

```sh
velmux keys --ghostty --install
```

(or press `g` on velmux's welcome screen), then reload Ghostty's config (`⌘⇧,`). `velmux keys` lists every binding;
[Using velmux](using-velmux.md#keys) has the full table.

## Next

- [Working with velmux](workflow.md): a day with several agents going, from
  starting work to putting it away
- [Using velmux](using-velmux.md): the sidebar, tabs, keys, mouse, copying, themes
- [Working in slots](slots.md): parked work, syncing, ports, pull requests
