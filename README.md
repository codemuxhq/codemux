# codemux

A TUI multiplexer for Claude Code agent sessions, across local and SSH hosts.

## Status

P1.2 in progress. Multi-agent local works: `Ctrl-B c` spawns a new claude in the current cwd, `Ctrl-B n`/`p` cycles, `Ctrl-B 1`-`9` focuses by index. Two navigator styles: **Popup** (default, full-screen claude + 1-row status bar + `Ctrl-B w` for the switcher) and **LeftPane** (always-visible left navigator). Toggle with `Ctrl-B v`. No SSH, no persistence yet — those are next. See [`docs/roadmap.md`](docs/roadmap.md).

## What it is

One TUI window where every Claude Code agent I have running — local or on a remote SSH host — shows up as a navigable pane. Switch between them in one keystroke, see what each is doing at a glance, peek at what each one has changed without leaving the app.

Personal tool. Single-user. TUI-only. Claude Code only.

## Running it

```
cargo run                       # Popup navigator (default)
cargo run -- --nav left-pane    # LeftPane navigator
CODEMUX_NAV=left-pane cargo run # same, via env var
```

Requires `claude` on PATH. `Ctrl-B q` exits.

## Docs

- [`docs/vision.md`](docs/vision.md) — what codemux is and why
- [`docs/use-cases.md`](docs/use-cases.md) — the concrete workflows it's designed for
- [`docs/architecture.md`](docs/architecture.md) — stack, data model, architecture decisions
- [`docs/roadmap.md`](docs/roadmap.md) — phased plan, ship criteria, non-milestones

## License

Private. Not for distribution.
