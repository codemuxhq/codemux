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

**Bounded exception (added with the status-bar segment refactor; revised
when the status-bar combined model with effort):** the
`agent_meta_worker` reads `~/.claude/settings.json` to extract the
**`model`** alias and the **`effortLevel`** field, exclusively to drive
the status bar's `ModelSegment`. The carve-out is intentionally narrow:

- One file (`~/.claude/settings.json`).
- Two fields (`model`, `effortLevel`).
- Focused agent only — we never read more than once per poll cycle.
- Local agents only in v1 — SSH-backed agents skip this entirely (the
  worker still needs the focused agent's local cwd for the branch
  read, and the local user's claude settings don't necessarily
  reflect the remote claude session's state).
- Read-only, polled at 2 s with no parsing of any other field in
  the file.

The previous incarnation of this carve-out tailed the per-session
JSONL transcript (`~/.claude/projects/<encoded-cwd>/*.jsonl`) for the
most recent assistant turn's `message.model` field. That approach
worked for the single-session case but was fragile when multiple
claude sessions shared a project directory (host TUI vs. test
codemux instance vs. subagent transcripts): the "newest jsonl by
mtime" heuristic would pick whichever session was most recently
written to, masking `/model` switches in the active agent and
occasionally returning `None` when the chosen file had no assistant
line yet. settings.json is a single-writer global file that updates
immediately on `/model`, so the bug class disappears. The tradeoff
is that model+effort are now global rather than per-agent — but
`/model` itself updates a global file, so the per-agent illusion in
claude was never really there.

Anything beyond that scope (rendering messages, tracking tool use,
parsing prompts, tailing the JSONL transcripts) requires a new AD
update. The above is the only sanctioned reason to touch Claude's
on-disk state, and it exists because there is no out-of-band channel
for "current model + effort" today (`/model` mid-session updates
in-conversation state plus the global settings.json, but there's no
event hook).

### AD-3 — Remote PTY container is `codemuxd`, behind an `AgentTransport` enum

P1 SSH transport. A small Rust daemon (`codemuxd`) holds the remote PTY across
SSH disconnects. Local codemux ships a per-target binary, deploys it on first
connect to a host, and attaches/reattaches over a unix socket.

Workspace placement: `apps/daemon/`, crate `codemux-daemon`, binary `codemuxd`
— sibling to `apps/tui/`, per AD-16's pattern.

`codemuxd` is a pure byte shuttle: PTY ownership, unix socket, signal and
resize forwarding. It knows nothing about Claude Code (AD-1 still holds — no
semantic parsing anywhere).

`crates/session` defines `AgentTransport` as an **enum**, not a trait —
variants are closed and known at compile time, and the Rust style guide
prefers enum dispatch over `Box<dyn>` for closed sets:

    enum AgentTransport {
        Local(LocalPty),
        SshDaemon(SshDaemonPty),
    }

`apps/tui` consults the transport via the enum; the runtime is
transport-agnostic.

Wire protocol: length-prefixed binary frames (PTY data is binary; JSON would
force base64 on 99% of traffic). Message types: HELLO/HELLO_ACK (with
version), PTY_DATA, RESIZE, SIGNAL, CHILD_EXITED, PING/PONG, ERROR. Strict
version negotiation — mismatch disconnects; local re-deploys the matching
daemon. No shimming. The protocol is the artifact to design carefully; the
implementation is replaceable.

Bootstrap: bundled daemon binaries for known targets; on first SSH connect,
detect target via `uname`, check `~/.cache/codemuxd/agent.version`, scp if
absent or stale. Subsequent connects are zero-cost.

Filesystem per host:
`~/.cache/codemuxd/{sockets,pids,logs}/{agent-id}.{sock,pid,log}`, sockets at
mode 0600. Single attached client per agent.

Rejected — `tmux new -A -s ccmux-<id>` (the original AD-3 sketch): wrapping a
multiplexer with a multiplexer is more than aesthetic. tmux's behavioural
surface (signal handling, terminfo, scriptable UI) is large enough that it
leaks into our error modes.

Rejected — `dtach`: small, well-understood C; would ship faster. Disqualified
by abandoned upstream (no release since 2016) — when a PTY/signal edge case
bites a load-bearing dependency, no path to a fix exists.

Rejected — multi-attach (multiple clients per agent socket): codemux is
single-user; "second observer" is not on any roadmap. Single client keeps the
daemon state model trivial.

Rejected — PTY output replay buffer in v1: reattach renders blank until the
next paint; user types a key, claude redraws. A bounded ring buffer (~256 KB)
is a tempting v1.5 if blank-screen is annoying enough — ship without first.

### AD-5 — Local codemux is a single Rust process; SSH outward, `codemuxd` inward

The local codemux binary is single-process — no client/server split, no local
daemon. All TUI, navigation, transport, and persistence live in one process.
SSH is the outbound transport for remote PTYs.

A small per-host daemon (`codemuxd`, AD-3) holds remote PTYs. The original
formulation of this AD said "no daemon on remote hosts"; that constraint is
**retired**. It was true for the P0 single-local-PTY world but became a
fiction the moment SSH transport was specified — *something* must hold the
PTY across an SSH disconnect, and Claude Code is a TTY-attached interactive
process that dies on SIGHUP. The honest framing: no codemux daemon *locally*;
one minimal codemux-owned daemon per remote host.

Naming: the daemon is `codemuxd`, not `codemux-agent`. The domain type
`Agent` already means "a Claude Code workspace"
(`crates/session/src/domain.rs`); overloading "agent" to also mean the
host-side daemon would force lifetime disambiguation in every doc, log line,
and conversation.

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

### AD-24 — Keymap registry as POD; config is a Plain Old Data structure

Key bindings are typed action enums per scope (`PrefixAction`, `PopupAction`,
`ModalAction`) plus `Bindings` POD structs that the runtime consults via
`bindings.<scope>.lookup(KeyEvent) -> Option<Action>`. This is the TEA
(Elm-style) dispatch pattern documented in the Ratatui guide: input → typed
action → state mutation, with the keymap as the single source of truth.

Configuration lives at `$XDG_CONFIG_HOME/codemux/config.toml`, falling back
to `$HOME/.config/codemux/config.toml`. XDG on every Unix, including macOS:
the `directories`/`dirs` crates default to `~/Library/Application Support/`
on macOS, which is the Apple GUI convention and the wrong place for a CLI
tool — modern CLIs (gh, git, helix, kubectl, alacritty, ripgrep) all settled
on `~/.config/` regardless of platform; we follow suit. The config is loaded
once at startup into a `Config` POD
and passed by reference into `runtime::run`. There is **no port/trait** for
config — direct quote from the architecture-guide review of this slice
(NLM, 2026-04-23): *"For a personal pre-alpha tool, reading the config at
startup and passing it as a Plain Old Data structure at construction time is
the architecturally sound choice."* A port earns its keep only when config
becomes dynamic (remote service); not now.

Per the same review: mapping a key to an action is a presentation/delivery
concern. Both `keymap` and `config` therefore live in `apps/tui/`, not in a
separate crate. Extract per AD-23's fitness functions if/when a second
delivery (e.g. a phone view) needs to share the keymap vocabulary.

Failure mode: a missing config file is fine (defaults). A present-but-invalid
config file fails loud with a readable error before the TUI starts. Per CLI
guidelines, silent fallback would be worse than refusing to start.

The help screen (default `<prefix> ?`) is generated from the same `Bindings`
POD — single source of truth for both behavior and documentation.

**Cmd / Super support via auto-detected Kitty Keyboard Protocol.** macOS
terminals swallow Cmd before any TUI can see it, unless the application
negotiates the Kitty Keyboard Protocol with the terminal first. codemux
walks every loaded `KeyChord` at startup; if any uses `KeyModifiers::SUPER`,
it pushes `KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES` after
entering raw mode and pops it in `TerminalGuard::drop`. Auto-detect rather
than a separate config flag because the Bindings *are* the source of truth:
write `prefix = "cmd+b"` and the protocol negotiation follows automatically.
Terminals that do not understand the negotiation silently ignore it; the
help screen is the user-visible escape hatch ("if my chord does not fire,
the terminal is the limit").

Rejected: a HashMap-based registry indexed by `(Scope, KeyChord)`. With ~7
entries per scope, linear search through a fixed-size array is faster than
a HashMap and the table reads as data declaration. Re-evaluate if scope
sizes grow past ~30.

Rejected: deriving `serde::Deserialize` directly on `crossterm::event::KeyEvent`.
Hand-rolled `KeyChord` parser keeps the user-facing format independent of
crossterm's internal representation (which has fields like `kind` and `state`
the user never wants to think about).

### AD-25 — Per-agent scrollback via vt100's primary-grid back buffer

The wheel scrolls the focused agent's transcript history. The
implementation rests on three observations and one deliberate trade-off.

**Observation 1: Claude Code stays on the primary screen.** Verified by
PTY-probing the initial output for the alt-screen DEC modes (`?1049h`,
`?47h`, `?1047h`) — none appear, only `?2004h` (bracketed paste) and
`?25l` (cursor hide). This matters because vt100's alternate-grid is
hardcoded to `scrollback_len: 0` and would never collect history; only
the primary grid does. We're lucky here, and the test in
`runtime::tests::scrollback_zero_len_means_no_history` guards the
contract our luck depends on.

**Observation 2: vt100 owns the offset.** When new rows evict the
top while `scrollback_offset > 0`, vt100 auto-bumps the offset by one
so the same content stays under the user's gaze. We never store an
offset in `RuntimeAgent`; we read `screen.scrollback()` and call
`screen_mut().set_scrollback(cur ± delta)` per wheel tick. Per-agent
state is implicit in each agent's `Parser`, which means switching focus
**preserves** scroll position — coming back to a scrolled-back agent
shows it where you left it. This is intentional; do not "fix" it.

**Observation 3: tui-term renders scrollback automatically.**
`PseudoTerminal::new(parser.screen())` already calls
`screen.visible_rows()` (which respects the offset) and shifts the
cursor row by `screen.scrollback()`. No render-side glue is required
beyond the floating "scroll mode" indicator badge.

**Trade-off: no PTY resize when scrolled.** A bottom-row "SCROLL"
status strip would force a PTY `SIGWINCH` on every scroll-mode
entry/exit, and Claude redrawing its full UI on every transition would
be much worse UX than the alternative. Instead the indicator is a
floating widget painted via `Clear` + `Paragraph` over the bottom-right
of the agent pane — costs ~22 cells of overlap during scroll mode,
gains zero `SIGWINCH` churn.

**Mouse capture is unconditional**, gated only on the alt-screen entry
succeeding. `EnableMouseCapture` (`?1006h` SGR mouse) is what makes
`MouseEventKind::ScrollUp/Down` reach the event loop instead of being
translated to ↑/↓ arrows by the host terminal's `alternateScroll`
behavior. Side effect: terminal-native click-and-drag selection requires
holding ⌥/Alt to bypass capture. **Apple Terminal does not deliver SGR
mouse events**; scroll won't work there. Explicit non-goal — codemux
works with iTerm2, Alacritty, Ghostty, Wezterm, Kitty.

**Selection is implemented in-app, not handed to the terminal.** Because
mouse capture eats the drag events anyway, codemux paints its own
selection overlay (reverse-video on the buffer cells in the pane rect)
and writes the extracted text to the system clipboard via OSC 52
(`\x1b]52;c;<base64>\x07`) on mouse-up. `vt100::Screen::contents_between`
does the cell-range → text conversion (it already walks `visible_rows()`
so scrollback is included automatically). The user gets modifier-free
drag-to-select that works inside any agent pane; the ⌥-bypass remains
documented as the fallback for terminals without OSC 52 (Apple Terminal,
locked-down corp environments). The selection state is per-frame and
per-focused-agent: a tab switch, agent reap, or terminal resize clears
it. Same single-pane, single-selection model as tmux's
`copy-mode-mouse`. Gestures in v1 are drag-only — no double-click word
or shift-extend; both are deferred until they're asked for.

**Scroll mode is non-sticky for typing.** Bytes that would have been
forwarded to Claude — typing real text, control sequences, anything
the dispatcher returns as `KeyDispatch::Forward` — first snap the
focused agent back to the live view (`set_scrollback(0)`), so what
you type isn't echoed into a window you can't see. **Navigation
chords preserve scroll**: pressing the prefix, a direct nav bind, or
hitting digit-1..9 in prefix mode does NOT reset the offset, so a
`Cmd-B 2` to switch tabs leaves the agent you just left exactly where
you scrolled it. `Event::Paste` snaps for the same visibility reason
as forwarded bytes.

### AD-26 — Daemon owns a session-scoped vt100 parser for snapshot replay

`codemuxd` is built around session continuity: the PTY child outlives
any single client connection, so a user can close their TUI, walk
away, and reattach later to the same Claude. The wire protocol carries
*new* PTY bytes from daemon to client — but the screen state that came
before the reattach lived only in Claude's memory and the disconnected
client's vt100 buffer. On reconnect the new client got a fresh empty
parser; an idle Claude (sitting at its prompt, no SIGWINCH because the
geometry hadn't changed) emitted nothing, and the screen stayed blank
until the user typed something that forced Claude to redraw. Same
session, different visible state — exactly what session continuity
was supposed to prevent.

The fix is structural: the daemon mirrors the child's terminal in its
own `vt100::Parser`, sized to whatever client is currently attached,
and emits `Screen::state_formatted` (clear + per-cell positioned text
+ attributes + input modes) as the **first PtyData frame** after every
handshake. The client's parser starts empty; the snapshot leaves it
in a state byte-equivalent to the daemon's. Live forwarding then
resumes from the post-snapshot moment, no gap.

Three things make this safe:

**Atomic process+send in the reader thread** (`apps/daemon/src/pty.rs`,
`spawn_reader_thread`). Each PTY chunk is fed to the parser AND pushed
to `rx` under a single parser lock acquisition. The invariant the
snapshot path relies on is "any chunk in `rx` is also in the parser,
and vice versa." Without atomicity, a chunk arriving between the
snapshot's drain and its capture would be either silently dropped (in
the parser but already drained from rx) or duplicated (in rx but not
yet in parser, so the snapshot misses it and the live forward sends
it). The daemon's PTY reader pays one mutex acquisition per 8 KiB
read, which is invisible against the syscall cost.

**Snapshot lives in `Session`, not `conn`** (`apps/daemon/src/session.rs`,
`take_snapshot`). The connection adapter (`conn::run_io_loops`) only
deals with sockets, framing, and the inbound/outbound thread scope —
it never touches `vt100`, the `?1049h` toggle, or `state_formatted`.
That domain knowledge belongs with the parser, which lives in
`Session`. `Session::attach` is the orchestrator: it calls
`conn::perform_handshake` to read the `Hello`, resizes the master to
the client's geometry, asks itself for a snapshot, writes the snapshot
frame, then hands the rest off to `conn::run_io_loops`. Keeping the
escape-sequence specifics out of `conn` avoids the leaky-abstraction
trap that an earlier draft of this fix fell into.

`Session::take_snapshot` holds the parser lock for the entire atomic
window: parser resize → drain rx → encode bytes → release. The reader
thread blocks during this; once released, any new chunks land in the
parser AND `rx` and the freshly-spawned outbound loop forwards them.
The order at the client is therefore unambiguous: snapshot first,
post-snapshot live bytes after, no overlap. The master resize sits
*outside* the parser lock — it's a `TIOCSWINSZ` ioctl independent of
the parser, and stalling the reader on it would needlessly delay
in-flight chunks.

**`?1049h` prefix when alt-screen is active** (`Session::take_snapshot`).
`Screen::state_formatted` writes the contents of the *active* screen
but doesn't toggle which screen the receiver should be on. A Claude
session in alt-screen mode would otherwise have its alt-buffer content
clear-and-painted onto the client's primary buffer, which is wrong on
attach (visible content lands on the wrong half) and worse on the
child's next mode toggle (the misplaced content lingers when the
child switches back). For primary-mode sessions we deliberately omit
the toggle — the client parser starts in primary, so a no-op switch
just adds bytes.

