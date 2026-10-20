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
pane, so `<prefix> d` detaches and `velmux` reattaches with everything still
running. The sidebar lists the base checkout and each slot, one row each, with
the state of their agents on the right: `●` working, `▲` blocked on you, `○`
idle, `✓` finished while you were elsewhere. Each base checkout and slot has its own tabs, each a split
layout, so a slot can show Claude beside a test-runner shell. Only the project
you're in is expanded; others collapse to one line with an agent summary
(e.g. `▸ caliburr ▲1 ○3`). Panes are labelled by what's running and where
(`shell · packages/web`, `bun`, `nvim`).

The prefix defaults to `ctrl+b`; set `prefix` in the global config (e.g.
`"C-a"`). Config changes apply without a restart: `<prefix> s` opens the config
in your editor and reloads it when you quit, and `<prefix> R` reloads it.

| `<prefix>` then | |
|---|---|
| `n` / `r` | claim a slot (prompts for a branch) / release it |
| `a` | Claude in the base checkout |
| `c` / `e` | shell / editor here (splits the focused pane) |
| `\|` / `-` | split right / down with a shell |
| arrows, `H J K L` | focus a neighbour, move the nearest divider |
| `z` | zoom the focused pane |
| `t`, `tab` / `shift+tab` | new tab, next / previous tab |
| `T` | rename the tab (empty clears; names show in the tab bar) |
| `w` | location picker: type to fuzzy-filter, ↑↓ / `^p ^n`, enter to go |
| `W` | name this location (base, slot or folder); empty clears |
| `s` / `R` | settings: edit the config in a tab, applied when you quit the editor / reload config now |
| `alt+1`–`9`, `,` / `.` | go to location N (those with panes), previous / next |
| `1`–`9`, `0` | slot N of this repo (folder N when unclassified), its base |
| `o` | jump to the next agent that's blocked or finished unseen |
| `b` | hide / show the sidebar |
| `[` / `PgUp` | copy mode |
| `j` / `k`, `x` | cycle panes, close pane |
| `d` / `Q` | detach / stop the server |
| `<prefix>` | send the prefix key itself |

### Prefix-less keys

Any action can also be bound with no prefix. Two levels of jumping: `super+1..9`
goes to location N across every space, and `alt+1..9` / `alt+0` swap between
the current space's pool slots and its base (folders, when unclassified). The
other defaults mirror a Cmd-driven (herdr / cmux style) layout: `ctrl+1..9` tabs, `super+t`
new tab, `super+shift+[` / `]` tabs, `super+ctrl+[` / `]` locations, `super+n` /
`super+shift+w` claim / release, `super+d` / `super+shift+d` split, `super+w`
close, `super+shift+enter` zoom, `super+alt+arrows` focus, `super+b` sidebar,
`super+shift+u` attention, `super+p` location picker, `super+r` rename tab,
`super+shift+r` name the location, `super+,` settings. These use CSI-u sequences that apps never receive, so
they can't clash with app keys, but your terminal must be told to send them:

```sh
velmux keys            # every binding
velmux keys --ghostty  # Ghostty keybind lines for the prefix-less ones
```

Rebind anything in the global config (an empty list unbinds):

```toml
[keys]
split_right = ["prefix+v", "super+d"]
location = "alt+1..9"     # 1..9 ranges for location / tab / slot
new_tab = []
```

### Mouse

On by default (`mouse = false` in the global config turns it off). Click a pane
to focus it, or a sidebar row to jump there; drag a divider to resize; drag over
text to select it and copy on release. The wheel scrolls a pane's history
(opening copy mode, which closes again at the bottom), sends arrow keys to
full-screen apps such as `less`, and is passed straight through, along with
clicks, to apps that ask for the mouse (vim with `mouse=a`, full-screen TUIs).
Hold Shift (Option in iTerm2) for your terminal's native selection.

### Copy mode

`<prefix> [` (or `<prefix> PgUp`) freezes the focused pane and opens its
history (5000 lines). vi keys move (`hjkl`, `^u ^d ^b ^f`, `g G`, `0 $`), `v` /
`V` select by character / line, `y` or Enter copies and exits, `/` and `?` search
down / up (smartcase), `n N` repeat, `q` or Esc exits. Copies go to the system
clipboard (`pbcopy`, `wl-copy` or `xclip`) and to your terminal via OSC 52, so
they work over SSH too. Output is held while you read and applied on exit.

Full-screen apps (vim, or Claude with `"tui": "fullscreen"`) draw on the
alternate screen, which has no history; copy mode can still copy what's visible.

### Restore

The server saves projects, layouts and panes as they change
(`~/.local/state/velmux/session.json`). After a restart (`<prefix> Q`, an
upgrade, a crash, a reboot) `velmux` brings them back: shells and editors
reopen, and Claude panes run `claude --resume` **inside the slot they ran in**.
velmux launches Claude with `--session-id`, so no hooks are needed; if you
`/clear` mid-pane, restore picks the newest transcript recorded in that slot.
Slots released in the meantime are skipped.

### Import from herdr

```sh
velmux import-herdr --dry-run      # show what would come over
velmux import-herdr                # ~/.config/herdr/session.json
velmux import-herdr --session work # a named herdr session
velmux import-herdr --with-latest-claude  # also pick up Claude history (below)
```

Each herdr workspace inside a git repo becomes that repo's base view (or slot,
if it lives in one), with its active tab's split layout. Workspaces outside any
repo land in an **unclassified** group at the bottom of the sidebar, one entry
per folder (no slots there). Panes keep their working directory; agent panes
come back as agents, and Claude resumes the session herdr recorded (or starts
fresh, noted in the summary, if that transcript is gone). Extra tabs and
locations velmux already has panes in are skipped and listed.

herdr only records a Claude session for panes that were running Claude when it
saved. `--with-latest-claude` also pulls Claude history: for each imported
workspace without a Claude pane, it adds one resuming the newest conversation
that ran anywhere in that repo (subfolders included), split beside the
imported layout. Unclassified folders only take conversations held in exactly
that folder (so `~` doesn't claim everything), and conversations written to in
the last two minutes are left alone since they're probably open. `--dry-run`
shows each one's first prompt and age.

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
prefix = "C-b"                           # TUI prefix: C-a, ctrl+space, ^], …
mouse = true                             # click / drag / wheel in the TUI
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

## License

MIT. Agent-state detection rules are adapted from
[herdr](https://github.com/herdrdev/herdr) (Apache-2.0), used as a reference.
