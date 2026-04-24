# codemux

Personal TUI multiplexer for Claude Code agent sessions. Single-user, Rust, ratatui + portable-pty + tui-term. See `docs/vision.md` for the full pitch and `docs/roadmap.md` for the phase sequencing.

Currently in **P1.3** — multi-agent local works, config-driven keymap, two navigator styles (Popup / LeftPane), spawn minibuffer (`@host : path` structured prompt with SSH-config autocomplete). SSH transport and persistence are next (P1.4+).

## Workflow conventions

- **No worktrees, no feature branches.** Personal repo — commit directly to `main`.
- **No AI trailers** in commit messages (no `Co-Authored-By: Claude`, no "Generated with").
- **Reproducible builds**: caret-range in `Cargo.toml`, exact resolution in `Cargo.lock`. Never `=X.Y.Z` in the manifest (blocks `cargo update` security patches).
- Lints are strict: `unwrap_used` and `expect_used` are deny; `clippy::pedantic` is warn. Use `#[allow(...)]` on the genuinely-OK cases.

## Just commands

`just --list` shows them all. The ones you'll use:

| Command | What it does |
|---|---|
| `just` | List recipes |
| `just run` | `cargo run` — launch with defaults |
| `just run -- --nav left-pane` | Pass args through to the binary |
| `just fmt` | `cargo fmt --all` |
| `just lint` | `cargo clippy --workspace --all-targets -- -D warnings` |
| `just test` | `cargo test --workspace` |
| `just check` | **Pre-push gate**: fmt --check + lint + test. Run this before any commit. |

Use `just check` (not the individual recipes) before committing — it catches the same things CI would if there were CI.

## Repo layout

Cargo workspace, edition 2024, resolver 3. Architecture rationale lives in `docs/architecture.md` (AD-1 through AD-24).

```
codemux/
├── Cargo.toml                  # [workspace.dependencies], [workspace.lints]
├── apps/
│   └── tui/                    # crate: codemux-tui, binary: codemux
│       └── src/
│           ├── main.rs         # CLI parsing, tracing init, calls runtime::run
│           ├── runtime.rs      # event loop; owns PTYs, dispatches keys, renders chrome
│           ├── keymap.rs       # KeyChord parser + per-scope action enums + Bindings POD
│           ├── config.rs       # XDG config loader (~/.config/codemux/config.toml)
│           └── spawn.rs        # spawn-agent minibuffer (single concrete SpawnMinibuffer;
│                               # see top-of-file note before adding more variants)
└── crates/
    ├── session/                # bounded context: agent lifecycle (mostly P1.4+ scope)
    └── shared-kernel/          # IDs only (HostId, AgentId, GroupId); zero vendor deps
```

Allowed dependency edges: `apps/tui → session, shared-kernel, ratatui/tui-term/vt100/crossterm`. `session → shared-kernel`. **Never** the other direction; **never** any TUI dep in `crates/*`.

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
