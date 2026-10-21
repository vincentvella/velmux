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

## More agents, resumed through their hooks

velmux also resumes GitHub Copilot CLI, Cursor Agent, Droid, Devin CLI, Qwen
Code, Grok CLI, Qoder CLI, Kimi Code, Letta Code, Mastra Code, Antigravity
CLI, Kilo Code, pi, oh-my-pi and Hermes Agent. They choose their own session ids, so `velmux skill install` gives each
one that's set up on your machine a session-start hook (`velmux agent-hook`)
that tells velmux which session the pane is in; after a restart it's brought
back with that agent's resume command (`--resume <id>`, `--session <id>`…).
The hook is added beside your own hooks in the agent's settings
(`~/.factory/settings.json`, `~/.copilot/settings.json`, `~/.cursor/hooks.json`,
`~/.kimi-code/config.toml`…); Kilo, pi, oh-my-pi and Hermes, which take
plugins instead, get a small one of velmux's own (`~/.config/kilo/plugin/`,
`~/.pi/agent/extensions/`, `~/.omp/agent/extensions/`, `~/.hermes/plugins/`,
enabled in Hermes's `config.yaml`). Either way it does nothing outside velmux, and
`velmux skill uninstall` takes it out. Start them from **new conversation
with…** or by hand in a pane; `velmux doctor` lists any that are set up but
not hooked yet.

For these, working / idle comes from whether their output is flowing, and
they don't get velmux's instructions or the checkout guard. Other agents
(aider…) are recognised in a pane the same way, without resuming.

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
