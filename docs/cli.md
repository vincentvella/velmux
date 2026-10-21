# Command line

Everything the sidebar does to slots is also a command, for agents, scripts and
other tools. Commands act on the repo you're in (or `-C <path>`).

```sh
velmux                           # open velmux (starting its server)

# Slots
velmux init                      # create the pool's slots (idempotent)
velmux claim [branch]            # claim a free slot and print its path
cd "$(velmux claim feat/login)"
velmux release [slot]            # park the work, reset the slot, keep its dependencies
velmux status [--json]           # every slot: branch, dirty, ↓ behind / ↑ ahead, port; parked work
velmux sync [slot]               # fetch, then rebase (or merge) onto the base ref
velmux env [slot]                # the slot's ports etc. as exports: eval "$(velmux env)"
velmux path <slot>               # a slot's path
velmux open [slot]               # open it in your editor
velmux gc                        # prune dead worktrees, recreate missing slots

# Parked work
velmux parked [--json]           # branches released with work on them
velmux discard <branch>          # drop a parked branch's stashed changes (the branch is kept)

# The server
velmux restart                   # restart in place: panes keep running
velmux stop                      # stop it: everything comes back next time
velmux update [--check]          # update velmux the way it was installed (or just check)

# Setup
velmux doctor                    # check git, your agent, the skill, gh, your terminal, this repo's pool
velmux skill install [--project] # the Claude Code skill (rerun to update; skill uninstall)
velmux keys [--ghostty | --test] # key bindings / Ghostty config lines / what your terminal sends
velmux keys --ghostty --install  # write the Ghostty lines into its config
velmux import-herdr              # bring over a herdr session
```

[slot] defaults to the slot you're in.

Inside a velmux pane, `velmux claim` and `velmux release` go through the server,
so the pane running them (usually a Claude) moves into the slot and back.

`claim` takes `--no-fetch`, `--no-install`, `--by <name>` and `--json`.