This **breaks AD-1's "codemux never semantically parses Claude
Code"** on the daemon side specifically, but only structurally. The
parsing is escape-sequence reproduction (cursor positions, attribute
runs, mode flags) — not interpretation of Claude's UI semantics.
There is no reading of "is this a prompt", "is this a tool call",
"what is the assistant doing"; the parser is downstream of every byte
and treats them all the same way it would any VT-compatible stream.
The carve-out exists because the wire protocol cannot transmit screen
state any other way short of keeping unbounded raw byte history per
session, which is strictly worse on memory and still wouldn't handle
parser-state things like attribute carries across line wraps.

The daemon's parser uses `scrollback_len: 0` because the TUI client
already owns the scrollback buffer (AD-25). Duplicating it on the
daemon side would double the memory footprint of every remote session
for no gain — the client only needs the visible grid restored on
reconnect; history is already in its own parser.

### AD-27 — Tab affordances on the captured mouse stream

The tab strip is the navigator's single most-used affordance, and
codemux already captures the mouse stream (AD-25, mouse section): the
runtime sees `MouseEventKind::Down/Up/Drag` instead of letting the
terminal own them. With the events already in hand, leaving them
unbound would have meant the user has to lift their hand off the
trackpad and reach for `Ctrl-B 1..9` to switch agents — for a
multiplexer whose whole job is fast switching, that is a pointless
detour.

