# Workstreams

Some work spans repos: an API change and the app that uses it, a library and
the services that depend on it. A **workstream** groups slots from one or more
repos as one piece of work, with a place of its own where one agent can work
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
PRs** opens each in your browser. Once every slot's PR has merged, it says so
(a notification) and its row shows **⏏ release**: a click hands it all back.

## Its place

A workstream is a place like any other: open panes there, start a Claude there.
Its folder (`.pools/.workstreams/<name>/`, next to the repos' pools) holds a link
to each of its slots, named after the repo (`webapp/`, `api/`; `webapp-2/` for a
second slot from the same repo), so an agent started there sees every repo as a
subfolder, and has each slot in reach. It's told it's in a workstream: change
code in those subfolders and commit in each repo's own git. Panes there get
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

## Done

Right-click the workstream: **release workstream…** hands back every slot in
it (unfinished work is parked, as a release does) and removes it. Panes open in
its slots move back to their repo's checkout, still running (their
conversations are kept with the parked work too), and the workstream's own
panes (the conversation that worked across the repos) move to its first repo's
checkout. Its folder goes once nothing is working in it any more. Released
with work still unfinished, it's kept as one: `◌ checkout   2 branches · 3h`
under Workstreams (its branches aren't listed under their repos meanwhile).
Click it for **resume workstream**: it's made again, every branch still
parked is claimed back into it with its work and conversations, and its own
conversation returns too. **forget it** drops it and leaves the branches parked
in their repos. **remove
workstream** only ungroups: its slots go back to their repos, still claimed.
A slot released on its own leaves its workstream.
