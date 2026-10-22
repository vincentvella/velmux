---
name: velmux
description: Working inside velmux, the worktree-pool terminal for agents. Use when $VELMUX_PANE is set, or when the user mentions velmux, slots, claiming or releasing a slot, parked work, or syncing a slot with the base. Covers claiming a worktree slot to change code, releasing it, resuming parked work, syncing with the base, per-slot ports, and PRs.
---

<!-- velmux skill v1: installed by `velmux skill install`; rerun it to update -->

# velmux

velmux runs agents in a repo's **base checkout** (the "space") and in a pool of
warm git worktrees, the **slots** (`<pool root>/<repo>/slot-N`). Each slot keeps
its dependencies installed between tasks, so claiming one is fast.

## Where am I?

- `$VELMUX_PANE` set: you're in a velmux pane.
- `$VELMUX_SLOT` set (e.g. `slot-2`): you're in that slot, on its own branch.
- Only `$VELMUX_BASE` set: you're in the base checkout.
- `$VELMUX_WORKSTREAM` set: you're in that workstream (see below).

## In the base checkout: plan, then claim

(If an edit is refused with a velmux message about the checkout, this is why:
claim a slot and make the edit there.)

Read, explore and plan in the base, but **don't change code there**. When you're
ready to edit, claim a slot on a branch named for the work:

```sh
velmux claim feat/short-name
```

It prints the slot's path; work there from then on (use that path for every
file edit and command). Inside velmux, your pane moves under that slot in the
sidebar. If the branch already exists (or was parked), you get it back with any
parked changes restored.

## In a slot

- Commit as you go; the branch is yours.
- `velmux sync` catches the branch up with the base (fetch, then rebase or merge,
  with uncommitted work set aside and put back). If it reports conflicts it
  backed out and left the slot as it was: rebase onto the base ref yourself and
  resolve them, or tell the user.
- `$PORT` (and any other configured port variables) is unique to the slot: start
  dev servers on it, not on a hard-coded port. `velmux env` prints them.
- For a PR: push the branch and use `gh pr create`. If CI fails, `gh pr checks`
  and `gh run view --log-failed` show why.

## In a workstream

Work that spans repos (an API change and the app that uses it) can be a
**workstream**: slots from one or more repos, grouped. In one
(`$VELMUX_WORKSTREAM`), each subfolder of your folder is a link to one of its
slots, named after the repo, each a worktree on its own branch.

- Make changes inside those subfolders, and commit in each repo's own git.
- To bring in another repo: `velmux claim <short-branch-name> --repo <repo>`
  (its name in velmux, or its path). The slot is linked here as `<repo>/`, and
  your pane stays where it is.
- From a repo's checkout, `velmux claim <branch> --workstream <name>` claims a
  slot into a workstream (making it if needed) instead of moving you there.
- Don't release a workstream's slots yourself; the user releases the workstream
  when it's done.

## Done: release

```sh
velmux release
```

This hands the slot back. Uncommitted changes are stashed and the branch is
**parked** (it shows in the sidebar to resume later), the slot is reset with its
dependencies kept, and your conversation moves back to the base. Release once
the work is committed (or merged), not mid-task.

## Other commands

- `velmux status`: every slot (branch, dirty `*`, `↓` behind / `↑` ahead the
  base, port) and parked branches.
- `velmux parked`: parked branches; `velmux claim <branch>` resumes one.
- `velmux discard <branch>`: drop a parked branch's stashed changes (ask first).

## Don't

- Don't create, move or delete the slot worktrees yourself (`git worktree …`,
  `rm -rf` a slot): velmux owns them.
- Don't `git clean` away `node_modules` or other dependency folders in a slot;
  keeping them warm is the point.
- Don't edit the base checkout while a slot exists for that work.
