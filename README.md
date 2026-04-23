# codemux

A TUI multiplexer for Claude Code agent sessions, across local and SSH hosts.

## Status

P0 done. The walking skeleton runs: a single local `claude` session in a full-window TUI, keystrokes forwarded, Ctrl-C exits cleanly. No navigator, no persistence, no SSH yet — those are P1. See [`docs/roadmap.md`](docs/roadmap.md).

## What it is

One TUI window where every Claude Code agent I have running — local or on a remote SSH host — shows up as a navigable pane. Switch between them in one keystroke, see what each is doing at a glance, peek at what each one has changed without leaving the app.

Personal tool. Single-user. TUI-only. Claude Code only.

## Running it

```
cargo run
```

Requires `claude` on PATH. Ctrl-C exits.

## Docs

- [`docs/vision.md`](docs/vision.md) — what codemux is and why
- [`docs/use-cases.md`](docs/use-cases.md) — the concrete workflows it's designed for
- [`docs/architecture.md`](docs/architecture.md) — stack, data model, architecture decisions
- [`docs/roadmap.md`](docs/roadmap.md) — phased plan, ship criteria, non-milestones

## License

Private. Not for distribution.
