# Architecture

## At a glance

```
┌──────────────────── codemux TUI process ─────────────────────┐
│                                                              │
│  ratatui chrome  ──────  focused-agent pane (tui-term)       │
│  (navigator, status,     ┌────────────────────────────────┐  │
│   diff panel)            │  <cell grid parsed from the    │  │
│                          │   focused PTY's VT output>     │  │
│                          └────────────────────────────────┘  │
│         │                            │                       │
│         │ rusqlite                   │ portable-pty          │
│         ▼                            ▼                       │
│    ┌──────────┐          ┌───────────────────────────┐       │
│    │ state.db │          │ PTY per agent             │       │
│    │ (hosts,  │          │  ├── local: claude        │       │
│    │  agents, │          │  └── ssh: ssh -tt <h> --  │       │
│    │  groups) │          │        tmux new -A        │       │
│    └──────────┘          │         -s ccmux-<id>     │       │
│                          │         claude --resume   │       │
│                          └───────────────────────────┘       │
└──────────────────────────────────────────────────────────────┘
```

codemux is a single Rust binary. There is no daemon on any remote host, no server, no web frontend. Remote access is via `ssh` subprocesses whose stdio is a PTY. Local access is via a directly-spawned PTY.

## Workspace layout

codemux is a Cargo workspace. One thin binary in `apps/`, three library crates in `crates/` bounded by domain concern (not by technical layer).

```
codemux/
├── Cargo.toml                       # [workspace], [workspace.dependencies], [workspace.lints]
├── apps/
│   └── tui/                         # crate: codemux-tui, binary: codemux
│       └── src/
│           ├── main.rs              # argv, tracing init, calls runtime::run()
│           ├── runtime.rs           # wires adapters into ports, event loop, navigator view model
│           └── ui/                  # ratatui chrome, term pane (tui-term), diff panel, keymap
└── crates/
    ├── session/                     # bounded context: agent lifecycle
    │   └── src/
    │       ├── lib.rs
    │       ├── domain.rs            # Agent, Host, Group, SessionState
    │       ├── use_cases.rs         # spawn / focus / detach / kill / resume
    │       ├── ports.rs             # AgentRepo, PtyTransport, NotificationSink
    │       ├── error.rs             # thiserror, #[non_exhaustive]
    │       └── infra/
    │           ├── sqlite_store.rs  # ↦ AgentRepo
    │           ├── pty_local.rs     # ↦ PtyTransport (direct fork)
    │           ├── pty_ssh.rs       # ↦ PtyTransport (ssh + tmux new -A, AD-3)
    │           └── notify_shell.rs  # ↦ NotificationSink
    ├── diff/                        # bounded context: git diff probe + open-in-editor
    │   └── src/
    │       ├── lib.rs
    │       ├── domain.rs            # DiffSnapshot, Hunk
    │       ├── use_cases.rs
    │       ├── ports.rs             # GitProbe, EditorLauncher
    │       ├── error.rs
    │       └── infra/
    │           └── git_subprocess.rs
    └── shared-kernel/               # IDs and tracing only; zero vendor deps
        └── src/lib.rs               # HostId, AgentId, GroupId
```

Allowed dependency edges:

- `session` → `shared-kernel`
- `diff` → `shared-kernel`, `session`
- `apps/tui` → `session`, `diff`, `shared-kernel`, UI libs (`ratatui`, `tui-term`, `vt100`, `syntect`)

Forbidden, enforced in CI via `cargo-deny [bans]`:

- Any `crates/*` depending on `ratatui` / `tui-term` / `vt100` / `syntect`
- Any `crates/*` depending on `apps/*`
- Any cycle between component crates

## Data model

```
Host    { id, name, kind: local|ssh, ssh_target?, last_seen }
Agent   { id, host_id, label, cwd, group_ids[], session_id?, status, last_attached_at }
Group   { id, name, color }
```

- **Host** — a machine codemux can spawn Claude Code on. `local` uses direct fork; `ssh` uses `ssh_target` as the hostname.
- **Agent** — a logical workspace. Persists across PTY deaths and app restarts. Killing the PTY does not delete the agent. Deleting the agent removes the registry entry but does not delete the underlying Claude session file on disk — it can be resurrected with `--resume`.
- **Group** — a free-form tag. Many-to-many with agents. No hierarchy. An "ungrouped" synthetic group contains agents with no tags.

Persistence: a single SQLite file via `rusqlite` at `$XDG_STATE_HOME/codemux/state.db`.

## Process lifecycle