**The two gestures.** Click on a tab focuses it (no prefix). Drag a
tab onto another's slot reorders the agents in browser-tab semantics
— `Vec::remove(from) + insert(to)`, not swap. Same gesture in both
nav styles: the bottom strip in Popup mode and the left nav rows in
LeftPane mode are both clickable.

**Hitboxes are recorded by the renderer.** A `TabHitboxes` struct
(`apps/tui/src/runtime.rs`) is owned by `event_loop`, cleared at the
top of every `render_frame`, and passed to the two leaf renderers
(`render_status_bar`, `render_left_pane`). Each renderer records a
named `Hitbox { rect, agent_id }` for every tab as it draws it. The
mouse handler reads the hitboxes back on `Down(Left)` / `Up(Left)` to
translate `(column, row)` to an agent identity (not an index).

This is the cleanest seam available: the renderer is the only place
that knows where each tab landed (after layout splits, separator
spans, hint reservation, area clipping). Recording the rect inline
during rendering avoids the duplicate-the-width-math trap that any
post-hoc geometry derivation would have fallen into.

**Press grabs a stable identity, not an index.** `mouse_press:
Option<AgentId>` stores the agent's id (not its `Vec` slot). Storing
identity means a terminal resize, agent reap, or background reorder
between Down and Up still resolves the gesture correctly: the event
loop runs `agents.iter().position(|a| a.id == id)` at the moment of
the mutation, returning `None` (and silently cancelling) if the
agent is gone. An index-based press would have silently re-targeted
to a different agent in the same slot — the kind of fragility the
identity boundary exists to prevent. The renderer/dispatcher seam is
also pure-functional: `tab_mouse_dispatch` returns
`Option<TabMouseDispatch>` (variants `PressTab(AgentId)` /
`Click(AgentId)` / `Reorder { from, to }` / `Cancel`), so every
gesture branch is unit-testable without an event-loop harness.

