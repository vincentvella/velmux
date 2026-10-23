# How it works

## Pools and slots

A repo's slots are ordinary `git worktree`s of its checkout, kept in
`<pool_root>/<repo>/slot-N`, outside the repo so its watchers, bundlers and type
checker never see them. Their state (free or claimed, branch, by whom, the
lockfile hash of their last install, parked work) lives in `pool.json` next to
them, guarded by a file lock, so velmux, the command line and agents can all
claim at once without racing.

**Release** stashes uncommitted work under a `velmux[<branch>]` marker (stashes
are shared by all of a repo's worktrees, so the branch can be resumed in any
slot), detaches the slot to the base ref, and runs `git clean` with dependency
and cache folders excluded. Claiming that branch again pops its stash.

## Staying small

Each slot has a real `node_modules`, deduplicated by the package manager itself:
bun and pnpm clone or hardlink from their global store, yarn berry uses
`nmMode: hardlinks-global`, and npm or yarn classic are seeded with an APFS
copy-on-write clone of your checkout's tree. Installs are skipped while the
lockfile is unchanged. Measured: five slots of a 917 MB bun repo cost about
89 MB of real disk. (Measure with `df`: `du` counts clones as full copies.)

## Claude's history

Claude Code keys its memory to the repo but its transcripts to the folder it
runs in. velmux links each slot's transcript folder to the checkout's, so every
slot's conversations live under the repo, and resumes a conversation inside the
slot it ran in (resuming adopts the current folder). Each pane's exact session
comes from Claude's own record of its running processes, never from guessing at
the newest transcript.

## The server

A background server owns every pane: their pseudo-terminals, a terminal
emulator for each, and the rendering, which goes to a thin client over a Unix
socket as screen diffs. That's why you can close your terminal and come back.

A **live restart** execs the new binary with every pane's pseudo-terminal left
open, handing over each pane's screen, scrollback and the window size, so
nothing on the other end notices. Agent state (working, blocked, idle) is read
from each pane's foreground process and the bottom of its screen.

## Keeping up

The sidebar's slot states (branch, uncommitted changes, behind / ahead the
base) come from git. Checking a slot in a big repo takes a few hundred
milliseconds, so velmux checks the repo you're in (and, in a workstream, its
repos) and any repo with an agent working every 2 seconds, and the rest every 30.
Fetches run on their own schedule (`fetch_every`), pull requests about once a
minute.

## Code

- `crates/velmux-pool`: pools, slots, git, dependencies, parked work, PRs (via `gh`), Claude linking
- `crates/velmux-tui`: the server, panes, rendering, and the client
- `crates/velmux-cli`: the `velmux` binary
