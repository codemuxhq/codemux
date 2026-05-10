# Architecture

This document is the canonical map of how codemux is built. The first half
describes the system as it exists today: workspace layout, dependency rules,
data model, runtime flow. The second half is the Architecture Decision
Records, the durable rationale for *why* it is built this way, with rejected
alternatives kept in writing so we don't relitigate them.

If you only have two minutes, read [At a glance](#at-a-glance) and skim the
ADR titles in the table at the start of [Architecture Decision Records](#architecture-decision-records).

---

## At a glance

```
┌──────────────────── codemux TUI process (apps/tui) ─────────────────────┐
│                                                                         │
│  ratatui chrome ─── focused-agent pane ─── status bar segments          │
│  (tabs, popups)    (tui-term + vt100)     (model · branch · prefix-hint)│
│         │                  ▲                          ▲                 │
│         │                  │                          │                 │
│         │           ┌──────┴───────┐                  │                 │
│         │           │ vt100 cell   │     off-thread workers             │
│         │           │ grid (per    │     (agent_meta_worker reads       │
│         │           │ agent)       │      ~/.claude/settings.json +     │
│         │           └──────┬───────┘      per-agent statusline JSON;    │
│         ▼                  │              git_branch, log_tail, …)      │
│  event loop / key dispatch │                                            │
│  (apps/tui/src/runtime.rs) │                                            │
│         │                  │                                            │
│         ▼                  ▼                                            │
│  AgentTransport  ──────────────────────────────────► closed enum, two   │
│  (crates/session)                                    variants:          │
│         │                                                               │
│         ├── Local(LocalPty)  ───► portable-pty ───► claude (child)      │
│         │                                                               │
│         └── SshDaemon(SshDaemonPty)                                     │
│                   │                                                     │
│                   │ wire frames over an ssh -L unix tunnel              │
│                   ▼                                                     │
│      ┌────────────────────────────────────────────────────────────┐     │
│      │ codemuxd (apps/daemon) on the remote host                  │     │
│      │   • unix socket  ~/.cache/codemuxd/sockets/<agent>.sock    │     │
│      │   • reader thread: atomic process+send under one lock      │     │
│      │   • vt100::Parser mirrors the screen for replay-on-attach  │     │
│      │   • PTY child ───► claude (remote)                         │     │
│      └────────────────────────────────────────────────────────────┘     │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

codemux is a single-user Rust workspace shipping two binaries: `codemux` (the
TUI you run on your laptop) and `codemuxd` (a per-host daemon that holds
remote PTYs across SSH disconnects). Local agents go through a `portable-pty`
child rendered via `vt100` + `tui-term` directly inside the TUI. Remote agents
go through `codemuxd`, which mirrors the child's screen in its own `vt100`
parser so reattaching after a disconnect replays the current frame instead
of waiting for Claude to redraw. There is no central server, no daemon
running on your laptop, and no semantic parsing of Claude Code anywhere
([AD-1](#ad-1--host-the-pty-do-not-semantically-parse-claude-code)).

---

## Workspace layout

Cargo workspace, edition 2024, resolver 3.

```
codemux/
├── Cargo.toml                       # [workspace], deps, lints
├── apps/
│   ├── tui/                         # crate: codemux-tui, binary: codemux
│   │   └── src/
│   │       ├── main.rs              # argv, tracing init, runtime::run
│   │       ├── runtime.rs           # event loop, key dispatch, render
│   │       ├── keymap.rs            # Bindings POD, Action enums (AD-24)
│   │       ├── config.rs            # TOML loader, validation
│   │       ├── spawn.rs             # spawn modal (host/path zones)
│   │       ├── status_bar/          # closed-set segments (AD-29)
│   │       ├── agent_meta_worker.rs # AD-1 carve-out: model + tokens
│   │       ├── statusline_ipc.rs    # AD-1 carve-out: statusline tee
│   │       ├── index_*.rs           # session-long fuzzy directory index
│   │       ├── fuzzy_worker.rs      # nucleo-matcher scoring thread
│   │       └── …                    # smaller workers (git_branch,
│   │                                #  url_scan, toast, log_tail, …)
│   └── daemon/                      # crate: codemux-daemon, binary: codemuxd
│       └── src/
│           ├── main.rs              # thin shell: parse cli, call lib
│           ├── lib.rs               # supervisor entry (in-process testable)
│           ├── supervisor.rs        # accept loop, single client at a time
│           ├── session.rs           # vt100 mirror, take_snapshot replay
│           ├── pty.rs               # PTY reader thread (atomic process+send)
│           ├── conn.rs              # framing, handshake, run_io_loops
│           ├── bootstrap.rs         # validate cwd, lock pid, bind socket
│           └── fs_layout.rs         # paths under ~/.cache/codemuxd/
└── crates/
    ├── session/                     # bounded context: agent lifecycle
    │   └── src/
    │       ├── domain.rs            # Agent, Host, AgentStatus
    │       ├── transport.rs         # AgentTransport enum (AD-3, AD-20)
    │       └── error.rs             # thiserror, #[non_exhaustive]
    ├── shared-kernel/               # IDs only; zero vendor deps (AD-19)
    │   └── src/lib.rs               # HostId, AgentId, GroupId
    ├── wire/                        # protocol message types (AD-3)
    │   └── src/                     # depends only on thiserror
    └── codemuxd-bootstrap/          # SSH adapter
        ├── build.rs                 # assembles daemon tarball at compile
        └── src/                     # prepare_remote + attach_agent
```

The split is deliberate: `apps/` holds delivery-shaped code (TUI, daemon),
`crates/` holds bounded contexts and pure-data crates that the apps compose.
See [AD-15](#ad-15--package-by-component-not-by-technical-layer) for the
"package by component, not by layer" rationale and
[AD-14](#ad-14--cargo-workspace-with-apps-and-crates-split) /
[AD-16](#ad-16--tui-binary-at-appstui-daemon-binary-at-appsdaemon) for the
top-level shape.

## Dependency rules

Allowed edges:

- `apps/tui  →  session, shared-kernel, codemuxd-bootstrap, ratatui, tui-term, crossterm`
- `apps/daemon  →  wire, portable-pty, vt100`  *(see carve-out below)*
- `codemuxd-bootstrap  →  session, wire`
- `session  →  shared-kernel`
- `wire  →  shared-kernel`  *(IDs only)*

Forbidden:

- Any `crates/*` depending on a TUI rendering crate (`ratatui`, `tui-term`,
  `crossterm`).
- Any `crates/*` depending on `apps/*`.
- Any cycle between component crates.

**Carve-out: `apps/daemon` depends on `vt100`.** `vt100` is technically a
TUI-adjacent dep, but the daemon is not a TUI. It's a byte shuttle that
needs a pure VT parser to mirror the child's screen for replay-on-attach
([AD-26](#ad-26--daemon-owns-a-session-scoped-vt100-parser-for-snapshot-replay)).
This is the only sanctioned exception to "no TUI deps outside `apps/tui`".

## Data model

```
Host    { id, name, kind: local|ssh, ssh_target?, last_seen }
Agent   { id, host_id, label, cwd, group_ids[], session_id?,
          status, last_attached_at }
Group   { id, name, color }                  # tagging (deferred, AD-?)

AgentTransport ::= Local(LocalPty) | SshDaemon(SshDaemonPty)
                                              # closed enum, AD-3 + AD-20

AgentStatus ::= Starting | Running | Idle | NeedsInput | Dead
```

- **Host**: a machine codemux can spawn Claude Code on. `local` uses direct
  fork. `ssh` uses `ssh_target` to reach the remote `codemuxd`.
- **Agent**: a logical workspace. Persists across PTY deaths and app restarts
  once persistence lands ([AD-7](#ad-7--state-is-a-single-sqlite-file-via-rusqlite),
  deferred). Killing a PTY does not delete the agent.
- **AgentTransport**: closed sum type the runtime uses to drive a PTY
  without knowing whether it's local or remote-via-daemon. New transports
  are an enum-variant change, not a trait-object explosion.
- **IDs**: `HostId`, `AgentId`, `GroupId` are `Arc<str>` newtypes in
  `shared-kernel`, cheap to clone through channels.

## Runtime architecture

### The frame loop

Everything in the TUI hangs off a single event loop in
`apps/tui/src/runtime.rs::event_loop`. One iteration is roughly 50 ms:

1. Drain `crossterm` events into a buffer (key, mouse, paste, resize).
2. Reap dead transports (`AgentTransport::try_wait` returning `Some(code)`).
3. Apply pending state transitions (in-flight SSH bootstrap progress,
   focus changes, popup state).
4. Call `render_frame`. This is where tab hitboxes are recorded inline
   by the renderer for later mouse resolution
   ([AD-27](#ad-27--tab-affordances-on-the-captured-mouse-stream)).
5. Dispatch each event from the buffer.
6. Sleep until the next tick.

There is no async runtime. The event loop is synchronous; off-thread work
(PTY readers, fuzzy index, agent_meta_worker) communicates back via
crossbeam channels which the loop drains once per tick.

### Key dispatch

A `KeyEvent` becomes wire bytes through three named layers:

```
KeyEvent ──► dispatch_key ──► either an Action (consumed by codemux)
                              or  Forward(bytes) ──► key_to_bytes ──► transport.write
                                                           │
                                                           ├─ translate_readline_shortcut  (opinionated GUI-chord adapter, AD-28)
                                                           └─ encode_terminal_key          (pure VT100/ANSI encoder, AD-28)
```

`dispatch_key` consults a two-state prefix machine (`Idle`, `AwaitingCommand`)
backed by a `Bindings` POD loaded from `config.toml`. Direct binds win first;
the prefix is sticky for nav moves so a single `Ctrl-B` lets you cycle focus
without re-pressing it. The same `Bindings` table generates the help screen
and the prefix-hint segment, a single source of truth
([AD-24](#ad-24--keymap-registry-as-pod-config-is-a-plain-old-data-structure)).

### Local agent output path

```
claude (child) ─► PTY master ─► reader ─► vt100::Parser ─► tui-term widget
                  (portable-pty)                            (rendered into a
                                                             ratatui Rect)
```

Per-agent scrollback lives entirely inside each agent's `vt100::Parser`
primary-grid back buffer; the wheel scrolls by adjusting that parser's
offset. No render-side glue, no mid-process resize on scroll-mode
entry/exit ([AD-25](#ad-25--per-agent-scrollback-via-vt100s-primary-grid-back-buffer)).

### Remote agent output path

```
claude (remote) ─► PTY master ─► daemon reader ──┬─► vt100::Parser (mirror, for snapshot)
                                                 └─► crossbeam channel ─► outbound thread
                                                                          │
                                                                          │ wire frames
                                                                          ▼
                                                                ssh -L unix tunnel
                                                                          │
                                                                          ▼
                                                            TUI conn / inbound thread
                                                                          │
                                                                          ▼
                                                          TUI vt100::Parser ─► tui-term
```

The daemon's reader feeds each PTY chunk to the parser AND the outbound
channel under one lock acquisition (atomic process+send). On reattach,
`Session::take_snapshot` drains the channel under the same lock, encodes
the mirrored screen via `vt100::Screen::state_formatted`, and emits the
result as the **first `PtyData` frame** after the handshake, so the
client's parser ends up byte-equivalent to the daemon's
([AD-26](#ad-26--daemon-owns-a-session-scoped-vt100-parser-for-snapshot-replay)).

### Spawning a remote agent

The SSH path is split into two phases so the spawn modal can pause for
folder selection between them:

1. **`prepare_remote`**: probe the remote `codemuxd` version; if missing
   or stale, scp the embedded tarball and remote-build. The tarball is
   assembled at compile time by `crates/codemuxd-bootstrap/build.rs` from
   `apps/daemon` + `crates/wire` + workspace files.
2. **`attach_agent`**: spawn the daemon, open an `ssh -L` unix-socket
   tunnel, do the wire `Hello`/`HelloAck` handshake, and return an
   `AgentTransport::SshDaemon` to the runtime.

See [AD-3](#ad-3--remote-pty-container-is-codemuxd-behind-an-agenttransport-enum)
for the full daemon design and [AD-5](#ad-5--local-codemux-is-a-single-rust-process-ssh-outward-codemuxd-inward)
for the "no local daemon, one minimal remote daemon" framing.

---

## Architecture Decision Records

| #  | Title                                                              | Status                              |
|----|--------------------------------------------------------------------|-------------------------------------|
| 1  | Host the PTY, do not semantically parse Claude Code                | Accepted (amended)                  |
| 2  | Reattach a dead PTY via Claude session ID                          | Deferred (P1)                       |
| 3  | Remote PTY container is `codemuxd`, behind an `AgentTransport` enum| Accepted                            |
| 5  | Local codemux is a single Rust process; SSH outward, daemon inward | Accepted (amended)                  |
| 6  | Edits panel is `git diff`, read-only                               | Deferred (P2)                       |
| 7  | State is a single SQLite file via `rusqlite`                       | Deferred (P1)                       |
| 8  | No auth                                                            | Accepted (policy)                   |
| 10 | PTY library: `portable-pty`                                        | Accepted                            |
| 11 | TUI stack: `ratatui` + `tui-term` + `vt100`                        | Accepted                            |
| 12 | Nested-PTY input routing via prefix key                            | Superseded by AD-24                 |
| 13 | OS notifications via platform helper shell-out                     | Deferred (P2)                       |
| 14 | Cargo workspace with `apps/` and `crates/` split                   | Accepted                            |
| 15 | Package by component, not by technical layer                       | Accepted                            |
| 16 | TUI binary at `apps/tui/`, daemon binary at `apps/daemon/`         | Accepted                            |
| 17 | Per-component error types via `thiserror`                          | Accepted                            |
| 18 | Infrastructure adapters co-located with their component            | Accepted (policy)                   |
| 19 | `shared-kernel` carries IDs only                                   | Accepted                            |
| 20 | Ports-and-adapters via enum dispatch inside each component         | Accepted                            |
| 21 | Workspace-wide dependencies and lints                              | Accepted                            |
| 22 | Navigator is a view model in `apps/tui`, not its own crate         | Accepted                            |
| 23 | Fitness functions for extracting further crates                    | Accepted (policy)                   |
| 24 | Keymap registry as POD; config is a Plain Old Data structure       | Accepted                            |
| 25 | Per-agent scrollback via vt100's primary-grid back buffer          | Accepted                            |
| 26 | Daemon owns a session-scoped vt100 parser for snapshot replay      | Accepted                            |
| 27 | Tab affordances on the captured mouse stream                       | Accepted                            |
| 28 | Wire encoder vs readline-shortcut adapter (split layers)           | Accepted                            |
| 29 | Status-bar segments are a closed set of built-ins, selected by config | Accepted                         |

Numbering gaps (4, 9) are intentional. Those slots were used for ideas
that were absorbed into other ADs before being written up. Renumbering
would break links from code comments and commit messages.

ADRs are written in a fixed shape:

- **Status**: `Accepted`, `Deferred (Phase X)`, or `Superseded by AD-N`. Add
  amendment dates inline when the body has been revised.
- **Context**: what problem we are solving and why now.
- **Decision**: what we chose, with enough specificity that someone reading
  only this entry can implement it.
- **Consequences**: what this commits us to. Both good and bad.
- **Rejected alternatives**: paths considered and why we did not take them.
  Kept in writing so we don't relitigate.
- **Carve-outs**: explicit exceptions that survive the rule, only present
  where they apply.

---

### AD-1 — Host the PTY, do not semantically parse Claude Code

**Status:** Accepted (2026-01), amended 2026-04 (settings.json carve-out for
`agent_meta_worker`), amended 2026-04 (statusline IPC carve-out for token
usage).

**Context.** codemux needs to *display* Claude Code's UI without understanding
it. Claude Code's terminal UI changes frequently: approval prompts,
permission flows, tool call rendering, status badges. A multiplexer that
re-parses any of that becomes a chase that loses every release cycle.

**Decision.** codemux parses VT escape sequences via `tui-term` / `vt100`,
but only to render the cell grid into a pane. It never interprets
conversation state, tool calls, permission prompts, or session contents.
Claude Code's UI is opaque to codemux.

**Consequences.** Whatever Claude Code can do in a terminal, codemux supports
for free. Whatever codemux wants to *show beside* Claude Code, it must derive
from out-of-band sources: git for diffs, host probes for liveness, the
filesystem for model/effort, never the JSONL transcripts.

**Rejected alternatives.**
- *Tail `~/.claude/projects/*.jsonl` and render messages ourselves.* Easier
  diff view, tighter integration. Disqualified: every Claude Code release
  becomes a chase, and approval prompts / interactive flows are a nightmare
  to reimplement.

**Carve-outs.**

There are exactly two sanctioned reads of Claude's on-disk state, and they
exist because no out-of-band channel exists for the data they need:

1. **`agent_meta_worker` reads `~/.claude/settings.json`.** Two fields
   (`model`, `effortLevel`), focused agent only, polled at 2 s, local
   agents only in v1, read-only, no parsing of any other field. The
   previous incarnation tailed the per-session JSONL transcript for the
   most recent assistant turn's `message.model` field; that worked for
   the single-session case but was fragile when multiple Claude sessions
   shared a project directory (the "newest jsonl by mtime" heuristic
   would pick whichever session was most recently written, masking
   `/model` switches in the active agent). `settings.json` is a
   single-writer global file that updates immediately on `/model`, so
   the bug class disappears. The trade-off is that model+effort are now
   global rather than per-agent, but `/model` itself updates a global
   file, so the per-agent illusion was never really there.
2. **`statusline_ipc` consumes the per-agent statusline JSON snapshot.**
   We inject `statusLine.command` into the spawned Claude session's
   `--settings`, pointing at `codemux statusline-tee`. Claude writes a
   snapshot JSON each turn; the `tee` subcommand persists the snapshot
   to `~/.cache/codemux/statusline/<agent>.json` and re-emits Claude's
   line on stdout untouched. The TUI reads the JSON for token-usage
   data. Same architectural footing as the `settings.json` read: read
   our own files, never re-parse Claude's wire output.

Anything beyond this scope (rendering messages, tracking tool use, parsing
prompts, tailing JSONL transcripts) requires a new AD.

---

### AD-2 — Reattach a dead PTY via Claude session ID

**Status:** Deferred (P1).

**Context.** When a PTY dies (claude crashes, machine sleeps, network
flakes for a remote agent), codemux should be able to bring it back without
losing conversation context.

**Decision (sketch).** Persist the Claude session ID with the agent. On
focus, if the PTY is `Dead`, spawn `claude --resume <id>` instead of a
fresh process.

**Consequences.** Agents become recoverable even after a hard kill. Pairs
naturally with [AD-7](#ad-7--state-is-a-single-sqlite-file-via-rusqlite).

**Rejected alternatives.**
- *Treat dead PTYs as terminal.* User loses context for transient failures.
  Not acceptable.

---

### AD-3 — Remote PTY container is `codemuxd`, behind an `AgentTransport` enum

**Status:** Accepted.

**Context.** Claude Code is a TTY-attached interactive process that dies on
SIGHUP. The moment we want SSH transport, *something* must hold the PTY
across SSH disconnects. The original P0 framing of "no daemon anywhere"
became a fiction the moment SSH transport was on the roadmap.

**Decision.** A small Rust daemon, `codemuxd`, runs on each remote host and
holds remote PTYs. The local codemux ships a per-target binary, deploys it
on first connect, and attaches/reattaches over a unix socket forwarded
through `ssh -L`.

`codemuxd` is a pure byte shuttle: PTY ownership, unix socket, signal and
resize forwarding, and a `vt100` mirror for replay-on-attach
([AD-26](#ad-26--daemon-owns-a-session-scoped-vt100-parser-for-snapshot-replay)).
It knows nothing about Claude Code (AD-1 still holds, no semantic parsing).

`crates/session` defines `AgentTransport` as an **enum**, not a trait.
Variants are closed and known at compile time:

```rust
#[non_exhaustive]
enum AgentTransport {
    Local(LocalPty),
    SshDaemon(SshDaemonPty),
}
```

`apps/tui` consults the transport via the enum; the runtime is
transport-agnostic. See [AD-20](#ad-20--ports-and-adapters-via-enum-dispatch-inside-each-component)
for the enum-vs-`Box<dyn>` rationale.

**Wire protocol.** Length-prefixed binary frames (PTY data is binary; JSON
would force base64 on 99% of traffic). Message types: `Hello`/`HelloAck`
(with version), `PtyData`, `Resize`, `Signal`, `ChildExited`, `Ping`/`Pong`,
`Error`. Strict version negotiation: mismatch disconnects, local re-deploys
the matching daemon, no shimming. Protocol is the artifact to design
carefully; the implementation is replaceable.

**Bootstrap.** Bundled daemon binaries for known targets. On first SSH
connect: detect target via `uname`, check `~/.cache/codemuxd/agent.version`,
scp + build from the embedded tarball if absent or stale. Subsequent
connects are zero-cost. The bootstrap is split into `prepare_remote`
(stages 1-4) and `attach_agent` (stages 5-7) so the spawn modal can pause
for folder selection between them.

**Filesystem per host.**
`~/.cache/codemuxd/{sockets,pids,logs}/{agent-id}.{sock,pid,log}`,
sockets at mode `0600`. Single attached client per agent.

**Consequences.** Reattach across SSH disconnects works. The protocol is a
load-bearing contract that has to survive version skew. We pay the cost of
shipping and version-negotiating a per-target binary on first connect.

**Rejected alternatives.**
- *`tmux new -A -s ccmux-<id>`* (the original AD-3 sketch). Wrapping a
  multiplexer with a multiplexer is more than aesthetic. tmux's behavioural
  surface (signal handling, terminfo, scriptable UI) leaks into our error
  modes.
- *`dtach`.* Small, well-understood C; would ship faster. Disqualified by
  abandoned upstream (no release since 2016): when a PTY/signal edge case
  bites a load-bearing dependency, no path to a fix exists.
- *Multi-attach (multiple clients per agent socket).* codemux is single-user;
  "second observer" is not on the roadmap. Single client keeps the daemon
  state model trivial.
- *PTY output replay buffer in v1.* Reattach renders blank until the next
  paint; user types a key, claude redraws. Superseded by AD-26's vt100
  snapshot, which is strictly better.

---

### AD-5 — Local codemux is a single Rust process; SSH outward, `codemuxd` inward

**Status:** Accepted (2026-01), amended 2026-03 (the "no daemon on remote
hosts" constraint is retired now that AD-3 is in place).

**Context.** codemux is a personal tool with one user. A client/server split
on the local machine would be pure ceremony.

**Decision.** The local codemux binary is single-process: no client/server
split, no local daemon. All TUI, navigation, transport, and persistence
live in one process. SSH is the outbound transport for remote PTYs. A
small per-host daemon (`codemuxd`, AD-3) holds remote PTYs.

**Consequences.** No IPC overhead on the local machine. Remote sessions
get continuity at the cost of one minimal codemux-owned daemon per remote
host.

**Naming.** The daemon is `codemuxd`, not `codemux-agent`. The domain type
`Agent` already means "a Claude Code workspace"
(`crates/session/src/domain.rs`); overloading "agent" to also mean the
host-side daemon would force lifetime disambiguation in every doc, log
line, and conversation.

**Rejected alternatives.**
- *Local client/server split.* Solves no problem we have. Ceremony tax.

---

### AD-6 — Edits panel is `git diff`, read-only

**Status:** Deferred (P2).

**Context.** Reviewing what an unattended agent has done is a top-three
workflow. Building our own diff view is a rabbit hole.

**Decision (sketch).** A right-side panel that runs `git diff` on the focused
agent's cwd and renders the result with `syntect` for syntax highlighting.
One-keystroke "open in `$EDITOR`" for deep review. No staging, no
annotations: viewing only.

**Rejected alternatives.**
- *Tail JSONL transcripts and render diffs from tool calls.* Violates AD-1.

---

### AD-7 — State is a single SQLite file via `rusqlite`

**Status:** Deferred (P1). `rusqlite` is a workspace dep but currently
unused; no schema or migrations exist yet.

**Context.** Persistence is needed for AD-2 (reattach across restarts),
agent metadata across sessions, group tagging, and the host registry.

**Decision (sketch).** Single SQLite file at `$XDG_STATE_HOME/codemux/state.db`.
Schema migrations via `rusqlite_migration`.

**Rejected alternatives.**
- *Multiple JSON/TOML files.* Race conditions on multi-write, no migration
  story.
- *A "real" database (sled, redb, postgres).* Single-user pre-alpha tool;
  SQLite is the boring correct answer.

---

### AD-8 — No auth

**Status:** Accepted (policy).

**Context.** The TUI has no network surface. Authentication is moot.

**Decision.** No auth. Revisit only if a P4 phone view ever happens, which
would need a control socket and a thin frontend, a meaningful re-spec,
not a small feature.

---

### AD-10 — PTY library: `portable-pty`

**Status:** Accepted.

**Context.** We need a pure-Rust PTY spawner that works for both local fork
and (later) `ssh` subprocesses, on Linux and macOS.

**Decision.** Use `portable-pty`. Caret-range in `Cargo.toml`, exact
resolution in `Cargo.lock` ([AD-21](#ad-21--workspace-wide-dependencies-and-lints)).

**Rejected alternatives.**
- *`expect`-style libraries.* Heavier than we need.
- *Direct `forkpty`.* No Windows path, more `unsafe` to maintain.

---

### AD-11 — TUI stack: `ratatui` + `tui-term` + `vt100`

**Status:** Accepted.

**Context.** We need a TUI framework, a widget for nested PTY rendering, and
a VT parser. We want them to compose with minimal glue.

**Decision.**

- **`ratatui`** for chrome (status bar, navigator, popup overlays, spawn
  modal).
- **`tui-term`** widget for nested PTY rendering. Drops into a ratatui
  `Rect` with zero glue.
- **`vt100`** underneath `tui-term` for VT parsing.

As of 2026-04, `ratatui 0.30.0` + `tui-term 0.3.4` + `vt100 0.16.2` compose
cleanly. Note that `ratatui 0.30.0` is itself a workspace split
(`ratatui-core`, `ratatui-widgets`, `ratatui-crossterm`, `ratatui-macros`);
the top-level `ratatui` crate re-exports the lot.

**Fallback.** **`alacritty_terminal`**. If OSC 8 (hyperlinks), full SGR
mouse modes, or alt-screen edge cases start to bite, swap the terminal
backend. Contained refactor, not an architectural change. Direct precedent:
`egui_term`, `gpui-terminal`, and `missiond-core` all chose
`alacritty_terminal` for fidelity.

**Rejected alternatives.**
- *Rolling our own on top of `vte` (zellij's path).* Most powerful, most
  work. Not worth it for a personal tool when `tui-term` exists.
- *`wezterm-term`.* Not cleanly available on crates.io as a standalone
  crate.
- *In-process rendering of Claude Code via JSONL tailing or
  protocol-aware chrome.* See AD-1.

---

### AD-12 — Nested-PTY input routing via prefix key

**Status:** Superseded by [AD-24](#ad-24--keymap-registry-as-pod-config-is-a-plain-old-data-structure).

**Context.** Originally drafted as a P1 plan: tmux-style prefix (default
`Ctrl-B`); without the prefix, keys go verbatim to the focused PTY.

**Outcome.** Subsumed by AD-24's `Bindings` POD + the `dispatch_key`
prefix state machine in `apps/tui/src/runtime.rs`. The decision was right;
the implementation lives under a more general framework.

---

### AD-13 — OS notifications via platform helper shell-out

**Status:** Deferred (P2).

**Decision (sketch).** `osascript` (macOS) / `notify-send` (Linux) via
shell-out. No platform-specific Rust crates. Notifications fire only on
attention-needed transitions (finished, needs-input).

---

### AD-14 — Cargo workspace with `apps/` and `crates/` split

**Status:** Accepted.

**Context.** From day one we knew there would be at least one binary
(`codemux`) and reusable library code. A workspace was the right shape.

**Decision.** Cargo workspace at the repo root. `apps/` for delivery-shaped
crates that produce binaries (`apps/tui`, `apps/daemon`). `crates/` for
bounded contexts and pure-data crates (`session`, `shared-kernel`, `wire`,
`codemuxd-bootstrap`). Edition 2024, resolver 3.

**Consequences.** New binaries become siblings of `apps/tui` rather than
forcing a rename. New bounded contexts become siblings of `session`. The
top-level tree tells you what the system *is* without you reading any
Rust ([AD-15](#ad-15--package-by-component-not-by-technical-layer)).

---

### AD-15 — Package by component, not by technical layer

**Status:** Accepted.

**Context.** A common Rust workspace anti-pattern is splitting by layer:
`crates/domain`, `crates/state-store`, `crates/pty-host`, `crates/tui-chrome`.

**Decision.** Crates and modules are bounded by domain concern (`session`),
not by technical layer. A component crate owns its domain types and (when
they exist) its use cases, ports, and adapters. Top-level folders under
`crates/` name *bounded contexts*, so the source tree screams what the
application does.

**Consequences.** Small updates stay localized. New behaviour goes in one
crate, not five.

**Rejected alternatives.**
- *Horizontal layering (`domain`, `state-store`, `pty-host`, `tui-chrome`,
  `notify`, `app-shell`).* Lasagna Architecture: small updates reverberate
  through every layer, proxy methods accumulate at boundaries, and the
  source tree tells you nothing about what the application *does*.

---

### AD-16 — TUI binary at `apps/tui/`, daemon binary at `apps/daemon/`

**Status:** Accepted.

**Context.** The directory should describe the delivery shape, not the
technology. A future `apps/phone-view/` becomes a sibling without
renaming anything.

**Decision.** `apps/tui/` ships crate `codemux-tui`, binary `codemux`.
`apps/daemon/` ships crate `codemux-daemon`, binary `codemuxd`. Each is
a thin shell over a library entry point: `apps/daemon/src/lib.rs` is
the supervisor entry, `apps/daemon/src/main.rs` is one function that
parses argv and calls into the lib. This makes integration tests
in-process driveable (`apps/daemon/tests/`).

---

### AD-17 — Per-component error types via `thiserror`

**Status:** Accepted.

**Context.** Sharing a single workspace-wide error enum forces every crate
to know every other crate's failure shape; over time the enum grows until
abstraction layers collapse.

**Decision.** Each library crate defines its own `thiserror` enum, marked
`#[non_exhaustive]`. The binary uses `color-eyre` at the edge to wrap
library errors for human-readable reporting.

```rust
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("pty failed")]
    Pty(#[source] std::io::Error),
    /* … */
}
```

**Consequences.** Per-crate error vocabularies stay contained. Variants can
be added without breaking downstream `match` arms (because of
`#[non_exhaustive]`).

**Rejected alternatives.**
- *Shared `Error` enum for the whole workspace.* Ball-of-Mud trap.

---

### AD-18 — Infrastructure adapters co-located with their component

**Status:** Accepted (policy).

**Context.** When a bounded context grows real infrastructure adapters
(SQLite-backed repository, a network client, etc.), where do they live?

**Decision.** Inside the same crate as the bounded context. When `session`
grows a real `infra/` module, it lives in `crates/session/src/infra/`,
not in a separate `crates/infra/`.

**Rejected alternatives.**
- *Separate `crates/infra/` per technology.* Reintroduces layered packaging
  (AD-15).

---

### AD-19 — `shared-kernel` carries IDs only

**Status:** Accepted.

**Context.** Every crate needs `HostId`, `AgentId`, `GroupId`. A shared
crate is unavoidable, but shared crates have a way of growing.

**Decision.** `crates/shared-kernel/` is the smallest crate in the workspace.
It defines newtype ID wrappers backed by `Arc<str>` (cheap to clone through
event-loop channels). Zero vendor deps, zero business logic. If something
isn't an ID, it doesn't go in here.

```rust
macro_rules! newtype_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Arc<str>);
    };
}
newtype_id!(HostId);
newtype_id!(AgentId);
newtype_id!(GroupId);
```

---

### AD-20 — Ports-and-adapters via enum dispatch inside each component

**Status:** Accepted.

**Context.** Hexagonal architecture / ports-and-adapters is a useful
discipline for keeping I/O at the edges. The Rust shape it usually takes
in textbooks is `Box<dyn Trait>` ports.

**Decision.** When a port has a *closed* set of adapters known at compile
time, model it as an `enum`, not a trait object. The first real instance
in this codebase is `AgentTransport`:

```rust
#[non_exhaustive]
pub enum AgentTransport {
    Local(LocalPty),
    SshDaemon(SshDaemonPty),
}
```

The runtime calls six methods (`try_read` / `write` / `resize` / `signal` /
`try_wait` / `kill`); the variant dispatches.

**Consequences.** No vtable indirection, no `Box` allocation per agent. The
exhaustive `match` is the type-checker reminding us we've added a transport.

**Rejected alternatives.**
- *`Box<dyn Transport>`.* Overkill for two variants known at compile time.
  Rust style guide and the ratatui community both prefer enum dispatch
  for closed sets.

---

### AD-21 — Workspace-wide dependencies and lints

**Status:** Accepted.

**Context.** Without coordination, six crates can independently bump and
diverge on a shared dep, and lint configurations drift.

**Decision.** Shared dependencies are declared in `[workspace.dependencies]`
at the root. Member crates inherit with `{ workspace = true }`.

**Versioning policy.** **Caret-range in `Cargo.toml`, exact resolution in
`Cargo.lock`**, per the Cargo idiom. Do not use `=X.Y.Z` in the manifest;
it blocks `cargo update` from pulling security patches.

**Lints.** Shared lints in `[workspace.lints]`. Rust edition 2024, Cargo
resolver 3.

```toml
[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all       = { level = "deny",  priority = -1 }
pedantic  = { level = "warn",  priority = -1 }
unwrap_used = "deny"
expect_used = "deny"
```

`unwrap_used` and `expect_used` are Clippy `restriction` lints (opt-in by
Clippy's own design); they're enforced here because this is intended as
long-lived code and the `#[allow(...)]` escape hatch handles the
genuinely-OK cases. Member crates inherit with `[lints] workspace = true`.

**Rejected alternatives.**
- *`cargo-hakari`.* Needed only in very large workspaces; edition 2024 +
  resolver 3 handle feature unification natively.

---

### AD-22 — Navigator is a view model in `apps/tui`, not its own crate

**Status:** Accepted.

**Context.** The navigator (tab strip, popup switcher, focus index, filter)
is UI state. The agents it represents are domain.

**Decision.** The navigator's *data* (the agent list) belongs to `session`.
Its *UI state* (selected index, popup visibility, hitboxes) lives in
`apps/tui/src/runtime.rs` as `NavState` and friends. No separate
`crates/navigator/`.

**Consequences.** The navigator can change shape (popup vs left-pane,
keyboard vs mouse) without touching `session`. If a second delivery
(e.g. a phone view) ever needs the navigator vocabulary, extract per
[AD-23](#ad-23--fitness-functions-for-extracting-further-crates).

---

### AD-23 — Fitness functions for extracting further crates

**Status:** Accepted (policy).

**Context.** "Should this be its own crate?" is one of the easiest things
to over-decide. We need a brake.

**Decision.** Extract a new crate only when one of these signals fires:

1. Full workspace `cargo test` on warm cache exceeds ~15 s.
2. A component needs different feature flags on a shared dep.
3. A component becomes Claude-Code-agnostic and is a credible OSS
   publication candidate.
4. A second binary needs the bounded context without the TUI adapters.

**Consequences.** We don't pay the per-crate ceremony tax (split
`Cargo.toml`, public-API friction) until one of those signals actually
fires.

---

### AD-24 — Keymap registry as POD; config is a Plain Old Data structure

**Status:** Accepted.

**Context.** Key bindings need to be configurable, the help screen needs
to render them, and the prefix-state dispatcher needs to consult them.
All three need to agree.

**Decision.** Key bindings are typed action enums per scope (`PrefixAction`,
`PopupAction`, `ModalAction`, `DirectAction`, `ScrollAction`) plus a
`Bindings` POD that the runtime consults via
`bindings.<scope>.lookup(KeyEvent) -> Option<Action>`. This is the TEA
(Elm-style) dispatch pattern: input → typed action → state mutation, with
the keymap as the single source of truth.

**Configuration location.** `$XDG_CONFIG_HOME/codemux/config.toml`, falling
back to `$HOME/.config/codemux/config.toml`. XDG on every Unix, including
macOS: the `directories`/`dirs` crates default to
`~/Library/Application Support/` on macOS, which is the Apple GUI
convention and the wrong place for a CLI tool. Modern CLIs (gh, git,
helix, kubectl, alacritty, ripgrep) all settled on `~/.config/`
regardless of platform; we follow suit. The config is loaded once at
startup into a `Config` POD and passed by reference into `runtime::run`.

**No port for config.** Direct quote from the architecture-guide review of
this slice (NLM, 2026-04-23): *"For a personal pre-alpha tool, reading
the config at startup and passing it as a Plain Old Data structure at
construction time is the architecturally sound choice."* A port earns
its keep only when config becomes dynamic (remote service); not now.

**Mapping a key to an action is a presentation concern.** Both `keymap`
and `config` therefore live in `apps/tui/`, not in a separate crate.
Extract per [AD-23](#ad-23--fitness-functions-for-extracting-further-crates)
if/when a second delivery (e.g. a phone view) needs to share the keymap
vocabulary.

**Failure mode.** Missing config file = defaults. Present-but-invalid
config file = exit non-zero with a readable error before the TUI starts.
Per CLI guidelines, silent fallback would be worse than refusing to start.

**Help screen.** Generated from the same `Bindings` POD: single source
of truth for behaviour and documentation.

**Cmd / Super support via auto-detected Kitty Keyboard Protocol.** macOS
terminals swallow Cmd before any TUI can see it, unless the application
negotiates the Kitty Keyboard Protocol with the terminal first. codemux
walks every loaded `KeyChord` at startup; if any uses `KeyModifiers::SUPER`,
it pushes `KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES` after
entering raw mode and pops it in `TerminalGuard::drop`. Auto-detect
rather than a separate config flag because the `Bindings` *are* the
source of truth: write `prefix = "cmd+b"` and the protocol negotiation
follows automatically. Terminals that do not understand the negotiation
silently ignore it; the help screen is the user-visible escape hatch
("if my chord doesn't fire, the terminal is the limit").

**Rejected alternatives.**
- *HashMap-based registry indexed by `(Scope, KeyChord)`.* With ~7 entries
  per scope, linear search through a fixed-size array is faster than a
  HashMap and the table reads as data declaration. Re-evaluate if scope
  sizes grow past ~30.
- *`#[derive(Deserialize)]` on `crossterm::event::KeyEvent`.* Hand-rolled
  `KeyChord` parser keeps the user-facing format independent of
  crossterm's internal representation (which has fields like `kind` and
  `state` the user never wants to think about).

---

### AD-25 — Per-agent scrollback via vt100's primary-grid back buffer

**Status:** Accepted.

**Context.** The wheel should scroll the focused agent's transcript history
without breaking when the user switches agents or when the terminal
resizes.

**Decision.** The wheel scrolls the focused agent's transcript history.
The implementation rests on three observations and one deliberate trade-off.

**Observation 1: Claude Code stays on the primary screen.** Verified by
PTY-probing the initial output for the alt-screen DEC modes (`?1049h`,
`?47h`, `?1047h`); none appear, only `?2004h` (bracketed paste) and
`?25l` (cursor hide). This matters because vt100's alternate-grid is
hardcoded to `scrollback_len: 0` and would never collect history; only
the primary grid does. We're lucky here, and the test
`runtime::tests::scrollback_zero_len_means_no_history` guards the
contract our luck depends on.

**Observation 2: vt100 owns the offset.** When new rows evict the top
while `scrollback_offset > 0`, vt100 auto-bumps the offset by one so the
same content stays under the user's gaze. We never store an offset in
`RuntimeAgent`; we read `screen.scrollback()` and call
`screen_mut().set_scrollback(cur ± delta)` per wheel tick. Per-agent
state is implicit in each agent's `Parser`, which means switching focus
**preserves** scroll position: coming back to a scrolled-back agent
shows it where you left it. This is intentional; do not "fix" it.

**Observation 3: tui-term renders scrollback automatically.**
`PseudoTerminal::new(parser.screen())` already calls
`screen.visible_rows()` (which respects the offset) and shifts the cursor
row by `screen.scrollback()`. No render-side glue is required beyond the
floating "scroll mode" indicator badge.

**Trade-off: no PTY resize when scrolled.** A bottom-row "SCROLL" status
strip would force a PTY `SIGWINCH` on every scroll-mode entry/exit, and
Claude redrawing its full UI on every transition would be much worse UX
than the alternative. Instead the indicator is a floating widget painted
via `Clear` + `Paragraph` over the bottom-right of the agent pane. It
costs ~22 cells of overlap during scroll mode, gains zero `SIGWINCH`
churn.

**Mouse capture is unconditional**, gated only on the alt-screen entry
succeeding. `EnableMouseCapture` (`?1006h` SGR mouse) is what makes
`MouseEventKind::ScrollUp/Down` reach the event loop instead of being
translated to ↑/↓ arrows by the host terminal's `alternateScroll`
behavior. Side effect: terminal-native click-and-drag selection requires
holding ⌥/Alt to bypass capture. **Apple Terminal does not deliver SGR
mouse events**; scroll won't work there. Explicit non-goal: codemux
works with iTerm2, Alacritty, Ghostty, Wezterm, Kitty.

**Selection is implemented in-app, not handed to the terminal.** Because
mouse capture eats drag events anyway, codemux paints its own selection
overlay (reverse-video on the buffer cells in the pane rect) and writes
the extracted text to the system clipboard via OSC 52
(`\x1b]52;c;<base64>\x07`) on mouse-up. `vt100::Screen::contents_between`
does the cell-range → text conversion (it already walks `visible_rows()`
so scrollback is included automatically). The user gets modifier-free
drag-to-select that works inside any agent pane; the ⌥-bypass remains
documented as the fallback for terminals without OSC 52 (Apple Terminal,
locked-down corp environments). Selection state is per-frame and
per-focused-agent: a tab switch, agent reap, or terminal resize clears
it. Same single-pane, single-selection model as tmux's
`copy-mode-mouse`. Gestures in v1 are drag-only: no double-click word
or shift-extend; both are deferred until they're asked for.

**Scroll mode is non-sticky for typing.** Bytes that would have been
forwarded to Claude (typing real text, control sequences, anything the
dispatcher returns as `KeyDispatch::Forward`) first snap the focused
agent back to the live view (`set_scrollback(0)`), so what you type
isn't echoed into a window you can't see. **Navigation chords preserve
scroll**: pressing the prefix, a direct nav bind, or hitting digit-1..9
in prefix mode does NOT reset the offset, so a `Cmd-B 2` to switch tabs
leaves the agent you just left exactly where you scrolled it.
`Event::Paste` snaps for the same visibility reason as forwarded bytes.

---

### AD-26 — Daemon owns a session-scoped vt100 parser for snapshot replay

**Status:** Accepted.

**Context.** `codemuxd` exists for session continuity: the PTY child
outlives any single client connection, so a user can close their TUI,
walk away, and reattach later to the same Claude. The wire protocol
carries *new* PTY bytes from daemon to client, but the screen state
that came before the reattach lived only in Claude's memory and the
disconnected client's vt100 buffer. On reconnect a new client got a
fresh empty parser; an idle Claude (sitting at its prompt, no SIGWINCH
because the geometry hadn't changed) emitted nothing, and the screen
stayed blank until the user typed something that forced Claude to
redraw. Same session, different visible state, exactly what session
continuity was supposed to prevent.

**Decision.** The daemon mirrors the child's terminal in its own
`vt100::Parser`, sized to whatever client is currently attached, and
emits `Screen::state_formatted` (clear + per-cell positioned text +
attributes + input modes) as the **first PtyData frame** after every
handshake. The client's parser starts empty; the snapshot leaves it in
a state byte-equivalent to the daemon's. Live forwarding then resumes
from the post-snapshot moment, no gap.

Three things make this safe:

**1. Atomic process+send in the reader thread**
(`apps/daemon/src/pty.rs::spawn_reader_thread`). Each PTY chunk is fed
to the parser AND pushed to `rx` under a single parser lock acquisition.
The invariant the snapshot path relies on is "any chunk in `rx` is also
in the parser, and vice versa." Without atomicity, a chunk arriving
between the snapshot's drain and its capture would be either silently
dropped (in the parser but already drained from rx) or duplicated (in
rx but not yet in parser, so the snapshot misses it and the live
forward sends it). The daemon's PTY reader pays one mutex acquisition
per 8 KiB read, which is invisible against the syscall cost.

**2. Snapshot lives in `Session`, not `conn`** (`apps/daemon/src/session.rs`,
`take_snapshot`). The connection adapter (`conn::run_io_loops`) only
deals with sockets, framing, and the inbound/outbound thread scope.
It never touches `vt100`, the `?1049h` toggle, or `state_formatted`.
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
*outside* the parser lock. It's a `TIOCSWINSZ` ioctl independent of
the parser, and stalling the reader on it would needlessly delay
in-flight chunks.

**3. `?1049h` prefix when alt-screen is active**
(`Session::take_snapshot`). `Screen::state_formatted` writes the
contents of the *active* screen but doesn't toggle which screen the
receiver should be on. A Claude session in alt-screen mode would
otherwise have its alt-buffer content clear-and-painted onto the
client's primary buffer, which is wrong on attach (visible content
lands on the wrong half) and worse on the child's next mode toggle
(the misplaced content lingers when the child switches back). For
primary-mode sessions we deliberately omit the toggle, since the client
parser starts in primary, so a no-op switch just adds bytes.

**Carve-outs.** This is a **structural** parsing of Claude's output:
escape-sequence reproduction (cursor positions, attribute runs, mode
flags), not interpretation of Claude's UI semantics. AD-1 is unbroken
in spirit: there is no reading of "is this a prompt", "is this a tool
call", "what is the assistant doing"; the parser is downstream of
every byte and treats them all the same way it would any VT-compatible
stream. The carve-out exists because the wire protocol cannot transmit
screen state any other way short of keeping unbounded raw byte history
per session, which is strictly worse on memory and still wouldn't
handle parser-state things like attribute carries across line wraps.

**The daemon's parser uses `scrollback_len: 0`** because the TUI client
already owns the scrollback buffer (AD-25). Duplicating it on the
daemon side would double the memory footprint of every remote session
for no gain. The client only needs the visible grid restored on
reconnect; history is already in its own parser.

---

### AD-27 — Tab affordances on the captured mouse stream

**Status:** Accepted.

**Context.** The tab strip is the navigator's single most-used affordance,
and codemux already captures the mouse stream
([AD-25](#ad-25--per-agent-scrollback-via-vt100s-primary-grid-back-buffer),
mouse section). The runtime sees `MouseEventKind::Down/Up/Drag` instead
of letting the terminal own them. With the events already in hand,
leaving them unbound would have meant the user has to lift their hand
off the trackpad and reach for `Ctrl-B 1..9` to switch agents. For a
multiplexer whose whole job is fast switching, that is a pointless
detour.

**Decision.** Two gestures on the tab strip:

- **Click on a tab focuses it** (no prefix).
- **Drag a tab onto another's slot reorders the agents** in browser-tab
  semantics: `Vec::remove(from) + insert(to)`, not swap. Same gesture
  in both nav styles: the bottom strip in Popup mode and the left nav
  rows in LeftPane mode are both clickable.

**Hitboxes are recorded by the renderer.** A `TabHitboxes` struct
(`apps/tui/src/runtime.rs`) is owned by `event_loop`, cleared at the top
of every `render_frame`, and passed to the two leaf renderers
(`render_status_bar`, `render_left_pane`). Each renderer records a named
`Hitbox { rect, agent_id }` for every tab as it draws it. The mouse
handler reads the hitboxes back on `Down(Left)` / `Up(Left)` to
translate `(column, row)` to an agent identity (not an index).

This is the cleanest seam available: the renderer is the only place
that knows where each tab landed (after layout splits, separator spans,
hint reservation, area clipping). Recording the rect inline during
rendering avoids the duplicate-the-width-math trap that any post-hoc
geometry derivation would have fallen into.

**Press grabs a stable identity, not an index.** `mouse_press: Option<AgentId>`
stores the agent's id (not its `Vec` slot). Storing identity means a
terminal resize, agent reap, or background reorder between Down and Up
still resolves the gesture correctly: the event loop runs
`agents.iter().position(|a| a.id == id)` at the moment of the mutation,
returning `None` (and silently cancelling) if the agent is gone. An
index-based press would have silently re-targeted to a different agent
in the same slot, the kind of fragility the identity boundary exists
to prevent. The renderer/dispatcher seam is also pure-functional:
`tab_mouse_dispatch` returns `Option<TabMouseDispatch>` (variants
`PressTab(AgentId)` / `Click(AgentId)` / `Reorder { from, to }` /
`Cancel`), so every gesture branch is unit-testable without an
event-loop harness.

Release outcomes:

- same id → click → `change_focus`
- different id → drag → resolve both ids to current indices, then
  `reorder_agents` followed by `shift_index` on `focused` and
  `previous_focused` so the same agent stays focused across the reorder
- released outside any tab → cancel

Crossterm only fires `Drag` on motion, so a same-cell down→up is a clean
click with no intervening drag; the same code path serves both gestures.

**Cost.** Captured clicks and drags can no longer reach the terminal for
native text selection over the tab strip. The `⌥/Option-drag` escape
hatch (iTerm2, Ghostty, Alacritty, WezTerm, Kitty) bypasses mouse capture
per-drag and is documented in the help screen alongside the new `click` /
`drag` lines. Apple Terminal does not deliver SGR mouse anyway, so neither
tab gestures nor scroll work there. Same explicit non-goal as AD-25.

---

### AD-28 — Wire encoder vs readline-shortcut adapter (split layers)

**Status:** Accepted.

**Context.** Translating a `KeyEvent` into the bytes a terminal-mode child
expects is two responsibilities, not one. Conflating them leaks GUI-style
opinions into what should be a pure VT100/ANSI encoder.

**Decision.** Two named functions in `apps/tui/src/runtime.rs`:

1. **`encode_terminal_key`**: pure VT100 / ANSI key encoder. Maps
   `Backspace → DEL`, `Up → ESC[A`, `Char('a') → 'a'`, etc. The only
   modifier branching it does is `Ctrl-letter → 0x01..0x1A`, because
   that's protocol (Ctrl-C *is* 0x03), not opinion. No GUI-style
   modifier ever changes the output here. Tests pin this invariant
   explicitly: `encode_terminal_key(Backspace, SUPER)` must equal
   `encode_terminal_key(Backspace, NONE)`, both `vec![0x7f]`.
2. **`translate_readline_shortcut`**: the deliberately opinionated
   adapter that bridges GUI-style chords (Cmd+Backspace, Shift+Enter,
   Ctrl+Backspace, ...) to readline byte sequences:
   `Cmd+Backspace → Ctrl+U` (unix-line-discard),
   `Ctrl/Alt+Backspace → Meta+DEL` (unix-word-rubout),
   `(Shift|Alt|Ctrl|Cmd)+Enter → Meta+Enter` (newline-in-input).
   Returns `Some(bytes)` only when the chord matches a registered
   shortcut; `None` otherwise.

`key_to_bytes` is a one-line orchestrator: shortcut first, encoder
fallback. Wire bytes leaving the function are byte-identical to the
previous combined implementation; the change is purely structural.

**Why split.** The architecture-guide review (NLM, 2026-04-28) flagged
the previous combined function as a Leaky Abstraction. The byte
encoder was carrying GUI-flavored opinions about what `Cmd+Backspace`
or `Shift+Enter` "should" mean, and a reader of the encoder had no
way to tell where the protocol stopped and the opinion started. The
same critique had landed earlier against the modified-Enter handler
inside the old `key_to_bytes` and was carried forward unresolved. The
split fixes both at once: the encoder is pristine and reads as
protocol; the shortcut adapter has a docstring whose first paragraph
is "this layer leaks GUI conventions onto the wire on purpose, here's
why."

**Why opinionated translation, not raw modifier passthrough.** Claude
(and every readline-style TUI input we target) speaks the universal
readline byte vocabulary (`Ctrl+U`, `Meta+DEL`, `Meta+Enter`) but
not the Kitty Keyboard Protocol's CSI-u extended encoding for
modified non-character keys. Passing `Cmd+Backspace` through verbatim
via CSI-u would land literal escape garbage in Claude's input. So the
adapter's job is precisely to translate user intent into the bytes
the child can act on; the "fidelity loss" the review flagged is the
entire point of the layer existing.

**Acceptance criteria for adding a new shortcut.** The chord must:
(a) be a recognized GUI convention with no ambiguity (Cmd+Right for
end-of-line, Cmd+A for select-all, etc.), (b) have a well-known
readline byte sequence on the receiving end, and (c) the destination
must be readline-style input, not a vt100 application that
interprets the raw chord differently. Anything violating (c) belongs
in the encoder, not here.

**Why hardcode rather than route through `Bindings`.** The shortcut
layer is *protocol bridging*, not user policy. `Bindings` (AD-24) maps
key chords to application intents (`SpawnAgent`, `FocusNext`); it is
the right home for "what should codemux do when I press X". The
shortcut adapter answers "what bytes does a readline-style child
process expect when the user expresses delete-line intent", a
separate layer with a separate vocabulary. If a future user wants to
remap `Cmd+Backspace` to send something other than `Ctrl+U`, the
shape now makes that an additive change (introduce a
`ReadlineShortcuts` POD on `Bindings`), not a teardown.

**Rejected alternatives.**
- *Single combined `key_to_bytes` function with modifier branching
  inline* (the prior shape). Re-evaluating it would re-stage the
  exact Leaky Abstraction the review fired on, twice.

---

### AD-29 — Status-bar segments are a closed set of built-ins, selected by config

**Status:** Accepted.

**Context.** The bottom status bar's right side renders a stack of context
segments (model · repo · branch · prefix-hint by default). Users want
control over which appear; we want to avoid building a plugin system.

**Decision.** Users pick which segments to show, in what order, via
`[ui] status_bar_segments = [...]` in `config.toml`. Order is
left-to-right; under width pressure, segments are dropped from the
**LEFT** first so the rightmost (highest-priority) one stays visible.

The plumbing lives in `apps/tui/src/status_bar/`:

- `mod.rs`: the `StatusSegment` trait, `SegmentCtx` POD, the
  right-to-left drop algorithm in `render_segments`, and the
  `build_segments(&ids)` registry that maps config strings to built-in
  implementations.
- `segments.rs`: the built-in segments. Each is a stateless unit
  struct that reads its data off `SegmentCtx` and returns
  `Some(Line)` or `None` (skip silently).

**What this is not.** Not dynamic plugins. Not shell-out segments. Not
scripting. Built-in IDs only. Adding a new segment is a typed change
in this codebase: implement `StatusSegment`, add an ID constant,
register it in `build_segments`'s match arm. The config layer follows
automatically.

**Consequences.** Three things we get from the closed-set discipline:

1. **No subprocess on the render hot path.** A shell-out segment would
   invoke `sh -c` per refresh per segment per agent: expensive and a
   stutter risk.
2. **Typed contract.** Each segment owns its style decisions in one
   place. A typo in a built-in ID is a clippy-flagged literal mismatch;
   a typo in the config logs once at startup and falls back gracefully.
3. **Bounded surface area.** When debugging "why is this segment empty,"
   the answer is always one Rust impl in `segments.rs`. Shell-command
   segments would split the answer between codemux, the user's shell,
   and the user's PATH.

**Drop-from-the-left rationale.** The user's primary cue (the prefix-key
hint) lives at the right edge today. Keeping the existing visual anchor
stable across narrow terminals (and letting newly-added segments
degrade gracefully on small screens) was the design goal. Segments
stack up to the left of the hint, and when a 60-cell terminal can't
fit `model: opus-4-7 │ repo: codemux │ codemux:main │ ctrl+b ? for help`,
the user still sees `ctrl+b ? for help` and the focused agent's tab,
not a confused half-hint.

**The model segment is special.** It triggers an AD-1 carve-out (the
`agent_meta_worker` reads `~/.claude/settings.json` to extract the
user's currently-selected model alias and effort level). See
[AD-1's](#ad-1--host-the-pty-do-not-semantically-parse-claude-code)
amended prose for the bounded-exception scope.

**Rejected alternatives.**
- *Hardcoded inline rendering of the three segments in
  `render_status_bar`* (the simplest possible thing). Would require a
  config knob per segment to disable individually, and any new segment
  would mean another inline branch in the renderer. The trait shape
  lets the renderer stay schema-agnostic and pushes per-segment
  formatting into one file per segment.

---

## Open questions

Triaged from earlier drafts. These are not ADRs; they're flagged
unknowns for future decisions.

- **Notification content and granularity.** P2 via
  [AD-13](#ad-13--os-notifications-via-platform-helper-shell-out);
  attention-needed events only (finished, needs-input). No notifications
  for running / idle / starting.
- **Approval-prompt visibility from the navigator.** Deferred to P2+.
  Likely just a "needs input" status dot; user focuses to see what's
  being asked.
- **Cross-host file-edits cache.** Accepted pain. `ssh <host> -- git diff`
  on focus change, cached per agent. If it becomes slow enough to bother,
  revisit, possibly a tiny `git-diff-watcher` helper on remote hosts.
- **Handoff / export.** Deferred. Low-frequency need; revisit if the use
  case becomes real.
- **Multi-window / detach-pane.** Deferred. TUI makes this meaningfully
  harder than the GUI path would have.
- **Phone / iPad access.** P4 maybe. Would require a control socket and
  a thin web frontend, a meaningful re-spec, not a small feature.
- **Devpod auth / host setup wizard.** Deferred. Today: manual per-host
  login flow. Revisit post-P3.
- **Color and accessibility.** P1 concern. Status signal uses shape +
  color, never color alone.
- **Telemetry.** None. Ever. Maybe a local `sessions-per-day` counter
  for self-curiosity in P3+. No remote reporting, ever.
