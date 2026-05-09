# Roadmap

Personal tool. No external deadline. Phases are sequenced by the pain each solves, not by calendar.

## P0 — Walking skeleton

Not user-facing. Proves the plumbing works.

**Scope**
- `ratatui` window renders
- One nested PTY running local `claude` via `portable-pty` + `tui-term`
- Keystrokes forwarded to the PTY (no prefix key yet — the whole window is the PTY)
- Ctrl-C exits cleanly, detaches the PTY

**Out of scope**: navigator, multiple agents, SSH, persistence, diff panel, anything chrome-related.

**Ship test**: I can start codemux, it shows a working `claude` session, I can interact with Claude normally. Quit, restart, it still works.

## P1 — MVP (replaces tmux for this use case)

The minimum to stop reaching for tmux.

**Scope**
- Navigator — shape (Option A vs Option B, per `docs/architecture.md`) decided during this phase
- Spawn local agents
- Spawn SSH agents via `codemuxd` (AD-3) — small per-host Rust daemon shipped with codemux, deployed on first connect
- Per-agent metadata: status dot (running / idle / needs-input / dead), pwd or repo name, host
- Keyboard cycling between agents via prefix key (AD-12)
- Per-agent scrollback (wheel + arrows / PgUp/PgDn / g/G; non-sticky exit) — see AD-25
- Persistence via `rusqlite` — agents survive app restart
- Basic process lifecycle: spawn, attach, detach, kill

**Out of scope**: diff panel, OS notifications, groups, command palette, OSC 8 fidelity.

**Ship test**: I spend a full working day in codemux and don't open tmux once.

## P2 — The actual point

The two features that make codemux worth building over "prettier tmux."

**Scope**
- Diff panel — `git diff` for the focused agent, read-only, `syntect` syntax highlighting
- "Open diff in `$EDITOR`" shortcut (AD-6 deep-review path)
- OS notifications on attention-needed events (AD-13): agent finished, agent needs input
- Needs-input state detection — PTY output heuristics; if this proves unreliable, defer to P3

**Ship test**: I stop compulsively alt-tabbing to a terminal to check "is it done yet?"

## P3 — Scale

For when 3–4 agents becomes 6–10 across multiple devpods.

**Scope**
- Groups / tags — many-to-many, user-defined, collapsible in the navigator
- Command palette — fuzzy search across agent labels, host names, recent cwds
- Navigator display modes — icon rail, full, hidden (if Option A was chosen in P1)
- Host overview screen

**Ship test**: I genuinely have a cross-host workflow that would have been impossible before — e.g., a "launch-week" group spanning one local + two devpod agents, all switched in single keystrokes.

## P4 — Maybe

Only if the need emerges. No commitment.

**Candidates**
- Phone read-only view — requires a control socket + thin web frontend. Meaningful re-spec.
- Session export / handoff — markdown transcript of a completed agent
- `mosh` as an opt-in per-host SSH transport
- Multi-window / pane detachment to another terminal

## Explicit non-milestones

Things codemux commits to NOT building, even when they seem "just one more feature":

- Review surface with annotations, send-back, staging, approvals (AD-6)
- Team features — sharing, presence, co-viewing
- Editor / IDE features — LSP, syntax search, code navigation
- Cross-tool — Codex, Cursor Agent, Gemini, anything non-Claude-Code
- Workflow enforcement — required worktrees, mandatory review-before-merge
- MCP server registry, prompt manager, "studio"
- Cloud sync of agent state
- Auth beyond the P4 phone-view re-spec
- Telemetry

If any of these start to feel tempting, re-read `docs/001--vision.md`.