- **Spawn.** Create an Agent record. For local: `portable-pty` spawns `claude` in the chosen cwd. For SSH: `portable-pty` spawns `ssh -tt <target> -- 'tmux new -A -s ccmux-<agent-id> -- claude'` (or `claude --resume <session-id>` if a prior session exists).
- **Attach.** When the user focuses the agent, codemux ensures its PTY is alive and renders its output via `tui-term` into the focused-pane rectangle.
- **Detach.** The user leaves the agent. The PTY stays alive; codemux stops rendering it but keeps reading its output into an off-screen buffer.
- **Reattach after PTY death.** If the PTY is dead on focus (host reboot, SSH drop), codemux uses the stored Claude session ID and re-runs the spawn command with `--resume`. The user sees a brief "resuming…" and then the restored session.
- **Kill.** Explicit user action, with confirmation. Ends the PTY, marks the agent status as `dead` in the registry. The agent record persists.
- **Resurrect.** User un-marks a dead agent, or creates a new agent pointing at the same session ID. codemux runs `claude --resume <session-id>`.

## Architecture decisions

### AD-1 — Host the PTY, do not *semantically* parse Claude Code

codemux parses the VT escape sequences of each agent's PTY via `tui-term`/`vt100` — but only to render the cell grid into a pane. It never interprets conversation state, tool calls, permission prompts, or session contents. Claude Code's UI is opaque.

Rejected: tailing `~/.claude/projects/*.jsonl` and rendering messages ourselves. Easier diff view, tighter integration — but every Claude Code release becomes a chase, and approval prompts / interactive flows are a nightmare to reimplement.

Consequence: whatever Claude Code can do in a terminal, codemux supports for free. Whatever codemux wants to *show beside* Claude Code, it derives from out-of-band sources (git for diffs, host probes for liveness).

### AD-2 — One PTY per agent, recoverable via Claude session ID

Each agent's PTY is the canonical process. If the PTY dies, codemux stores the Claude session ID and reattaches with `claude --resume <id>` on next focus.

Rejected: always-on background processes per agent. Higher resource cost, harder lifecycle. `--resume` already delivers near-instant reattach.

### AD-3 — Wrap remote `claude` in `tmux new -A`

For SSH agents, the remote command is:

```
ssh -tt <host> -- 'tmux new -A -s ccmux-<agent-id> -- claude --resume <session-id>'
```

`tmux new -A -s name` attaches if the named session exists, creates it otherwise. A dropped SSH connection leaves Claude Code running on the remote, ready to be reattached. No tmux UI is ever shown to the user — it's purely a keep-alive layer.

Rejected: bare `ssh -tt <host> claude`. A network blip kills the agent.

Rejected: `mosh`. Lovely for laggy networks, but adds a per-host dependency and breaks symmetry with local agents. Could come back later as an opt-in per-host transport.

### AD-4 — ~~Tauri shell + web frontend~~ — DELETED

Original brief proposed Tauri + React + xterm.js. Superseded by the TUI pivot (AD-11). See `docs/vision.md` for the rationale.

### AD-5 — Single Rust process, SSH outward to remote hosts

codemux is one binary. For local agents, it spawns PTYs directly. For remote agents, it spawns `ssh` subprocesses whose stdio is a PTY. No daemon on any remote host.

Rejected: a daemon binary on every host. More uniform control plane, but an install/upgrade tax that isn't worth it. SSH already provides everything needed: remote shell, PTY, file presence. Reach for daemons only when the model outgrows this.

### AD-6 — Edits panel is `git diff`, read-only

For the focused agent, the diff panel runs `git -C <agent-cwd> diff` (or `diff HEAD`), re-run on a debounce when the PTY emits output that looks like a tool finished. For remote agents: `ssh <host> -- git -C <cwd> diff`.

Two depths:

- **Peek**: the diff is inlined into the TUI panel with `syntect`-based syntax highlighting.
- **Deep review**: a keybind opens the diff in `$EDITOR`. Deep review happens there, not in codemux.

Rejected: staging, commit, annotations, "approve this edit" flows. codemux is not a code-review tool (see `docs/vision.md` non-goals).

Rejected: filesystem watchers (`inotify` / `fsnotify`) for true real-time. Requires per-host helpers; gains sub-second freshness that isn't needed.

### AD-7 — State is a single SQLite file via `rusqlite`

One `state.db` in `$XDG_STATE_HOME/codemux/`. Tables: `host`, `agent`, `group`, `agent_group`. Small, transactional, queryable, inspectable via `sqlite3` CLI, 25+ year file-format stability.

Pinned: `rusqlite =0.39.0`. Schema migrations via `rusqlite_migration`.

Rejected: JSON. Defensible at current scale (single-user, dozens of agents), but the first query beyond filter/sort becomes awkward — e.g., "agents in group X on host Y that touched a file matching Z."

