# Contributing

## Building

```sh
cargo build --release
ln -s "$PWD/target/release/velmux" ~/.local/bin/velmux
```

A running velmux server restarts itself in place when its binary changes
(`auto_restart`), so every release build is picked up by your open session, with
panes kept running. Test risky changes with a debug build on a separate server
first (see below).

## Tests

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

CI runs both on macOS and Linux with the newest stable Rust, whose clippy can
flag things an older local toolchain doesn't: check CI after pushing.

The suite has three layers:

- **Unit tests** in each crate.
- **Pool integration tests** (`crates/velmux-pool/tests/`): real git repos in
  temporary folders: claim, release, parked work, sync, ports, concurrency.
- **End-to-end tests** (`crates/velmux-cli/tests/e2e.rs`): the real `velmux`
  binary in a pseudo-terminal, driven with keys and mouse clicks, its screen read
  back through a terminal emulator. They use a fake Claude
  (`examples/fake_claude.rs`, installed as `…/claude/versions/9.9.9` so velmux
  detects it as Claude; it records sessions the way Claude does and takes
  commands like `claim`, `release`, `say`) and a fake `gh`. Each test gets its
  own socket, state, config, Claude folder and `HOME`. The harness is in
  `tests/common/`.

## A separate server

To try a build without touching your own session, point everything at
throwaway paths:

```sh
export VELMUX_SOCKET=/tmp/velmux-dev.sock      # short: sockets cap at ~104 bytes
export VELMUX_STATE_DIR=/tmp/velmux-dev/state
export VELMUX_CONFIG=/tmp/velmux-dev/config.toml
export CLAUDE_CONFIG_DIR=/tmp/velmux-dev/claude  # or claims link into your real ~/.claude
target/debug/velmux
```

`velmux debug` prints the focused pane as the server holds it (sizes and rows),
handy when the screen and the app disagree.

## Screenshots

The images in the README and docs are generated, not taken:

```sh
cargo test -p velmux-cli --test screenshots -- --ignored
```

builds a demo scene with the end-to-end harness (fake Claudes drawing mock
screens, a fake `gh` for PR status) and writes each view as SVG to
`docs/screenshots/`. Rerun it after UI changes.

## Releasing

1. Bump `version` in the workspace `Cargo.toml` (and `cargo build` to update
   `Cargo.lock`), commit, push.
2. Tag it: `git tag v0.2.0 && git push origin v0.2.0`.

The release workflow builds macOS (arm64, x86_64) and static Linux (x86_64,
arm64) binaries and publishes them with their checksums as a GitHub release.
The [Homebrew tap](https://github.com/vincentvella/homebrew-tap) notices the
new release within the hour and regenerates its formula (run its "Update
formulas" workflow to do it now).

## The site

velmux.com is `site/` (Astro + Starlight), built from `docs/` by
`site/scripts/sync-docs.mjs`. It only deploys when you run a deploy (the
Vercel project isn't connected to the repo), and it builds on your machine, so
deploys use no Vercel build minutes:

```sh
cd site
bun install && bun run dev   # preview at localhost:4321
bun run deploy               # build locally, upload the result to production
```

## Notes

- Agent-state detection rules are adapted from
  [herdr](https://github.com/herdrdev/herdr) (Apache-2.0); keep the attribution.
- Keep docs in `docs/`; the README stays a short introduction.
