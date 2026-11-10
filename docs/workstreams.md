# Workstreams

Some work spans repos: an API change and the app that uses it, a library and
the services that depend on it. A **workstream** groups slots from one or more
repos as one piece of work, with a place of its own where its one Claude works
across all of them.

```
WORKSTREAMS                +
  ◇ checkout
  ▣ feat/saved-cards       ●
    @2 webapp · #142✓
  ▣ feat/saved-cards
    @1 api · ↓3
```

A workstream's slots are listed under it rather than under their repos (the
repos keep their checkout, free slots and parked work). Its own row sums up
their pull requests: `3 PR ✗1 …1 ⇡1` (how many, failing checks, checks still
running, merged), or `✓` when every open one passes; its menu's **open all
PRs** opens each in your browser; **sync all slots** brings every slot that's
behind its base up to date, and **push & open PRs** pushes every slot that has
no PR yet and opens one for each. Once every slot's PR has merged, it says so
(a notification) and its row shows **⏏ release**: a click hands it all back.

## Its place, and its one Claude

A workstream has **one Claude**, started when you make it. Asking for another
there (or in one of its slots) takes you to that one. Shells, dev servers and
editors open there as anywhere else. A slot that joins with a Claude of its
own has that conversation parked with it; it comes back if the slot leaves the
workstream, and goes with the branch if the slot is released.

Its folder (`.pools/.persistent/ws-<id>/`, next to the repos' pools; the id is
random, so a later workstream of the same name never shares its conversation)
holds a link to each of its slots, named after the repo (`webapp/`, `api/`;
`webapp-2/` for a second slot from the same repo), so its Claude sees every
repo as a subfolder, and has each slot in reach. velmux also writes a
`CLAUDE.md` there naming the slots and importing each repo's own `CLAUDE.md`,
so their instructions load from the start. Panes there get
`$VELMUX_WORKSTREAM`.

## Making one, adding slots

- The `+` on the **Workstreams** heading starts an empty one (it asks for a name).
- In a workstream, **claim a slot in a repo…** (its right-click menu, or `n` on
  its empty screen) picks a repo, asks for a branch, and claims a slot there
  into it.
- Right-click a slot: **add to workstream…** picks one, or **new workstream…**
  starts one with it.
- Drag a slot onto a workstream (its row or one of its slots); drag it back onto
  the repos, or right-click it for **leave workstream**, to take it out.
- From a workstream's place, an agent (or you) brings in another repo with
  `velmux claim <branch> --repo <repo>` (the repo's name in velmux, or its path).
  The slot is linked in the folder and the pane stays put. From a repo's
  checkout, `velmux claim <branch> --workstream <name>` claims a slot into a
  workstream (making it if needed) instead of moving there.

## From the command line

`velmux workstream` shows the one you're in (or `velmux workstream <name>`, or
all of them): each slot's repo, slot, branch, path, and how it stands (`*`
uncommitted changes, `↓` behind / `↑` ahead its base). `--json` for scripts and
agents.

## Done

Right-click the workstream: **release workstream…** hands back every slot in
it (unfinished work is parked, as a release does) and removes it. Panes open in
its slots move back to their repo's checkout, still running (their
conversations are kept with the parked work too), and the workstream's own
panes (the conversation that worked across the repos) move to its first repo's
checkout. Released with work still unfinished, it's kept as one, folder and
all: `◌ checkout   2 branches · 3h`
under Workstreams (its branches aren't listed under their repos meanwhile).
Click it for **resume workstream**: it's made again in the same folder, every
branch still parked is claimed back into it with its work and conversations,
and its Claude resumes its conversation there. **forget it** drops it and leaves
the branches parked in their repos. Its folder goes once nothing is working in
it and it isn't kept to resume. **remove
workstream** only ungroups: its slots go back to their repos, still claimed.
A slot released on its own leaves its workstream.