Release outcomes:

- same id → click → `change_focus`
- different id → drag → resolve both ids to current indices, then
  `reorder_agents` followed by `shift_index` on `focused` and
  `previous_focused` so the same agent stays focused across the
  reorder
- released outside any tab → cancel

Crossterm only fires `Drag` on motion, so a same-cell down→up is a
clean click with no intervening drag — the same code path serves
both gestures.

**Cost.** Captured clicks and drags can no longer reach the terminal
for native text selection over the tab strip. The `⌥/Option-drag`
escape hatch (iTerm2, Ghostty, Alacritty, WezTerm, Kitty) bypasses
mouse capture per-drag and is documented in the help screen alongside
the new `click` / `drag` lines. Apple Terminal does not deliver SGR
mouse anyway — neither tab gestures nor scroll work there. Same
explicit non-goal as AD-25.

### AD-28 — Wire encoder vs readline-shortcut adapter (split layers)

Translating a `KeyEvent` into the bytes a terminal-mode child expects
is two responsibilities, not one, and they live in two named
functions in `apps/tui/src/runtime.rs`:

1. **`encode_terminal_key`** — pure VT100 / ANSI key encoder. Maps
   `Backspace → DEL`, `Up → ESC[A`, `Char('a') → 'a'`, etc. The only
   modifier branching it does is `Ctrl-letter → 0x01..0x1A`, because
   that's protocol (Ctrl-C *is* 0x03), not opinion. No GUI-style
   modifier ever changes the output here. Tests pin this invariant
   explicitly: `encode_terminal_key(Backspace, SUPER)` must equal
   `encode_terminal_key(Backspace, NONE)` — both `vec![0x7f]`.
