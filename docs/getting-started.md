# Getting started

## Install

velmux is a single binary for macOS and Linux. You need git.

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
sidebar. Its pool of slots is created the first time one is claimed (or run
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
velmux keys --ghostty >> ~/.config/ghostty/config
```

Then reload Ghostty. `velmux keys` lists every binding;
[Using velmux](using-velmux.md#keys) has the full table.

## Next

- [Using velmux](using-velmux.md): the sidebar, tabs, keys, mouse, copying, themes
- [Working in slots](slots.md): parked work, syncing, ports, pull requests
