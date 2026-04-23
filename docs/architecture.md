# Architecture

## At a glance

```
┌──────────────────── codemux TUI process ─────────────────────┐
│                                                              │
│  ratatui chrome  ──────  focused-agent pane (tui-term)       │
│  (status bar,            ┌────────────────────────────────┐  │
│   later: navigator,      │  <cell grid parsed from the    │  │
│   diff panel)            │   focused PTY's VT output>     │  │
│                          └────────────────────────────────┘  │
│                                      │                       │
│                                      │ portable-pty          │
│                                      ▼                       │
│                          ┌───────────────────────────┐       │
│                          │ PTY per agent             │       │
│                          │  └── local: claude        │       │
│                          └───────────────────────────┘       │
└──────────────────────────────────────────────────────────────┘
```

codemux is a single Rust binary. There is no daemon on any remote host, no
server, no web frontend. P0 spawns one local PTY directly. SSH transport,
persistence, multi-agent navigator, diff panel, and notifications all land in
later phases — see "Deferred ideas" below.

## Workspace layout

```
codemux/
├── Cargo.toml                       # [workspace], [workspace.dependencies], [workspace.lints]
├── apps/
│   └── tui/                         # crate: codemux-tui, binary: codemux
│       └── src/
│           ├── main.rs              # argv, tracing init, calls runtime::run()
│           ├── runtime.rs           # event loop, owns the PTY
│           └── ui/                  # ratatui chrome (mostly P1+)
└── crates/
    ├── session/                     # bounded context: agent lifecycle
    │   └── src/
    │       ├── lib.rs
    │       ├── domain.rs            # Agent, Host, AgentStatus
    │       └── error.rs             # thiserror, #[non_exhaustive]
    └── shared-kernel/               # IDs only; zero vendor deps
        └── src/lib.rs               # HostId, AgentId, GroupId
```

Allowed dependency edges:

- `session` → `shared-kernel`
- `apps/tui` → `session`, `shared-kernel`, UI libs (`ratatui`, `tui-term`,
  `vt100`, `crossterm`)

Forbidden:

- Any `crates/*` depending on `ratatui` / `tui-term` / `vt100` / `crossterm`
- Any `crates/*` depending on `apps/*`
- Any cycle between component crates

## Data model

```
Host    { id, name, kind: local|ssh, ssh_target?, last_seen }
Agent   { id, host_id, label, cwd, group_ids[], session_id?, status, last_attached_at }
Group   { id, name, color }   # arrives with P3 tagging
```

- **Host** — a machine codemux can spawn Claude Code on. `local` uses direct
  fork; `ssh` uses `ssh_target` as the hostname (P1).
- **Agent** — a logical workspace. Persists across PTY deaths and app restarts
  (once persistence lands in P1). Killing the PTY does not delete the agent.
- **Group** — a free-form tag (P3). Many-to-many with agents.

P0 keeps a single live `Agent` in memory. Persistence is P1.

## Architecture decisions

### AD-1 — Host the PTY, do not *semantically* parse Claude Code

codemux parses the VT escape sequences of each agent's PTY via `tui-term` /
`vt100` — but only to render the cell grid into a pane. It never interprets
conversation state, tool calls, permission prompts, or session contents. Claude
Code's UI is opaque.

Rejected: tailing `~/.claude/projects/*.jsonl` and rendering messages
ourselves. Easier diff view, tighter integration — but every Claude Code
release becomes a chase, and approval prompts / interactive flows are a
nightmare to reimplement.

Consequence: whatever Claude Code can do in a terminal, codemux supports for
free. Whatever codemux wants to *show beside* Claude Code, it derives from
out-of-band sources (git for diffs, host probes for liveness).

### AD-10 — PTY library: `portable-pty`

Pure-Rust PTY spawning, cross-platform, works for both local fork and (later)
`ssh` subprocesses. Versioning: caret-range in `Cargo.toml`, exact resolution
in `Cargo.lock`.

### AD-11 — TUI stack: `ratatui` + `tui-term` + `vt100`

- **`ratatui`** for chrome (status bar; P1+ adds navigator, diff panel, new-agent sheet).
- **`tui-term`** widget for nested PTY rendering — drops into a ratatui `Rect`
  with zero glue.
- **`vt100`** underneath `tui-term` for VT parsing.