2. **`translate_readline_shortcut`** — the **deliberately
   opinionated** adapter that bridges GUI-style chords (Cmd+Backspace,
   Shift+Enter, Ctrl+Backspace, …) to readline byte sequences:
   `Cmd+Backspace → Ctrl+U` (unix-line-discard), `Ctrl/Alt+Backspace
   → Meta+DEL` (unix-word-rubout), `(Shift|Alt|Ctrl|Cmd)+Enter →
   Meta+Enter` (newline-in-input). Returns `Some(bytes)` only when the
   chord matches a registered shortcut; `None` otherwise.

`key_to_bytes` is now a one-line orchestrator: shortcut first,
encoder fallback. Wire bytes leaving the function are byte-identical
to the previous combined implementation; the change is purely
structural.

**Why split.** The architecture-guide review (NLM, 2026-04-28) flagged
the previous combined function as a Leaky Abstraction — the byte
encoder was carrying GUI-flavored opinions about what `Cmd+Backspace`
or `Shift+Enter` "should" mean, and a reader of the encoder had no
way to tell where the protocol stopped and the opinion started. The
same critique had landed earlier against the modified-Enter handler
inside the old `key_to_bytes` and was carried forward unresolved. The
split fixes both at once: the encoder is pristine and reads as
protocol; the shortcut adapter has a docstring whose first
paragraph is "this layer leaks GUI conventions onto the wire on
purpose, here's why."

