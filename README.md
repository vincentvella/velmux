# velmux

**A terminal workspace for running many coding agents at once — on warm git
worktrees that never go cold.**

![velmux: a workstream across two repos, claimed slots (a Claude working, one blocked on a permission prompt, one idle), PR status, parked work and a scratch session](docs/screenshots/overview.svg)

Parallel agents need parallel checkouts. The usual answer, a fresh worktree per
task, pays for a cold `install` every time and leaves a trail of abandoned
folders. velmux gives each repo a small **pool of worktree slots** that are
claimed for a task and handed back when it's done, with `node_modules`, build
caches and all kept warm. Claiming one takes about a second.

```
~/Workspace/app                  ← your checkout: where agents plan
~/Workspace/.pools/app/slot-1…5  ← warm worktrees: where they work
```

## What you get

- **Agents that manage their own worktrees.** Start a Claude in your repo; it
  plans there, claims a slot on its own branch when it's ready to change code,
  and hands it back when it's done.
- **Workstreams across repos.** Group slots from several repos (an API change
  and the app that uses it) as one piece of work, with a place where one agent
  reaches every one of them, and release it all together when it's done.
- **Every agent at a glance.** One sidebar for every repo, slot and session, with
  each agent's state: working, blocked on you, finished. Desktop notifications
  (and a sound) when one needs you.
- **Nothing lost.** Hand a slot back mid-task and its work is parked, ready to
  resume, Claude conversation included. Quit, crash or reboot and every pane
  comes back, scrollback and all.
- **Slots that stay healthy.** See which are behind your main branch, sync them
  in one step, and give each its own ports so dev servers never collide.
- **Pull requests in the loop.** PR and CI status on every slot; open, check, and
  hand failing checks straight to the slot's Claude.
- **Find any conversation.** Search every Claude session across all your repos
  and jump back into it.
- **A proper multiplexer.** Tabs, splits, copy mode, mouse, themes, prefix-less
  ⌘ shortcuts, and live restarts that keep everything running.

| | |
|---|---|
| ![every key, one press away](docs/screenshots/prefix.svg) | ![a slot's right-click menu: sync, PR, release](docs/screenshots/slot-menu.svg) |
| ![search every conversation](docs/screenshots/find.svg) | ![themes, previewed live](docs/screenshots/theme.svg) |

## Quickstart

```sh
brew install vincentvella/tap/velmux
velmux skill install      # teach Claude to work in velmux
cd ~/Workspace/your-repo
velmux                    # open it
```

Then press `ctrl+b a` to start a Claude in your repo, and ask it for something.
[Getting started](docs/getting-started.md) covers the rest, including the
⌘ shortcuts for Ghostty.

Runs on macOS and Linux. Works with any agent CLI: it knows Claude Code, Codex,
Gemini CLI and opencode well, and follows and resumes 15 more (Copilot, Cursor
Agent, Droid, Qwen Code, Kimi, pi…). See [Agents](docs/agents.md).

## Documentation

- [Getting started](docs/getting-started.md)
- [Using velmux](docs/using-velmux.md): the sidebar, tabs, keys, mouse, copying, themes
- [Working in slots](docs/slots.md): claiming, parked work, syncing, ports, pull requests
- [Workstreams](docs/workstreams.md): slots from several repos, worked on together
- [Agents](docs/agents.md): Claude Code, Codex, Gemini CLI, opencode
- [Configuration](docs/configuration.md)
- [Command line](docs/cli.md)
- [How it works](docs/how-it-works.md)
- [Changelog](CHANGELOG.md)
- [Contributing](CONTRIBUTING.md)

## License

MIT. Agent-state detection rules and two notification sounds come from
[herdr](https://github.com/herdrdev/herdr) (Apache-2.0); other sounds from
[Kenney](https://kenney.nl) (CC0). See [NOTICE](NOTICE).
