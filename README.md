# codemux

A TUI multiplexer for Claude Code agent sessions, across local and SSH hosts.

## Status

P1.3 in progress. Multi-agent local works with a config-driven keymap (`Ctrl-B ?` shows the live binding list). `Ctrl-B c` opens a spawn modal with host + path fields and filesystem autocomplete. Two navigator styles: **Popup** (default, full-screen claude + 1-row status bar + `Ctrl-B w` switcher) and **LeftPane** (always-visible left navigator). Toggle with `Ctrl-B v`. No SSH, no persistence yet — those are next. See [`docs/roadmap.md`](docs/roadmap.md).

## What it is

One TUI window where every Claude Code agent I have running — local or on a remote SSH host — shows up as a navigable pane. Switch between them in one keystroke, see what each is doing at a glance, peek at what each one has changed without leaving the app.

Personal tool. Single-user. TUI-only. Claude Code only.

## Running it

```
cargo run                       # Popup navigator (default)
cargo run -- --nav left-pane    # LeftPane navigator
CODEMUX_NAV=left-pane cargo run # same, via env var
```

Requires `claude` on PATH. `Ctrl-B q` exits. `Ctrl-B ?` opens help.

## Configuration

Optional. Lives at `~/.config/codemux/config.toml` (XDG-aware). Missing file = defaults. Example:

```toml
# Override the prefix key (default: ctrl+b).
[bindings]
prefix = "ctrl+a"

# Per-action overrides. Anything you do not list keeps its default.
[bindings.on_prefix]
quit = "x"
help = "?"
```

A bad config exits non-zero with a readable error; it does not silently fall back.

### macOS Cmd key

`cmd+b` (or `super+b` / `win+b`) is parseable, and codemux auto-enables the Kitty Keyboard Protocol on startup whenever any of your bindings use the SUPER modifier — so the Cmd press actually arrives at the application.

```toml
[bindings]
prefix = "cmd+b"
```

This works in **Ghostty, Kitty, WezTerm, recent Alacritty, Foot, and partially in iTerm2**. It does not work in **macOS Terminal.app** — that terminal swallows Cmd before any application can see it. If your chord doesn't fire after rebinding, your terminal is the limit, not codemux. Switch to a Kitty-protocol-aware terminal or fall back to a Ctrl-based prefix.

## Docs

- [`docs/vision.md`](docs/vision.md) — what codemux is and why
- [`docs/use-cases.md`](docs/use-cases.md) — the concrete workflows it's designed for
- [`docs/architecture.md`](docs/architecture.md) — stack, data model, architecture decisions
- [`docs/roadmap.md`](docs/roadmap.md) — phased plan, ship criteria, non-milestones

## License

Private. Not for distribution.