**Why opinionated translation, not raw modifier passthrough.** Claude
(and every readline-style TUI input we target) speaks the universal
readline byte vocabulary — `Ctrl+U`, `Meta+DEL`, `Meta+Enter` — but
not the Kitty Keyboard Protocol's CSI-u extended encoding for
modified non-character keys. Passing `Cmd+Backspace` through
verbatim via CSI-u would land literal escape garbage in Claude's
input. So the adapter's job is precisely to translate user intent
into the bytes the child can act on; the "fidelity loss" the review
flagged is the entire point of the layer existing.

**Acceptance criteria for adding a new shortcut.** The chord must:
(a) be a recognized GUI convention with no ambiguity (Cmd+Right
for end-of-line, Cmd+A for select-all, etc.), (b) have a
well-known readline byte sequence on the receiving end, and (c)
the destination must be readline-style input — not a vt100
application that interprets the raw chord differently. Anything
violating (c) belongs in the encoder, not here.

**Why hardcode rather than route through `Bindings`.** The shortcut
layer is *protocol bridging*, not user policy. `Bindings` (AD-24)
maps key chords to application intents (`SpawnAgent`, `FocusNext`);
it is the right home for "what should codemux do when I press X".
The shortcut adapter answers "what bytes does a readline-style child
process expect when the user expresses delete-line intent" — a
separate layer with a separate vocabulary. If a future user wants to
remap `Cmd+Backspace` to send something other than `Ctrl+U`, the
shape now makes that an additive change (introduce a
`ReadlineShortcuts` POD on `Bindings`), not a teardown.

