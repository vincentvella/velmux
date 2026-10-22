# Changelog

What changed in each release of velmux. Each version links to its release
(downloads and checksums), and `velmux update` gets you the newest.

## [Unreleased]

## [0.8.0] - 2026-10-22

### Added

- Working / blocked / idle for GitHub Copilot CLI, Cursor Agent, Droid, Devin
  CLI, Qwen Code, Grok CLI, Qoder CLI, Kimi Code, Letta Code, Antigravity CLI,
  Kilo Code, pi, oh-my-pi and Hermes Agent, from herdr's detection rules
  (their permission prompts, spinners, titles and progress reports), instead of
  just "working while output flows". They show as blocked when they ask you.

### Fixed

- Copying from an agent's pane also rejoins lines it wrapped at a space
  (Claude's paragraphs, and commands it broke before a flag), putting the
  space back; list items, boxes and code keep their line breaks.

### Changed

- Releasing a workstream keeps its own conversation: its panes move to its
  first repo's checkout, still running, instead of closing. Its folder goes
  once nothing is working in it.

## [0.7.1] - 2026-10-22

### Added

- **claim a slot in a repo…** in a workstream (its right-click menu, or `n` on
  its empty screen): pick a repo and a branch, and the slot joins it.

### Changed

- Releasing a slot (its menu, `⏏ free`) or a whole workstream moves the panes
  open in it back to the repo's checkout, still running, instead of closing
  them.
- The demo screenshots show a workstream.

## [0.7.0] - 2026-10-21

### Added

- **Workstreams**: slots from one or more repos grouped as one piece of work,
  in their own section of the sidebar, with a place where one agent reaches
  every slot (its folder links to each, one subfolder per repo). Start one from
  the heading's `+` or a slot's **add to workstream…**, drag slots in and out,
  and let an agent bring in another repo with
  `velmux claim <branch> --repo <repo>`. **release workstream…** hands back all
  of its slots.

### Changed

- The sidebar has three sections: Workstreams, Repos and Scratch (the scratch
  sessions' divider is now a heading).
- A menu opened from another menu (the agent and workstream pickers) appears
  where that one was.

## [0.6.0] - 2026-10-21

### Added

- A slot's Claude conversations are named after it: its branch, or the name
  you gave the slot, as a `/rename` title, so the `/resume` picker and
  conversation search show it. One you've named yourself keeps your name;
  `name_conversations = false` turns it off.
- A slot whose PR has merged shows `#142⇡ · ⏏ free`: click **⏏ free** to
  release it.

### Changed

- New conversations in a repo always start in its checkout, even from a slot:
  they plan there and claim a slot of their own. Claiming a slot by hand opens a
  shell in it.
- Next / previous place (`⌘⌃]` / `⌘⌃[`, `<prefix> .` / `,`) visit every place
  in the sidebar, in its order, instead of skipping ones with no panes open.

### Fixed

- Copying from an agent's pane rejoins the long lines (commands, paths) the
  agent wrapped itself, instead of copying its line breaks and indentation.
- Installs and claim hooks run in your shell, so nvm (and fnm, mise…) pick the
  slot's node version; with a `.nvmrc`, nvm is switched explicitly.
- `suspended (tty input)` when a claim ran an install: nothing velmux starts
  can take over your terminal any more.
- After an agent exits, its pane becomes your own shell, always interactive
  (your `.zshrc` loads).
- The pool's file lock no longer fails a claim where it can't be taken
  (sandboxes, filesystems without `flock`).

## [0.5.0] - 2026-10-21

### Added

- A resizable sidebar: drag its edge, or `<prefix> <` / `>`. Its width is kept
  across restarts, never narrower than 20 columns.
- Resize keys repeat without the prefix, like tmux: `<prefix> > > >`, and
  `<prefix> H H H` for splits.
- A slot's PR number opens the PR when clicked (it's a terminal hyperlink too);
  `$BROWSER` is used when set.

### Changed

- Slots take two lines in the sidebar: the branch gets the whole first line
  (with its dirty mark and agents' states); the second has the slot (`@2`), how
  it stands against the base (`↓3 ↑1`), its PR and checks, and any conflict.

## [0.4.0] - 2026-10-21

### Added

- Codex, Gemini CLI and opencode are followed like Claude Code: working /
  blocked / idle, conversations resumed after a restart, Codex and Gemini
  conversations in search, velmux's instructions in the checkout. **New
  conversation with…** starts any agent that's installed.
- 15 more agents are resumed after a restart through a session hook or plugin
  that `velmux skill install` adds: GitHub Copilot CLI, Cursor Agent, Droid,
  Devin CLI, Qwen Code, Grok CLI, Qoder CLI, Kimi Code, Letta Code, Mastra Code,
  Antigravity CLI, Kilo Code, pi, oh-my-pi and Hermes Agent. `velmux doctor`
  lists any not hooked yet.
- Pools laid out beside the repo (`shop-pool-1`, `shop-pool-2`…, the way some
  other tools do it) are taken in as its slots where they are.
- `focus_follows_mouse = true`: focus follows the pointer between panes.

### Changed

- Sharper working / blocked detection for Codex, Gemini CLI and opencode
  (rules adapted from herdr): Codex's trust and update dialogs, questions and
  approvals; Gemini's confirmations; opencode's permission picker and progress
  bar.

## [0.3.0] - 2026-10-21

### Added

- Update checks: velmux asks velmux.com at startup and twice a day and says
  once per release when a newer one is out (`↑` on the menu). **update
  velmux…** / `velmux update` updates it the way it was installed (Homebrew, a
  checksum-verified swap of a downloaded binary, cargo); `update_check = false`
  turns the checks off.
- velmux.com, with the docs, an install box for your platform and
  `curl -fsSL https://velmux.com/install.sh | sh`.

## [0.2.0] - 2026-10-21

### Added

- First run: a welcome screen, and empty places that show their single-key
  actions and a get-set-up checklist (install the Claude skill, Ghostty keys,
  doctor). `velmux doctor` checks your setup.
- Sounds when an agent finishes or needs you (picked from a list, played as you
  browse), and notifications whenever your terminal isn't in front.
- The checkout stays current and clean: it's fast-forwarded to its upstream
  after each fetch, you're told which slots fell behind (**sync all slots**),
  uncommitted changes are flagged (`✎N`) with **move changes to a slot…**, and
  Claudes velmux starts can't edit it (a hook plus a permission rule;
  `guard = false`).

## [0.1.0] - 2026-10-21

The first release.

- A small pool of warm git worktree slots per repo, claimed for a task and
  handed back when it's done, with dependencies and build caches kept.
- Agents plan in a repo's checkout and claim a slot when they're ready to change
  code (the `velmux` Claude skill); unfinished work is parked and resumed with
  its conversation.
- A sidebar of every repo, slot and session with each agent's state, and a
  notification when one needs you.
- Tabs, splits, copy mode, mouse, themes, ⌘ shortcuts, conversation search, PR
  and CI status per slot, per-slot ports, and live restarts that keep every
  pane running.

[Unreleased]: https://github.com/vincentvella/velmux/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/vincentvella/velmux/releases/tag/v0.8.0
[0.7.1]: https://github.com/vincentvella/velmux/releases/tag/v0.7.1
[0.7.0]: https://github.com/vincentvella/velmux/releases/tag/v0.7.0
[0.6.0]: https://github.com/vincentvella/velmux/releases/tag/v0.6.0
[0.5.0]: https://github.com/vincentvella/velmux/releases/tag/v0.5.0
[0.4.0]: https://github.com/vincentvella/velmux/releases/tag/v0.4.0
[0.3.0]: https://github.com/vincentvella/velmux/releases/tag/v0.3.0
[0.2.0]: https://github.com/vincentvella/velmux/releases/tag/v0.2.0
[0.1.0]: https://github.com/vincentvella/velmux/releases/tag/v0.1.0
