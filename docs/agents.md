# Agents

velmux runs any agent CLI in a pane, and knows four well: Claude Code, Codex,
Gemini CLI and opencode. For those it follows what they're doing, brings their
conversations back, and gives them velmux's instructions.

`agent` in the [config](configuration.md) is the one a new conversation starts
(`claude` by default). To start another, right-click a repo for **new
conversation with…**: it lists the ones installed. New conversations in a repo
always start in its checkout, even from one of its slots: they plan there and
claim a slot of their own when it's time to change code.

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

Their working / blocked / idle comes from herdr's detection rules for each
(their permission prompts, spinners, window titles and progress reports;
Mastra Code, which has none, counts as working while its output is flowing).
They don't get velmux's instructions or the checkout guard. Other agents
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
`velmux skill uninstall` takes them out again. After an update, velmux brings
them up to date itself (the skill too, if you haven't changed it).

## Any program can say what it's doing

A program that isn't an agent (a CI watcher, a long build, a test runner) can
show in the sidebar and the agents view like one, by writing its state to the
terminal as an OSC 7501 sequence, the one Codex and herdr use:

```sh
printf '\033]7501;state=working\033\\'
```

`state` is `working`, `blocked`, `idle`, `done` or `error`; `error` counts as
needing you, like `blocked` (and notifies you the same way), and `done` after
`working` marks it finished while you weren't looking. Its window title (OSC
0 or 2) shows beside it, as an agent's does: `printf '\033]2;3/5 ✓ svc#42\007'`.
velmux sets `VELMUX_PANE` in its panes: check for it to report only inside velmux.

A report lasts while the program that made it is in front: when it exits and
you're back at the shell, it's gone. `state=clear` ends it sooner.