Rejected: a single combined `key_to_bytes` function with modifier
branching inline (the prior shape). Re-evaluating it would re-stage
the exact Leaky Abstraction the review fired on, twice.

### AD-29 — Status-bar segments are a closed set of built-ins, selected by config

The bottom status bar's right side renders a stack of context segments
(model · repo · branch · prefix-hint by default). Users pick which segments
to show, in what order, via `[ui] status_bar_segments = [...]` in `config.toml`.
Order is left-to-right; under width pressure, segments are dropped from the
LEFT first so the rightmost (highest-priority) one stays visible.

The plumbing lives in `apps/tui/src/status_bar/`:
- `mod.rs` — the `StatusSegment` trait, `SegmentCtx` POD, the right-to-left
  drop algorithm in `render_segments`, and the `build_segments(&ids)` registry
  that maps config strings to built-in implementations.
- `segments.rs` — the four built-in segments. Each is a stateless unit
  struct that reads its data off `SegmentCtx` and returns `Some(Line)` or
  `None` (skip silently).

**What this is not.** Not dynamic plugins. Not shell-out segments. Not
scripting. Built-in IDs only. Adding a new segment is a typed change in
this codebase: implement `StatusSegment`, add an ID constant, register
it in `build_segments`'s match arm. The config layer follows automatically.

**Why the closed set.** Same shape as `host_colors` and the fuzzy/precise
search engine: the user picks from a curated menu rather than authoring
their own. Three reasons:

1. **No subprocess on the render hot path.** A shell-out segment would
   invoke `sh -c` per refresh per segment per agent — expensive and a
   stutter risk.
2. **Typed contract.** Each segment owns its style decisions in one
   place. A typo in a built-in ID is a clippy-flagged literal mismatch;
   a typo in the config logs once at startup and falls back gracefully.
3. **Bounded surface area.** When debugging "why is this segment empty,"
   the answer is always one Rust impl in `segments.rs`. Shell-command
   segments would split the answer between codemux, the user's shell,
   and the user's PATH.

**Why the drop-from-the-left algorithm.** The user's primary cue (the
prefix-key hint) lives at the right edge today. Keeping the existing
visual anchor stable across narrow terminals — and letting newly-added
segments degrade gracefully on small screens — was the design goal.
Segments stack up to the left of the hint, and when a 60-cell terminal
can't fit `model: opus-4-7 │ repo: codemux │ codemux:main │ ctrl+b ? for help`,
the user still sees `ctrl+b ? for help` and the focused agent's tab,
not a confused half-hint.

**The model segment is special.** It triggers an AD-1 carve-out (the
`agent_meta_worker` reads `~/.claude/settings.json` to extract the
user's currently-selected model alias and effort level). See AD-1's
amended prose for the bounded-exception scope.

Rejected: hardcoded inline rendering of the three segments in
`render_status_bar` (the simplest possible thing). Would require a
config knob per segment to disable individually, and any new segment
would mean another inline branch in the renderer. The trait shape lets
the renderer stay schema-agnostic and pushes per-segment formatting
into one file per segment.

## Deferred ideas

Architecture decisions sketched in earlier drafts but not load-bearing for what
exists today. Each will return to the main body — with full prose — when its
phase ships.

- **AD-2 — One PTY per agent, recoverable via Claude session ID.** P1 reattach
  story: store Claude session ID, reattach with `claude --resume <id>` on focus
  if the PTY died.
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
- **AD-20 — Ports-and-adapters inside each component crate.** First trigger
  fired in P1: SSH transport joining local PTY transport (see AD-3). The
  Rust shape is enum dispatch (`AgentTransport` enum with one variant per
  adapter), not trait objects — closed variant set, no need for `Box<dyn>`.
  Re-introduce per component when the next real second adapter arrives.
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