Versioning: caret-range in `Cargo.toml`, exact resolution in `Cargo.lock`. As
of 2026-04, `ratatui 0.30.0` + `tui-term 0.3.4` + `vt100 0.16.2` compose
cleanly. Note that `ratatui 0.30.0` is itself a workspace split (`ratatui-core`,
`ratatui-widgets`, `ratatui-crossterm`, `ratatui-macros`); the top-level
`ratatui` crate re-exports the lot.

Fallback: **`alacritty_terminal`** — if OSC 8 (hyperlinks), full SGR mouse
modes, or alt-screen edge cases start to bite, swap the terminal backend.
Contained refactor, not an architectural change. Direct precedent: `egui_term`,
`gpui-terminal`, and `missiond-core` all chose `alacritty_terminal` for
fidelity.

Rejected: rolling our own on top of `vte` (zellij's path). Most powerful, most
work. Not worth it for a personal tool when `tui-term` exists.

Rejected: `wezterm-term` — not cleanly available on crates.io as a standalone
crate.

Rejected: in-process rendering of Claude Code via JSONL tailing or
protocol-aware chrome. See AD-1.

### AD-15 — Package by Component, not by technical layer

Crates and modules are bounded by domain concern (`session`), not by technical
layer (`state-store`, `tui-chrome`, `pty-host`). A component crate owns its
domain types and (when they exist) its use cases, ports, and adapters.

Rejected: horizontal layering into `domain`, `state-store`, `pty-host`,
`tui-chrome`, `notify`, `app-shell` crates. This is the Lasagna Architecture
anti-pattern — small updates reverberate through every layer, proxy methods
accumulate at boundaries, and the source tree tells you nothing about what the
application *does*. Screaming Architecture wins: top-level folders under
`crates/` name bounded contexts.

### AD-17 — Per-component error types via thiserror

Each library crate defines its own `thiserror` enum, marked `#[non_exhaustive]`.
The binary uses `color-eyre` at the edge to wrap library errors for
human-readable reporting.

Rejected: a shared `Error` enum for the whole workspace. Ball-of-Mud trap —
every crate must know every other crate's failure shape, and the enum grows
until abstraction layers collapse. Per-crate errors keep each bounded context's
failure vocabulary contained; the binary is the only place that needs to talk
in "any error".

### AD-21 — Workspace-wide dependencies and lints

Shared dependencies are declared in `[workspace.dependencies]` at the root.
Member crates inherit with `{ workspace = true }`. Versioning policy:
**caret-range in `Cargo.toml`, exact resolution in `Cargo.lock`** — per the
Cargo idiom. Do not use `=X.Y.Z` in the manifest; it blocks `cargo update` from
pulling security patches.

Shared lints are declared in `[workspace.lints]`. Rust edition 2024, Cargo
resolver 3.

```toml
[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = { level = "deny", priority = -1 }
pedantic = { level = "warn", priority = -1 }
unwrap_used = "deny"
expect_used = "deny"
```

The `unwrap_used` and `expect_used` denies are Clippy `restriction` lints
(opt-in by Clippy's own design); they're enforced here because this is
intended as long-lived code and the `#[allow(...)]` escape hatch handles the
genuinely-OK cases. Member crates inherit with `[lints] workspace = true`.

Rejected: `cargo-hakari`. Needed only in very large workspaces; edition 2024 +
resolver 3 handle feature unification natively.

## Deferred ideas

Architecture decisions sketched in earlier drafts but not load-bearing for what
exists today. Each will return to the main body — with full prose — when its
phase ships.

- **AD-2 — One PTY per agent, recoverable via Claude session ID.** P1 reattach
  story: store Claude session ID, reattach with `claude --resume <id>` on focus
  if the PTY died.
- **AD-3 — Wrap remote `claude` in `tmux new -A -s ccmux-<id>`.** P1 SSH
  transport. Survives a dropped SSH connection; no tmux UI shown to the user.
- **AD-5 — Single Rust process, SSH outward.** No daemon on remote hosts; SSH
  subprocess provides the PTY for remote agents.
- **AD-6 — Edits panel is `git diff`, read-only.** P2. Inline `syntect` peek;
  one-keystroke "open in `$EDITOR`" for deep review. No staging, no
  annotations.
- **AD-7 — State is a single SQLite file via `rusqlite`.** P1 persistence at
  `$XDG_STATE_HOME/codemux/state.db`. Schema migrations via
  `rusqlite_migration`.
- **AD-8 — No auth.** Moot in TUI mode (no network surface). Revisit only if a
  P4 phone view ever happens.
- **AD-12 — Nested-PTY input routing via prefix key.** P1. Tmux-style prefix
  (default `Ctrl-B`); without the prefix, keys go verbatim to the focused PTY.
- **AD-13 — OS notifications via platform helper shell-out.** P2.
  `osascript` (macOS) / `notify-send` (Linux). No platform-specific crates.
- **AD-14 — Cargo workspace with `apps/` and `crates/` split.** Already in
  place; binary at `apps/tui/` is thin (argv, tracing, delegate to library).
- **AD-16 — TUI binary at `apps/tui/`, crate `codemux-tui`, binary `codemux`.**
  Directory describes the delivery shape. A future `apps/phone-view/` becomes a
  sibling without renaming.
- **AD-18 — Infrastructure adapters co-located with their component.** When
  the `session` crate grows back its real `infra/`, it lives inside `session`,
  not in a separate `crates/infra/`.
- **AD-19 — `shared-kernel` carries IDs and tracing only.** Already in place.
  `HostId`, `AgentId`, `GroupId` newtypes; zero vendor deps.
- **AD-20 — Ports-and-adapters inside each component crate.** Re-introduced
  per component when a real second adapter arrives (e.g. when SSH transport
  joins local PTY transport in P1, the port/adapter split earns its keep).
- **AD-22 — Navigator is a view model in `apps/tui`, not its own crate.** P1.
  The navigator's *data* belongs to `session`; its *UI state* (selection
  index, filter, collapse) belongs in the TUI delivery.
- **AD-23 — Fitness functions for extracting further crates.** Extract a new
  crate only when one of these signals fires: (1) full workspace `cargo test`
  on warm cache exceeds ~15s, (2) a component needs different feature flags on
  a shared dep, (3) a component becomes Claude-Code-agnostic and is a credible
  OSS publication candidate, (4) a second binary needs the session domain
  without the TUI adapters.

### Process lifecycle (deferred detail)

Spawn / attach / detach / reattach-after-death / kill / resurrect — the full
state model lives in the original P1 design notes. P0 needs only spawn (one
local PTY) and exit (Ctrl-C cleanly drops the PTY).

### Navigator layout — RESOLVED (Popup default; LeftPane opt-in)

Both navigator chromes ship in the binary. The selection is configurable per
launch via `--nav <left-pane|popup>` or the `CODEMUX_NAV` env var, and can be
toggled at runtime with the prefix key + `v`. Default is **Popup**.

- **Popup** (default) — full-screen focused PTY plus a one-row status bar
  listing agents and the focused index. Prefix + `w` opens a centered
  switcher (arrows + Enter, Esc to dismiss). Maximizes screen real estate
  for claude.
- **LeftPane** — always-visible 25-column navigator on the left, focused
  PTY on the right. Constant glanceability of what is running. Useful when
  watching multiple agents complete unattended work.

Both styles share the same prefix-key vocabulary (`c` spawn, `n`/`p` next/prev,
`1`-`9` focus by index, `v` toggle, `q` quit). The PTY is resized via SIGWINCH
on every layout transition so claude re-lays-out in place.

Why both stayed: after living with both prototypes, Popup felt clearly better
for focused work, but LeftPane has real value for "watch many agents" sessions.
Per the vision's "the navigator is the map" principle, neither is wrong; it
depends on what the user is doing in the moment. The toggle is one keystroke.

## Open questions

Triaged from the original brief:

- **Notification content and granularity.** P2 via AD-13; attention-needed
  events only (finished, needs-input). No notifications for running / idle /
  starting.
- **Approval-prompt visibility from the navigator.** Deferred to P2+. Likely
  just a "needs input" status dot; user focuses to see what's being asked.
- **Cross-host file-edits cache.** Accepted pain. `ssh <host> -- git diff` on
  focus change, cached per agent. If it becomes slow enough to bother,
  revisit — possibly a tiny `git-diff-watcher` helper on remote hosts.
- **Handoff / export.** Deferred. Low-frequency need; revisit if the use case
  becomes real.
- **Multi-window / detach-pane.** Deferred. TUI makes this meaningfully harder
  than the GUI path would have.
- **Phone / iPad access.** P4 maybe. Would require a control socket and a thin
  web frontend — a meaningful re-spec, not a small feature.
- **Devpod auth / host setup wizard.** Deferred. Today: manual `aifx login`
  per host. Revisit post-P3.
- **Color and accessibility.** P1 concern. Status signal uses shape + color,
  never color alone.
- **Telemetry.** None. Ever. Maybe a local `sessions-per-day` counter for
  self-curiosity in P3+. No remote reporting, ever.