Rejected: `sled` (abandoned upstream), `redb` (credible, but SQLite's inspectability wins), `sqlx` (overkill — forces tokio async everywhere).

### AD-8 — No auth — moot in TUI mode

The TUI process binds to nothing. No network surface, no sockets. When phone view (P4) arrives, a control socket or thin web frontend will need auth — defer that decision until then.

### AD-9 — ~~React + Vite + xterm.js~~ — DELETED

Superseded by AD-11.

### AD-10 — PTY library: `portable-pty`

Pure-Rust PTY spawning, cross-platform, works for both local fork and `ssh` subprocesses.

Pinned: `portable-pty` at the latest stable at implementation time (revise in P0 based on API stability).

### AD-11 — TUI stack: `ratatui` + `tui-term` + `vt100`

- **`ratatui`** for chrome (navigator, status bar, diff panel, new-agent sheet).
- **`tui-term`** widget for nested PTY rendering — drops into a ratatui `Rect` with zero glue.
- **`vt100`** underneath `tui-term` for VT parsing.

Pinned: `tui-term =0.3.4`, `vt100 =0.16.2`, `ratatui` at latest stable at implementation time.

Fallback: **`alacritty_terminal`** — if OSC 8 (hyperlinks), full SGR mouse modes, or alt-screen edge cases start to bite, swap the terminal backend. Contained refactor, not an architectural change. Direct precedent: `egui_term`, `gpui-terminal`, and `missiond-core` all chose `alacritty_terminal` for fidelity.

Rejected: rolling our own on top of `vte` (zellij's path). Most powerful, most work. Not worth it for a personal tool when `tui-term` exists.

Rejected: `wezterm-term` — not cleanly available on crates.io as a standalone crate.

Rejected: in-process rendering of Claude Code via JSONL tailing or protocol-aware chrome. See AD-1.

### AD-12 — Nested-PTY input routing via prefix key

Tmux-style: a prefix key (default `C-b`, configurable) indicates "the next keystroke is a codemux command." Without the prefix, keystrokes are forwarded verbatim to the focused agent's PTY.

Rationale: raw-key shortcuts (e.g., binding `C-j` directly) leak into child apps unpredictably. Prefix-keying is the proven-boring solution.

### AD-13 — OS notifications via platform helper shell-out

For P2 attention-needed events (agent finished, agent needs input), codemux shells out to a platform helper:

- macOS: `osascript -e 'display notification "..."'`
- Linux: `notify-send`

Rejected: linking platform-specific notification crates. Per-platform compile paths, more deps. The shell-out is small and easy to make conditional on `uname`.

### AD-14 — Cargo workspace with apps/crates split

codemux is delivered as a single binary but organized as a Cargo workspace with `apps/` (driving adapters) and `crates/` (libraries) directories. The binary at `apps/tui/` is thin: argv parsing, tracing init, and delegation to a library's `run()`. All meaningful logic lives in `crates/`.

Rejected: single-crate `src/main.rs` + `src/lib.rs`. Works today, but every future split pays extraction cost later. Workspace-first is nearly free now and preserves that optionality — including the eventual ability to add a second delivery (P4 phone view) without restructuring.

### AD-15 — Package by Component, not by technical layer

Crates and modules are bounded by domain concern (`session`, `diff`), not by technical layer (`state-store`, `tui-chrome`, `pty-host`). A component crate owns its domain types, use cases, ports, *and* its adapters.

Rejected: horizontal layering into `domain`, `state-store`, `pty-host`, `tui-chrome`, `notify`, `app-shell` crates. This is the Lasagna Architecture anti-pattern — small updates reverberate through every layer, proxy methods accumulate at boundaries, and the source tree tells you nothing about what the application *does*. Screaming Architecture wins: top-level folders under `crates/` name bounded contexts.

### AD-16 — TUI binary at apps/tui/, crate codemux-tui, binary codemux

Directory name describes the delivery shape, not the product. Cargo separates package name from binary name:

```toml
[package]
name = "codemux-tui"

[[bin]]
name = "codemux"
path = "src/main.rs"
```

A future P4 phone-view delivery becomes `apps/phone-view/` without renaming anything. Users still type `codemux` because that is the product.

### AD-17 — Per-component error types via thiserror

Each library crate defines its own `thiserror` enum, marked `#[non_exhaustive]`. The binary uses `color-eyre` (or `anyhow`) at the edge to wrap library errors for human-readable reporting.

Rejected: a shared `Error` enum for the whole workspace. Ball-of-Mud trap — every crate must know every other crate's failure shape, and the enum grows until abstraction layers collapse. Per-crate errors keep each bounded context's failure vocabulary contained; the binary is the only place that needs to talk in "any error".

### AD-18 — Infrastructure adapters are co-located with their component

The infra module (`session/src/infra/`, `diff/src/infra/`) lives inside the crate that defines its ports. No separate `crates/infra/` crate.

Rationale: cohesion. SQLite is how *session* persists; it is not a generic infrastructure concern. Keeping adapters with the component that owns their ports makes the component's full story — domain, use cases, ports, real-world wiring — live in one directory.

Revisit: if a second binary ships that needs the session domain without PTY or SQLite, the adapters hide behind a Cargo feature (`adapters = [...]`). Not needed today.

### AD-19 — shared-kernel carries IDs and tracing only

The `shared-kernel` crate holds cross-cutting primitives every component needs: `HostId`, `AgentId`, `GroupId` newtypes, plus tracing-subscriber init. Zero vendor deps, zero error types, zero business logic.

Per DDD, the shared kernel must be small and stable — changes to it ripple to every component. Anything domain-shaped that drifts into shared-kernel is mis-filed; it belongs in a specific component.

### AD-20 — Ports-and-adapters inside each component crate

Within a component crate:

- `domain.rs` — pure types, zero vendor deps
- `ports.rs` — traits (`AgentRepo`, `PtyTransport`, `GitProbe`, …) that use cases depend on
- `use_cases.rs` — logic parameterized over the port traits
- `infra/` — trait implementations using real tools

The binary (`apps/tui/runtime.rs`) is the only place where concrete adapters are instantiated and injected into use cases. Tests substitute in-memory adapters without touching filesystem, database, or network. Adapter swaps (e.g., the `alacritty_terminal` fallback in AD-11) become a single-file change.

### AD-21 — Workspace-wide dependencies and lints

Shared dependencies are pinned exactly in `[workspace.dependencies]` at the root. Shared lints are declared in `[workspace.lints]`. Rust edition 2024, Cargo resolver 3.

```toml
[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = { level = "deny", priority = -1 }
unwrap_used = "deny"
expect_used = "deny"
```

Member crates inherit with `{ workspace = true }` on deps and `[lints] workspace = true` on lints.

Rejected: `cargo-hakari`. Needed only in very large workspaces; edition 2024 + resolver 3 handle feature unification natively.

### AD-22 — Navigator is a view model in apps/tui, not its own crate

The navigator's *data* (agents with status, pwd, host) is owned by the `session` component. The navigator's *UI state* (selection index, filter string, collapse state, display mode) is presentation and lives in `apps/tui/runtime.rs` as a view model.

Navigator state has no meaning outside the TUI delivery. Extracting it into its own crate would invent a bounded-context split where none exists. A future phone-view delivery would build its own view model over the same `session` read model — not reuse this one.

### AD-23 — Fitness functions for extracting further crates

The three-crate layout is the starting point, not the endpoint. A new crate is extracted when, and only when, one of these signals fires:

1. **Compile pain.** Full workspace `cargo test` on warm cache exceeds ~15s. Extract the heaviest-deps component.
2. **Feature isolation pain.** A component needs different feature flags on a shared dep and hits the unification wall.
3. **Generic subdomain.** A component becomes Claude-Code-agnostic and is a credible OSS publication candidate (e.g., the PTY transport).
4. **Second binary.** A delivery like the P4 phone view needs the session domain without the TUI adapters.

Splitting before a signal fires is the anti-pattern. For a solo, single-binary, pre-alpha tool, three component crates is the right granularity.

### Navigator layout — OPEN

Two candidates on the table, decided during P1 when there's a running prototype to compare:

- **Option A** — two-pane, left navigator with expand-on-focus per row, right is the focused PTY. Arrows navigate the list; focused row expands inline with pwd, host, activity, diff summary. `Tab` or prefix-binding pushes focus into the PTY; a return binding pulls focus back.
- **Option B** — full-screen PTY + popup switcher invoked via prefix key. Zero permanent chrome except a 1-row status bar. Arrows in popup, Enter focuses, Esc dismisses.

Principle 3 (the navigator is the map) holds in either case.

## Open questions

Triaged from the original brief:

- **Notification content and granularity.** Resolved — P2 via AD-13; attention-needed events only (finished, needs-input). No notifications for running/idle/starting.
- **Approval-prompt visibility from the navigator.** Deferred to P2+. Likely just a "needs input" status dot; user focuses to see what's being asked.
- **Cross-host file-edits cache.** Accepted pain. `ssh <host> -- git diff` on focus change, cached per agent. If it becomes slow enough to bother, revisit — possibly a tiny `git-diff-watcher` helper on remote hosts.
- **Handoff / export.** Deferred. Low-frequency need; revisit if the use case becomes real.
- **Multi-window / detach-pane.** Deferred. TUI makes this meaningfully harder than the GUI path would have.
- **Phone / iPad access.** P4 maybe. Would require a control socket and a thin web frontend — a meaningful re-spec, not a small feature.
- **Devpod auth / host setup wizard.** Deferred. Today: manual `aifx login` per host. Revisit post-P3.
- **Color and accessibility.** P1 concern. Status signal uses shape + color, never color alone.
- **Telemetry.** None. Ever. Maybe a local `sessions-per-day` counter for self-curiosity in P3+. No remote reporting, ever.
