# velmux

Worktree **pools** for parallel agent work. Each repo's base checkout (the
"space") owns a fixed set of warm worktree slots — claimed for a workstream,
released back clean, never thrown away. Dependencies and build caches survive
release, so a claim is sub-second instead of a cold install.

```
~/Workspace/app                 ← base checkout (space)
~/Workspace/.pools/app/slot-1…5 ← pooled worktrees
```

## TUI

Run `velmux` with no arguments inside a repo. A background server owns every
pane, so `ctrl+b d` detaches and `velmux` reattaches with everything still
running. The sidebar nests panes under the base checkout and each slot, with
agent state: `●` working, `▲` blocked on you, `○` idle, `✓` finished while you
were elsewhere. Each base checkout and slot has its own split layout, like a
tmux window per slot, so a slot can show Claude beside a test-runner shell.

| `ctrl+b` then | |
|---|---|
| `n` | claim a slot (prompts for a branch) and start Claude there |
| `a` | Claude in the base checkout |
| `c` / `e` | shell / editor in the current base or slot (splits the focused pane) |
| `\|` / `-` | split right / down with a shell |
| arrows | focus the pane in that direction |
| `H` `J` `K` `L` | move the nearest divider |
| `z` | zoom the focused pane (toggle) |
| `j` `k` / `1`–`9` | next / previous pane across all slots, jump to slot N |
| `r` | release the current slot (closes its panes, stashes work) |
| `x` / `d` / `Q` | close pane / detach / stop the server |
| `ctrl+b` | send a literal ctrl+b |

## CLI

```sh
velmux init                     # create slots (idempotent, grows to config)
cd "$(velmux claim feat/login)" # claim LRU free slot, check out branch
velmux status [--json]
velmux release [slot]           # stash if dirty → detach to base ref → clean, deps kept
velmux open [slot]              # editor from config / $VISUAL / $EDITOR
velmux gc                       # prune dead worktrees, recreate missing slots
```

Reclaiming a branch restores the work parked when it was released, in
whichever slot it lands. Detached commits are kept on a `velmux/rescue/*`
branch.

## How it stays small

Each slot has a real `node_modules`, deduped by the package manager itself:
bun / pnpm clone or hardlink from their global store, yarn berry uses
`nmMode: hardlinks-global`, and npm / yarn classic are seeded with an APFS
copy-on-write clone of base's tree. Installs are skipped while the lockfile is
unchanged. Measured: five slots of a 917 MB bun repo cost ~89 MB of real disk
(use `df`; `du` counts clones as full copies).

## Claude history

Claude Code keys memory to the git common dir but transcripts to cwd. velmux
symlinks each slot's `~/.claude/projects/<slot>` to base's, so every slot's
sessions live under the base project. Resume a session inside the slot it ran
in — resuming adopts the current cwd.

## Config

`~/.config/velmux/config.toml` (or `$VELMUX_CONFIG`):

```toml
slots = 5
pool_root = "/Users/me/Workspace/.pools" # default: <base parent>/.pools
editor = "code"
agent = "claude"                         # command for agent panes
claude_history = true
```

`<repo>/.velmux.toml`:

```toml
slots = 3
base_ref = "origin/main"            # default: origin/HEAD, else base's branch
keep = ["node_modules", "ios/Pods"] # survives release (sensible defaults)
link = [".env.local"]               # shared from base (default: gitignored .env*)
post_claim = ["bun run codegen"]
install = true
install_cmd = "bun install"         # override detection
```

## Layout

- `crates/velmux-pool` — library: state (`pool.json` under `flock`), git ops, deps, Claude linking
- `crates/velmux-cli` — the `velmux` binary
- `crates/velmux-tui` — server (ptys, vt100, ratatui render) + thin client over a Unix socket
