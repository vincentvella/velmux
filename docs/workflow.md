# Working with velmux

[Getting started](getting-started.md) covers one task from start to finish. This
is what a day with velmux looks like once several are on the go: starting work,
keeping up with it, reviewing it, putting it away, and work that spans repos.
It assumes `velmux skill install` has been run, so Claude knows the moves.

![velmux: a workstream across two repos, claimed slots, PR status, parked work and a scratch session](screenshots/overview.svg)

## Start several things

Each task starts as a conversation in the repo's **checkout**: `<prefix> a` (or
`⌘N`, or right-click the repo). Ask for one thing per conversation; start the
next straight away rather than waiting.

Every Claude reads and plans in the checkout, which it can't edit (velmux keeps
it clean), and **claims a slot** of its own when it's ready to change code. Its
pane moves under that slot, its conversation is named after the branch, and the
slot keeps its dependencies warm, so it's building and testing within seconds.
Each slot gets its own `$PORT` (3020 for slot 2; more with
[`[ports]`](configuration.md#ports)), so five slots can all run their dev
servers at once.

To do something by hand instead, claim a slot yourself (`<prefix> n`) and work
in a shell there.

## Keep up without watching

You don't need to watch the agents: the sidebar does.

| | |
|---|---|
| `●` | working |
| `▲` | blocked on you (a permission prompt, a question) |
| `✓` | finished while you were elsewhere |
| `○` | idle |

You get a notification and a sound when one is blocked or finishes, as long as
your terminal isn't in front. `<prefix> o` jumps to the next agent that needs
you. The **agents** view (`<prefix> g`, or click `agents` at the top of the
sidebar) lists them all: the ones that need you first, then the rest by repo,
each with how long it's been at it and, for Claude, how full its context is.

Lost track of an old conversation? `<prefix> /` searches every one, in every
repo, and opens it where it ran.

## Review

When an agent has something to show, its slot is where to look:

- **Open it in your editor** or a shell there: right-click the slot, or
  `<prefix> space` (the menu for whatever's focused).
- **Push it and open a PR**: the slot's menu, **push & open PR**. Its PR then
  shows on the slot's second line with its checks (`#142✓`, `#143✗`, `#144…`);
  click the number to open it.
- **Failing checks** go straight back to the agent: **send failures to Claude**.
- **Behind main?** `↓3` on the slot says so; **sync with base** rebases it (or
  merges, per your config), and the repo's menu syncs them all.

## Put it away

- **Merged**: the slot shows `⏏ free`; click it to hand the slot back.
- **Not finished**: release it anyway (its menu, or the agent runs
  `velmux release`). Uncommitted work is **parked** on its branch and the slot
  is reset, dependencies kept. Parked work shows under its repo (`◌ feat/x`);
  click it for **resume in a slot**: the work and its conversation come back in
  whatever slot is free.
- Parked work you'll never come back to gets flagged after 30 days; the repo's
  menu clears it out.

Quitting velmux, a crash or a reboot loses nothing: every pane comes back with
its scrollback, and each Claude resumes its conversation.

## Work that spans repos

When one change needs two repos (an API and the app that calls it), make it a
**[workstream](workstreams.md)**: the `+` on the Workstreams heading, or a
slot's **add to workstream…**. A Claude started in the workstream sees each
repo as a subfolder and can claim a slot in another repo itself
(`velmux claim <branch> --repo api`).

Its row sums up all its PRs (`3 PR ✗1`), its menu syncs every slot and pushes
every branch for PRs at once, and when everything has merged it says so and
offers `⏏ release`. Released half-way, it's kept as one and comes back whole.

## Keep the checkout clean

Your checkout is where agents read and plan, and where you pull main. velmux
fast-forwards it after each fetch and flags changes made there (`✎2`); **move
changes to a slot…** (the repo's menu) takes them to a branch of their own.

## The keys worth remembering

The prefix is `ctrl+b` unless you changed it; press it and a popup shows every
key.

| | |
|---|---|
| `a` | new conversation (in the checkout) |
| `n` / `r` | claim a slot / release it |
| `space` | the menu for what's focused |
| `o` | the next agent that needs you |
| `g` | spaces / agents view |
| `w` | go anywhere (fuzzy-find) |
| `/` | find a conversation |
| `\|` / `-` | split right / down |

[Using velmux](using-velmux.md) has the rest: tabs, splits, copy mode, the mouse,
themes and the ⌘ shortcuts.
