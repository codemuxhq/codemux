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
