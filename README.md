# codemux

> A personal TUI for managing coding-agent sessions, local and over SSH, without leaving the terminal.

[![CI](https://img.shields.io/github/actions/workflow/status/codemuxhq/codemux/ci.yml?branch=main&label=ci)](https://github.com/codemuxhq/codemux/actions/workflows/ci.yml)
[![codemux-cli on crates.io](https://img.shields.io/crates/v/codemux-cli.svg?label=codemux-cli)](https://crates.io/crates/codemux-cli)
[![codemuxd on crates.io](https://img.shields.io/crates/v/codemuxd.svg?label=codemuxd)](https://crates.io/crates/codemuxd)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

```text
┌─ codemux ──────────────────────────────────────────────────────────────┐
│  ▸ 1  refactor-navbar    @local       running                          │
│    2  fix-flaky-test     @prod-pod    idle                             │
│    3  spike-codemod      @local       needs input                      │
│  ────────────────────────────────────────────────────────────────────  │
│                                                                        │
│   [ focused agent's pane -- full-fidelity Claude Code TUI ]            │
│                                                                        │
└─ model: opus-4-7 · codemux:main · ctrl+b ? for help ───────────────────┘
```

> **Status: pre-1.0, single maintainer.** Built primarily for the maintainer's
> daily workflow. Key bindings, config schema, and the wire protocol may shift
> between minor versions. See [`docs/005--roadmap.md`](docs/005--roadmap.md)
> for what's shipped vs. what's next.

## Why

A typical day: 3-4 Claude Code agents running in parallel, some local, some on
devpods, some on a server. They finish at unpredictable times. The agents
themselves work fine; the bookkeeping is the problem. You alt-tab between
terminals to check status, hunt for the right session, and lose track of which
window held what.

codemux is a single TUI where every agent shows up as a navigable pane, local
or over SSH. One keystroke to switch. Status at a glance. No per-machine GUI
install.

The core constraint: **the agent renders itself.** codemux parses VT escape
sequences only to put the agent's own output into a pane. It never interprets
conversation state, tool calls, or session content. If Claude's UI changes
tomorrow, or if you swap agents, codemux keeps working.

Full pitch in [`docs/001--vision.md`](docs/001--vision.md); the four workflows
it's designed for are in [`docs/002--scenarios.md`](docs/002--scenarios.md).

## Features

- **Multi-agent navigator** with three styles (popup, left-pane, hidden).
  Toggle live; remembers per agent: status, host, working directory.
- **One-keystroke switching.** Number keys for direct focus, popup switcher,
  or Vim-style navigation (in flight).
- **Spawn modal** with a fuzzy index over recent paths and `~/.ssh/config`
  hosts. Format: `@<host> : <path>`.
- **Per-agent status bar** showing model, git branch, token usage, and cwd.
  Sourced from Claude Code's own `statusLine` IPC, not parsed out of the TTY.
- **Fully configurable bindings.** The prefix key and every action are
  remappable from `~/.config/codemux/config.toml`.
- **`Cmd+B` on capable terminals.** Auto-negotiates the Kitty Keyboard
  Protocol when any binding uses SUPER.
- **Loud config failures.** Bad config exits non-zero with a readable error
  *before* the terminal switches to raw mode. No silent fallback, no
  half-broken sessions.

What's coming next (persistence, diff panel, save & archive, agent-agnostic
spawn) is tracked in the [roadmap](docs/005--roadmap.md).

## Install

`claude` must be on your `$PATH` for the TUI to spawn agents.

### Homebrew

```sh
brew install codemuxhq/tap/codemux
```

### Shell installer (Linux + macOS)

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/codemuxhq/codemux/releases/latest/download/codemux-installer.sh | sh
```

### Cargo

```sh
cargo install codemux-cli      # the TUI binary: `codemux`
cargo install codemuxd         # per-host daemon (auto-bootstrapped over SSH;
                               # install manually only if you want it on $PATH)
```

### GitHub Releases

Per-target tarballs for `linux-{x86_64,aarch64}` and `darwin-{x86_64,aarch64}`
at <https://github.com/codemuxhq/codemux/releases>.

## Quickstart

```sh
codemux                        # open the TUI in the current directory
codemux ~/work/some-repo       # open in a specific directory
```

| Shortcut          | Action                                     |
|-------------------|--------------------------------------------|
| `Ctrl-B c`        | Spawn a new agent                          |
| `Ctrl-B 1..9`     | Focus agent N                              |
| `Ctrl-B w`        | Popup switcher (arrows + Enter)            |
| `Ctrl-B v`        | Toggle popup vs. left-pane navigator       |
| `Ctrl-B ?`        | Live help (full binding list)              |
| `Ctrl-B q`        | Quit                                       |

### Spawning an agent

`Ctrl-B c` opens a one-row prompt at the bottom:

```
  ▸ /home/you/workbench/repositories/codemux
    /home/you/workbench/repositories/codemux/apps/
    /home/you/workbench/repositories/codemux/crates/
  ────────────────────────────────────────────────
  spawn: @local : /home/you/workbench/repositories/co█  [tab toggle]
```

Format is `@<host> : <path>`. Default focus is the **path** zone; `Tab` (or
`@`) jumps to the host zone, `Tab` again toggles back. The wildmenu shows live
candidates: directory listings for paths, `~/.ssh/config` Host entries for
hosts. `↓ / ↑` highlight; `Enter` spawns at the highlighted candidate (or the
literal text). `Esc` cancels.

Empty host → spawns locally. Empty path → spawns in your current cwd.

## Configuration

Optional. Lives at `~/.config/codemux/config.toml` (XDG-aware on every
platform, including macOS). Missing file = defaults; **bad file exits non-zero
with a readable error.**

```toml
# Override the prefix (default: ctrl+b)
[bindings]
prefix = "ctrl+a"

# Per-action overrides; anything you don't list keeps its default
[bindings.on_prefix]
quit = "x"
help = "?"
```

Useful CLI flags (`codemux --help` for the full list):

| Flag | Env | Description |
|---|---|---|
| `--nav <popup\|left-pane\|hidden>` | `CODEMUX_NAV` | Initial navigator style; toggle live with prefix + `v`. |
| `--log` / `-l` | `CODEMUX_LOG` | Show the in-TUI log strip. Logs always also go to `~/.cache/codemux/logs/codemux.log`. |
| `--agent-bin <path>` | `CODEMUX_AGENT_BIN` | Override the agent binary (default: `claude`). |

### macOS Cmd key

`prefix = "cmd+b"` (or `super+b` / `win+b`) works in **Ghostty, Kitty,
WezTerm, recent Alacritty, Foot**, and partially in **iTerm2**. codemux
auto-enables the Kitty Keyboard Protocol whenever any binding uses SUPER.

**macOS Terminal.app** swallows Cmd before any application can see it. If your
chord doesn't fire after rebinding, switch to a Kitty-protocol-aware terminal
or stay on a Ctrl prefix.

## Why not tmux + Claude Code?

tmux remains better at general terminal multiplexing. Keep using it for
shells, REPLs, log tails, and everything else. codemux is narrower and more
opinionated for one specific job:

|                                   | tmux + Claude            | codemux                                    |
|-----------------------------------|--------------------------|--------------------------------------------|
| At-a-glance status across hosts   | Window names you maintain | Built-in status per agent                 |
| Spawning on a remote host         | SSH, `cd`, `claude`       | Pick host + path from a fuzzy modal       |
| "Which window had agent X?"       | You remember              | Navigator answers it                      |
| General editor / shell / REPL     | Yes                       | Roadmap, not the priority                 |
| Pair / share sessions             | Yes                       | No, single-user by design                 |

If you only run one Claude session locally, plain `claude` is fine. codemux
earns its keep when you have several agents going at once, especially across
multiple hosts.

## Architecture in one paragraph

Two binaries: `codemux` is the TUI (owns local PTYs, runs the event loop,
renders chrome); `codemuxd` is a per-host daemon that owns remote PTYs and
mirrors the child's screen with `vt100` for replay-on-attach. They speak a
small typed wire protocol. The TUI is the only crate that depends on
ratatui/tui-term/crossterm; everything else is reusable. Decisions
(AD-1 through AD-31) and the dependency graph live in
[`docs/004--architecture.md`](docs/004--architecture.md).

## Documentation

- [`docs/001--vision.md`](docs/001--vision.md): what codemux is and the eight UX principles
- [`docs/002--scenarios.md`](docs/002--scenarios.md): the four concrete workflows it's designed for
- [`docs/003--acceptance-criteria.md`](docs/003--acceptance-criteria.md): testable user-task specs
- [`docs/004--architecture.md`](docs/004--architecture.md): stack, data model, all decisions
- [`docs/005--roadmap.md`](docs/005--roadmap.md): lanes of upcoming work

## Contributing

Personal project, but issues and PRs are welcome. For anything non-trivial,
open an issue first. The [vision](docs/001--vision.md) and
[non-goals](docs/001--vision.md#non-goals) are deliberate and shape what gets
accepted.

There is no CI gate beyond `just check` (fmt-check + clippy + test). Run it
before opening a PR. Lints are strict: `unwrap_used` and `expect_used` are
deny; `clippy::pedantic` is warn.

## Inspiration

Credit to **tmux** and **zellij** for the multiplexer idiom. **Claude Code
Desktop**, **Crystal**, and **ccmanager** showed the agent-orchestration UX
problem was worth solving. The **ratatui** stack and **vt100** make a
terminal-native renderer feasible without writing the parser layer from
scratch.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT License](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work by you, as defined in the Apache-2.0
license, shall be dual-licensed as above, without any additional terms or
conditions.
