# Configuration

Two files, both optional. Changes apply without a restart: `<prefix> s` opens
the global config in your editor and reloads it when you quit; `<prefix> R`
reloads it now.

## Global: `~/.config/velmux/config.toml`

(Or `$VELMUX_CONFIG`.)

```toml
prefix = "C-b"                           # the prefix key: C-a, ctrl+space, ^], …
slots = 5                                # slots per repo
pool_root = "~/Workspace/.pools"         # default: next to each repo, in .pools/
scratch_dir = "~/Workspace"              # new sessions start here; new space lists its repos
agent = "claude"                         # the command agent panes run
editor = "code"                          # for "open in editor" (else $VISUAL / $EDITOR)
mouse = true
focus_follows_mouse = false              # focus the pane under the pointer as it moves
auto_restart = true                      # restart in place when the velmux binary changes
claude_history = true                    # group each slot's Claude transcripts under its repo
fetch_every = 15                         # minutes between background fetches (0 = off)
fast_forward = true                      # fast-forward each checkout to its upstream after a fetch
guard = true                             # keep Claudes velmux starts from editing a checkout
name_conversations = true                # name a slot's Claude conversations after its branch
update_check = true                      # look for a newer velmux at startup and twice a day
sync = "rebase"                          # or "merge": how sync with base catches up
theme = "rose-pine"
```

### Keys

Rebind any action; an empty list unbinds it. `velmux keys` lists every action
and its bindings.

```toml
[keys]
split_right = ["prefix+v", "super+d"]
location = "alt+1..9"     # 1..9 ranges for location / tab / slot
new_tab = []
```

Prefix-less bindings (`super+…`, `ctrl+1`) need your terminal to send them:
`velmux keys --ghostty` prints the lines for Ghostty.

### Notifications

```toml
[notify]
desktop = true
sound = true
done_sound = "herdr-done"         # herdr-done confirm glass pluck chime, none,
blocked_sound = "herdr-request"   # herdr-request question bong knock, a macOS
                                  # system sound name (Glass, Funk…), or a file
```

### Themes

```toml
theme = "rose-pine"

[theme_colors]            # override any role: #rrggbb, 0–255, or a colour name
accent = "#f6c177"
working = "yellow"
```

Roles: `text` `text2` `dim` `accent` `selected_bg` `base_glyph` `slot_glyph`
`dirty` `working` `blocked` `done` `idle` `focus_bg` `focus_fg` `title_bg`
`title_fg` `bar_bg` `bar_fg` `alert` `jobs` `menu_border` `picker_border`
`toast_border` `copy_bg` `copy_fg` `tag_bg` `tag_fg`.

## Per repo: `<repo>/.velmux.toml`

```toml
slots = 3
base_ref = "origin/main"            # default: origin/HEAD, else the checkout's branch
keep = ["node_modules", "ios/Pods"] # kept on release (sensible defaults)
link = [".env.local"]               # linked from your checkout (default: gitignored .env*)
post_claim = ["bun run codegen"]    # run after each claim
install = true                      # install dependencies when the lockfile changes
                                    # (in your shell, so nvm & co. pick the slot's node)
install_cmd = "bun install"         # instead of detecting the package manager
sync = "merge"
```

### Ports

In a repo's `.velmux.toml` or the global config:

```toml
[ports]
stride = 10                                   # slot N adds N × stride
env = { PORT = 3000, RCT_METRO_PORT = 8081 }  # each variable's value in your checkout
# enabled = false
```

Two repos with slots running at the same time want different bases (e.g.
`PORT = 4000` in one).
