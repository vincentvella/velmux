# Agents

velmux runs any agent CLI in a pane, and knows four well: Claude Code, Codex,
Gemini CLI and opencode. For those it follows what they're doing, brings their
conversations back, and gives them velmux's instructions.

`agent` in the [config](configuration.md) is the one a new conversation starts
(`claude` by default). To start another, right-click a repo for **new
conversation with…** (or a slot for **agent in this slot…**): it lists the ones
installed.

| | Claude Code | Codex | Gemini CLI | opencode |
|---|---|---|---|---|
| Working / blocked / idle | ✓ | ✓ (it reports its own state; start-up dialogs and questions too) | ✓ (window title and confirmations) | ✓ (permission prompts, progress) |
| Resumes after a restart | ✓ | ✓ | ✓ | ✓ |
| Found by conversation search | ✓ | ✓ | ✓ | |
| velmux's instructions in a repo's checkout | ✓ | ✓ | via `GEMINI.md` | ✓ |
| Can't edit the checkout | ✓ | | ✓ | |

Other agents (aider, cursor-agent…) are recognised in a pane and count as
working while their output is flowing.

## How each one is followed

- **Claude Code** records which session each running process is in; velmux
  starts it with a session id of its own and resumes with `--resume`.
- **Codex** picks its own session id, so velmux finds it from the transcript it
  writes (`~/.codex/sessions/…`): the newest one started in the pane's folder
  since the pane started. It resumes with `codex resume <id>`.
- **Gemini CLI** is started with a session id velmux chooses
  (`--session-id`, which needs a recent Gemini CLI) and resumed with
  `--resume`. Its edit guard is a policy file (`--policy`) denying writes under
  the checkout.
- **opencode** sessions are found in its database (needs `sqlite3`, which
  macOS has) the same way as Codex's, and resumed with `-s <id>`. velmux's
  instructions reach it through `OPENCODE_CONFIG_CONTENT`.

## Instructions for every agent

`velmux skill install` installs the Claude Code skill and, for each of Codex,
Gemini CLI and opencode that's set up on your machine, adds velmux's
instructions to the file it reads for every project (`~/.codex/AGENTS.md`,
`~/.gemini/GEMINI.md`, `~/.config/opencode/AGENTS.md`), between
`velmux:start` / `velmux:end` markers so your own text is left alone.
`velmux skill uninstall` takes them out again.
