# codemux

Personal TUI multiplexer for Claude Code agent sessions. Single-user, Rust, ratatui + portable-pty + tui-term. See `docs/vision.md` for the full pitch and `docs/roadmap.md` for current phase and sequencing.

## Workflow conventions

- **No worktrees, no feature branches.** Personal repo — commit directly to `main`.
- **No AI trailers** in commit messages (no `Co-Authored-By: Claude`, no "Generated with").
- **Commit style**: `type(scope): subject`. Types: `fix`, `feat`, `chore`, `docs`, `refactor`. Scope is the phase tag (e.g. `p1`); omit for cross-cutting `chore`/`docs`.
- Lints are strict: `unwrap_used` and `expect_used` are deny; `clippy::pedantic` is warn. Use `#[allow(...)]` on the genuinely-OK cases.

## Just commands

`just --list` shows them all. **There is no CI** — `just check` (fmt --check + lint + test) is the only gate. Run it before every commit.

For verbose tracing: `RUST_LOG=codemux=debug just run` (the `EnvFilter` is scoped to the `codemux` target).

## Repo layout

Cargo workspace, edition 2024, resolver 3. Three crates:

- `apps/tui` — binary `codemux`. Owns PTYs, runs the event loop, renders chrome.
- `crates/session` — agent lifecycle bounded context (mostly P1.4+ scope).
- `crates/shared-kernel` — IDs only (`HostId`, `AgentId`, `GroupId`); zero vendor deps.

Allowed dependency edges: `apps/tui → session, shared-kernel, ratatui/tui-term/vt100/crossterm`. `session → shared-kernel`. **Never** the other direction; **never** any TUI dep in `crates/*`. Architecture rationale lives in `docs/architecture.md` (AD-1 through AD-28).

## Where things live

| Need to change | Look at |
|---|---|
| A new key binding or remap | `apps/tui/src/keymap.rs` (action enum + Bindings POD) — the help screen and config loader pick it up automatically |
| Spawn-modal behavior (host/path zones, autocomplete) | `apps/tui/src/spawn.rs` (single concrete struct — read the top-of-file comment before adding a second variant) |
| Top-level event routing, prefix-key state machine | `apps/tui/src/runtime.rs` `dispatch_key` |
| What renders on screen | `apps/tui/src/runtime.rs` `render_frame` and its helpers |
| CLI flags and env vars | `apps/tui/src/main.rs` `Cli` struct |
| Config file format | `apps/tui/src/config.rs` + the `Bindings` types in `keymap.rs` |
| Agent / Host / status domain types | `crates/session/src/domain.rs` |

## Key invariants worth knowing

- **AD-1: codemux never semantically parses Claude Code.** PTY → vt100 → tui-term → ratatui pane. We render Claude's output, we don't interpret it. Don't be tempted to tail `~/.claude/projects/*.jsonl`.
- **The keymap is the single source of truth.** Bindings POD drives behavior, the help screen, and the status-bar hints. Add a new action by extending the enum + Bindings struct + lookup table; everything else follows.
- **Per-component error types via thiserror, `#[non_exhaustive]`.** No shared workspace-wide `Error` enum. The binary uses `color-eyre` at the edge.
- **Failure mode for config**: missing file = defaults; present-but-invalid = exit non-zero with readable error before touching the terminal. Never silent fallback.
- **Kitty Keyboard Protocol auto-enables** when any binding uses `SUPER` (Cmd/Win). User writes `prefix = "cmd+b"`, the protocol negotiation follows. The help screen is the user-visible escape hatch when their terminal can't deliver Cmd.

## Docs

- [`docs/vision.md`](docs/vision.md) — what codemux is and the eight UX principles
- [`docs/use-cases.md`](docs/use-cases.md) — the four concrete workflows it's designed for
- [`docs/architecture.md`](docs/architecture.md) — stack, data model, all architecture decisions
- [`docs/roadmap.md`](docs/roadmap.md) — phased plan, ship criteria, explicit non-milestones
- [`docs/codemuxd-stages.md`](docs/codemuxd-stages.md) — live tracker for the AD-3 daemon build-out (Stages 0–5)
