# Acceptance criteria

## Contents

- [Spawn](#spawn)
  - [AC-001: Spawn the initial agent at launch](#ac-001-spawn-the-initial-agent-at-launch)
  - [AC-002: Spawn a local agent in the scratch directory](#ac-002-spawn-a-local-agent-in-the-scratch-directory)
  - [AC-003: Spawn a remote agent over SSH (cold-start bootstrap)](#ac-003-spawn-a-remote-agent-over-ssh-cold-start-bootstrap)
  - [AC-004: Path-zone wildmenu autocompletes against the focused host](#ac-004-path-zone-wildmenu-autocompletes-against-the-focused-host)
  - [AC-005: Quick-switch to precise mode by typing `~` or `/`](#ac-005-quick-switch-to-precise-mode-by-typing--or-)
  - [AC-006: Drill into a folder, then spawn at the chosen depth](#ac-006-drill-into-a-folder-then-spawn-at-the-chosen-depth)
  - [AC-007: Saved project alias resolves through the minibuffer](#ac-007-saved-project-alias-resolves-through-the-minibuffer)
  - [AC-008: Cancel the spawn minibuffer](#ac-008-cancel-the-spawn-minibuffer)
  - [AC-032: Spawn modal opens at the TUI startup cwd, not the focused agent's cwd](#ac-032-spawn-modal-opens-at-the-tui-startup-cwd-not-the-focused-agents-cwd)
  - [AC-033: Spawn modal swallows all keystrokes while open](#ac-033-spawn-modal-swallows-all-keystrokes-while-open)
  - [AC-045: Indexing runs in the background; input stays interactive](#ac-045-indexing-runs-in-the-background-input-stays-interactive)
- [Navigation](#navigation)
  - [AC-009: Cycle focus between agents](#ac-009-cycle-focus-between-agents)
  - [AC-010: Focus an agent by ordinal digit](#ac-010-focus-an-agent-by-ordinal-digit)
  - [AC-011: Bounce to the previously-focused agent](#ac-011-bounce-to-the-previously-focused-agent)
  - [AC-012: Switcher popup picks an agent by name](#ac-012-switcher-popup-picks-an-agent-by-name)
  - [AC-013: Toggle the navigator chrome](#ac-013-toggle-the-navigator-chrome)
  - [AC-034: Spawning a new agent records the prior focus as the bounce slot](#ac-034-spawning-a-new-agent-records-the-prior-focus-as-the-bounce-slot)
  - [AC-035: Reaping the focused agent moves focus to the new tail, not to `previous_focused`](#ac-035-reaping-the-focused-agent-moves-focus-to-the-new-tail-not-to-previous_focused)
- [Agent lifecycle](#agent-lifecycle)
  - [AC-014: Force-close a live agent](#ac-014-force-close-a-live-agent)
  - [AC-015: Dismiss a crashed or failed agent (no-op on live)](#ac-015-dismiss-a-crashed-or-failed-agent-no-op-on-live)
  - [AC-016: Quit codemux cleanly](#ac-016-quit-codemux-cleanly)
  - [AC-036: Reaping the last agent auto-exits codemux](#ac-036-reaping-the-last-agent-auto-exits-codemux)
  - [AC-037: A non-zero PTY exit transitions Ready → Crashed (not silent removal)](#ac-037-a-non-zero-pty-exit-transitions-ready--crashed-not-silent-removal)
  - [AC-038: A panic restores the terminal before the report is printed](#ac-038-a-panic-restores-the-terminal-before-the-report-is-printed)
- [Scrollback](#scrollback)
  - [AC-017: Enter scroll mode and navigate history](#ac-017-enter-scroll-mode-and-navigate-history)
  - [AC-018: Typing snaps to live; navigation preserves scroll](#ac-018-typing-snaps-to-live-navigation-preserves-scroll)
  - [AC-039: Pasting while scrolled-back snaps to live before the bracketed-paste write](#ac-039-pasting-while-scrolled-back-snaps-to-live-before-the-bracketed-paste-write)
- [Mouse](#mouse)
  - [AC-019: Click a tab to focus it](#ac-019-click-a-tab-to-focus-it)
  - [AC-020: Drag a tab to reorder](#ac-020-drag-a-tab-to-reorder)
  - [AC-021: Drag-to-select and copy via OSC 52](#ac-021-drag-to-select-and-copy-via-osc-52)
  - [AC-040: Mouse events are suppressed while an overlay is open](#ac-040-mouse-events-are-suppressed-while-an-overlay-is-open)
  - [AC-041: Ctrl+click on a URL hands it to the OS opener; Ctrl+hover shows underline + hand cursor](#ac-041-ctrlclick-on-a-url-hands-it-to-the-os-opener-ctrlhover-shows-underline--hand-cursor)
- [Status bar](#status-bar)
  - [AC-022: Configured segments render in order](#ac-022-configured-segments-render-in-order)
  - [AC-023: Customize the status bar via config](#ac-023-customize-the-status-bar-via-config)
  - [AC-024: Segments drop from the left under width pressure](#ac-024-segments-drop-from-the-left-under-width-pressure)
  - [AC-042: Status bar is hidden in `LeftPane` chrome](#ac-042-status-bar-is-hidden-in-leftpane-chrome)
- [Help](#help)
  - [AC-025: Help screen reflects the live keymap](#ac-025-help-screen-reflects-the-live-keymap)
- [Daemon](#daemon)
  - [AC-027: Daemon serves a screen-state snapshot on every attach](#ac-027-daemon-serves-a-screen-state-snapshot-on-every-attach)
  - [AC-028: Reattach to a remote agent across TUI restart](#ac-028-reattach-to-a-remote-agent-across-tui-restart)
  - [AC-043: Daemon survives SSH disconnect via `setsid -f`](#ac-043-daemon-survives-ssh-disconnect-via-setsid--f)
  - [AC-044: Stale daemon is killed and re-deployed on local-binary upgrade](#ac-044-stale-daemon-is-killed-and-re-deployed-on-local-binary-upgrade)
- [Config and CLI](#config-and-cli)
  - [AC-029: Missing config file falls back to defaults](#ac-029-missing-config-file-falls-back-to-defaults)
  - [AC-030: Invalid config fails loud before raw mode](#ac-030-invalid-config-fails-loud-before-raw-mode)
  - [AC-031: Invalid `[PATH]` arg fails loud before raw mode](#ac-031-invalid-path-arg-fails-loud-before-raw-mode)

## Spawn

### AC-001: Spawn the initial agent at launch

**Given:**
- codemux is launched from a shell whose current pwd is a valid directory.

**When:**
1. Run `codemux` with no positional argument (or `codemux <PATH>` where `<PATH>` is a valid directory).

**Then:**
- The TUI starts with one tab already in the navigator and focused; no minibuffer opens.
- The agent's cwd is the shell's pwd, or the canonicalized `<PATH>` if one was passed (relative paths resolve against the shell's pwd).
- The pane renders Claude's prompt screen.

**Failure modes:**
- **Claude binary not on `$PATH`:** the spawn error propagates out of `runtime::run`; the process exits non-zero before raw mode with the error on stderr. (`Failed` state is reserved for SSH bootstrap errors, not for local spawn failures.)
- **Invalid `<PATH>` argument** (missing or not a directory): see AC-031; the process exits non-zero before raw mode.

**Tests:**
- `apps/tui/tests/pty_smoke.rs::fake_agent_prompt_renders` — boots codemux against the `fake_agent` stub and asserts its prompt renders. Covers the `pane renders` Then-clause; does not yet cover the failure modes (Claude binary missing, invalid PATH).

### AC-002: Spawn a local agent in the scratch directory

**Given:**
- codemux is running on a local terminal.
- `config.toml` either does not set `[spawn] scratch_dir` (default `~/.codemux/scratch`) or sets it to a writable path.

**When:**
1. Press the prefix, then `c` (or the direct chord `Super+\`).
2. Press `Enter` without typing a path.

**Then:**
- The minibuffer at the bottom of the screen closes.
- A new tab appears in the navigator with the focused-agent indicator on it.
- The agent's cwd is the configured scratch directory (default `~/.codemux/scratch`), created on demand if it does not exist.
- The pane renders Claude's prompt screen.

**Failure modes:**
- **Claude binary not on `$PATH`:** the new tab enters the `Failed` state; the pane shows a crash banner with the spawn error; the navigator stays interactive.
- **Scratch path cannot be resolved or created** (e.g. `scratch_dir` is neither absolute nor `~`-prefixed, or `mkdir -p` fails): the runtime logs a tracing diagnostic and falls back to the platform default cwd so the user still gets an agent.

### AC-003: Spawn a remote agent over SSH (cold-start bootstrap)

**Given:**
- A reachable SSH host the user has never spawned an agent on (no `~/.cache/codemuxd/` on the remote).
- The host appears in `~/.ssh/config` or in codemux's saved hosts.
- `cargo` is on `$PATH` on the remote (the bootstrap ships a source tarball and compiles `codemuxd` on the remote; there are no per-arch prebuilt binaries today).

**When:**
1. Press the prefix, then `c`.
2. Press `@`, type the host name, press `Tab`.
3. Pick a remote path from the wildmenu, press `Enter`.

**Then:**
- The path zone locks with a stage indicator that walks through `probing host → preparing source → uploading source → building remote daemon → spawning daemon → opening tunnel → connecting`. (Internal stage IDs: `VersionProbe`, `TarballStage`, `Scp`, `RemoteBuild`, `DaemonSpawn`, `SocketTunnel`, `SocketConnect`.)
- A new tab appears once the daemon `HelloAck` arrives over the tunnel.
- The pane renders Claude's prompt screen.
- A subsequent spawn on the same host skips `TarballStage`, `Scp`, and `RemoteBuild` when the cached daemon's version matches `bootstrap_version()`.

**Failure modes:**
- **SSH auth fails:** the minibuffer returns to the host zone with the SSH error; no agent slot is created.
- **`cargo` not on the remote `$PATH`:** `RemoteBuild` fails; the bootstrap surfaces the build error verbatim and the slot enters `Failed`.
- **Cached remote daemon binary is older than `bootstrap_version()`:** `prepare_remote` SIGTERMs (then SIGKILLs) the stale daemon and re-deploys, killing any in-flight Claude session under it. (See AC-027.)
- **Wire-protocol mismatch from a current binary:** daemon sends `Message::Error{VersionMismatch}`; the client surfaces it as `Error::Handshake` and the slot enters `Failed`. There is no retry on this path. Only binary-version mismatch triggers redeploy, and only because `bootstrap_version()` is checked at the bootstrap layer.
- **Remote path does not exist:** daemon sends `Error` on attach; slot enters `Failed` with the daemon's error message.

### AC-004: Path-zone wildmenu autocompletes against the focused host

**Given:**
- The spawn minibuffer is open.
- The host zone holds a value (`local` or a committed remote host).

**When:**
1. Type a partial path fragment in the path zone (e.g. `code`).

**Then:**
- The wildmenu shows candidates ranked by the active search mode (fuzzy by default; precise after `Ctrl+T`). Six rows are visible at a time; the underlying caps are `MAX_COMPLETIONS = 8` (precise local/remote `read_dir`) and `MAX_FUZZY_RESULTS = 50` (fuzzy ranking).
- `Down`/`Up` move the highlight (scrolling the visible window past row six); `Tab` applies the highlighted candidate to the path field.
- For a remote host, candidates come from the TUI-side per-host index built by `apps/tui/src/index_manager.rs` walking the remote filesystem through `RemoteFs` over the existing SSH `ControlMaster` socket. The daemon process does not participate in path completion; it owns PTYs, not paths.

**Failure modes:**
- **Local directory scan exceeds the cap** (`MAX_SCAN_ENTRIES = 1024`): the first 1024 entries are listed and the rest are silently truncated. The user can refine the query to narrow.
- **Remote `list_dir` exceeds its cap** (`MAX_LIST_ENTRIES`): same silent truncation.
- **Index not yet built** (fuzzy mode, fresh session): the wildmenu shows an indexer-state sentinel row (e.g. `indexing...`) instead of candidates; precise mode (`Ctrl+T`) still works synchronously. `Ctrl+R` forces a rebuild.

### AC-005: Quick-switch to precise mode by typing `~` or `/`

**Given:**
- The spawn minibuffer is open in fuzzy mode (the default).
- The path zone is focused and the typed query is empty (or the path field still holds its auto-seeded cwd).

**When:**
1. Type `~` (or a compose-key variant: `˜` U+02DC, `̃` U+0303), or type `/`.

**Then:**
- The path zone switches to precise mode for the rest of this open.
- The path field is seeded with the user's `$HOME` (for `~`) or `/` (for `/`); for a remote host, the remote `$HOME` captured during prepare is used.
- The wildmenu lists the seeded directory's children.
- The user's `user_search_mode` preference is NOT changed; closing and reopening the minibuffer returns to fuzzy.

**Failure modes:**
- **`$HOME` is unset on the local side:** the field is seeded with the literal `~/`; the user can edit forward or backspace.

### AC-006: Drill into a folder, then spawn at the chosen depth

**Given:**
- The spawn minibuffer is in precise mode, path zone focused.
- The wildmenu shows one or more folder candidates.

**When:**
1. Press `Down` to highlight a folder.
2. Press `Tab` (or `Enter`) to descend.
3. Optionally drill again by highlighting a child and pressing `Tab` (or `Enter`).
4. With no candidate highlighted, press `Enter` to spawn at the current path.

**Then:**
- Step 2 descends into the highlighted folder: the path field becomes the folder's path (with trailing `/`), the selection clears, and the wildmenu refreshes to list that folder's children.
- Step 3 walks deeper.
- Step 4 spawns the agent at the path now in the field.
- In fuzzy mode this drilldown does NOT happen: `Tab` is a no-op, and `Enter` on a fuzzy hit applies-and-spawns in one step.

**Failure modes:**
- **Highlighted folder no longer exists at descend time** (e.g. deleted out from under): the refresh lists empty children; the user can `Backspace` out or pick a sibling.

### AC-007: Saved project alias resolves through the minibuffer

**Given:**
- `config.toml` contains a `[[spawn.projects]]` entry with `name = "codemux"` and a `path = "~/workbench/repositories/codemuxhq/codemux"` (or `host = "..."`-bound).

**When:**
1. Open the spawn minibuffer.
2. Type `codemux` in the path zone.

**Then:**
- The named project appears at the top of the wildmenu, score-boosted above any fuzzy-matched directory (`BOOST_NAMED = 1000` vs `BOOST_GIT = 300` / `BOOST_MARKER = 150`).
- The wildmenu row for the named project shows the bound host as `@host`.
- Pressing `Enter` spawns the agent on the project's configured host with the project's path. For host-bound entries, this triggers the prepare-then-spawn path (AC-003 stages run first, then attach).
- The minibuffer's host badge does not change until the prepare path commits the bound host. Until then, the badge reflects the user-typed (or default) host.

**Failure modes:**
- **Bound host unreachable:** falls into AC-003's SSH failure modes (minibuffer returns to host zone with error, no slot created).
- **Path expands to a directory that no longer exists:** for local spawns, the spawn proceeds and the agent's PTY inherits the parent cwd (no pre-spawn `stat`); for remote spawns, the daemon surfaces an attach error and the slot enters `Failed`.

### AC-008: Cancel the spawn minibuffer

**Given:**
- The spawn minibuffer is open with text in either zone.

**When:**
1. Press `Esc`.

**Then:**
- If the path zone has an active wildmenu selection or in-progress filter chars, the first `Esc` clears those *first* without closing. A second `Esc` then closes the minibuffer.
- Otherwise (no selection, empty path field), the minibuffer closes immediately; no agent slot is created.
- On close, the previously-focused agent (if any) regains focus.
- No background work that was started by the minibuffer (index build, host probe) blocks the close; the worker `Drop` impls handle their own cleanup.

### AC-032: Spawn modal opens at the TUI startup cwd, not the focused agent's cwd

**Given:**
- codemux was launched in `~/work/proj-A`. The user has since spawned and focused a second agent in `~/work/proj-B`.

**When:**
1. With the `proj-B` agent focused, press the prefix, then `c`.

**Then:**
- The path zone opens with `~/work/proj-A/` auto-seeded (the TUI's startup cwd), not `~/work/proj-B/`.
- The host zone defaults to `local`.

**Why this is pinned:** users frequently expect "spawn here" to mean "spawn next to the focused agent." The current behavior is consistent and predictable, but contradicts that expectation. Pin so a future "spawn from focused" change is a deliberate rebinding, not an accident.

**Failure modes:** none.

### AC-033: Spawn modal swallows all keystrokes while open

**Given:**
- The spawn minibuffer is open.

**When:**
1. Press the prefix chord (e.g. `Ctrl+B`).
2. Press a direct chord (e.g. `Cmd+'`).
3. Press `?`.

**Then:**
- All three keystrokes are routed through the modal's keymap (`ModalAction`), not the runtime's prefix or direct dispatch. The prefix chord and `?` may produce typed characters in the path field; direct chords have no effect.
- No agent focus change happens. No help screen opens.
- The runtime's `dispatch_key` only runs after the modal is closed.

**Failure modes:** none.

### AC-045: Indexing runs in the background; input stays interactive

**Given:**
- The spawn modal is open in fuzzy mode, against either a local host or a remote host whose per-host index is not yet built.
- The indexer worker is running on its own thread (local: `index_worker.rs` walks via `read_dir`; remote: `index_worker.rs` walks via `RemoteFs` over the SSH `ControlMaster` socket).

**When:**
1. The user types into the path zone while the index is still building.
2. The user presses `Ctrl+T` to switch to precise mode.
3. The index finishes building mid-typing.

**Then:**
- The keystroke handler runs on every press regardless of indexer state. The path zone keeps accepting characters; `Backspace` works; `Ctrl+T` toggles modes; `Esc` cancels.
- While building, the wildmenu shows a spinner sentinel (e.g. `⠋ indexing...`, or `⠋ indexing... {count} dirs` once the worker reports progress). No fuzzy candidates appear, even if the partial index would match.
- Precise mode (Step 2) works synchronously: it bypasses the index and uses `read_dir` (local) or `RemoteFs::list_dir` (remote) directly. Candidates appear immediately, even while fuzzy is still building.
- The runtime drains indexer events via `index_mgr.tick()` once per ~50ms frame using `try_recv()`; this never blocks `dispatch_key`.
- When the index finishes (Step 3), the wildmenu does NOT auto-refresh the current query. The user must press one more keystroke (any character or `Backspace`) for the new index to populate the wildmenu. The dimmed "stale" wildmenu persists until then.

**Failure modes:**
- **Indexer panic or worker thread death:** the channel disconnects; the modal sees `IndexState::Building { count: 0 }` indefinitely. `Ctrl+R` forces a rebuild and re-spawns the worker.
- **Remote `find` subprocess fails or hangs:** the worker thread surfaces the error via the channel; subsequent ticks transition the state and surface a wildmenu error row. The modal stays usable in precise mode.

---

## Navigation

### AC-009: Cycle focus between agents

**Given:**
- Three agents are spawned: `A` (focused), `B`, `C`.

**When:**
1. Press the prefix, then `n` (or `l`, `j`, `→`, `↓`; all are aliases per the keymap).

**Then:**
- Focus advances to `B`. The tab strip updates; the previous tab loses its focus indicator.
- No `SIGWINCH` fires on focus change. Pane geometry is shared across all agents per chrome style (`pty_size_for(style, term_rows, term_cols)`); resizes only run on terminal resize and chrome toggle (see AC-013).
- Pressing the chord again advances to `C`; once more wraps back to `A`.
- The mirrored chords `p|h|k|←|↑` walk the cycle in the opposite direction.
- The direct chords `Cmd+'` (next) and `Cmd+;` (prev) walk the same cycle without the prefix.

**Failure modes:**
- **Only one agent exists:** the chord is a no-op.

### AC-010: Focus an agent by ordinal digit

**Given:**
- Five agents are spawned in slots 1–5.
- Agent 3 is focused.

**When:**
1. Press the prefix, then `1`.

**Then:**
- Focus jumps to agent 1.
- After a digit, the prefix is *sticky*: pressing `j`/`l`/`n`/digit again continues navigating without re-arming the prefix. Any non-nav keystroke (or `Esc`) drops out of the sticky state and forwards normally.

**Failure modes:**
- **Digit out of range** (e.g. `prefix 9` with 4 agents): the chord is a no-op, and the prefix state *stays sticky* (the runtime classifies `FocusAt(d-1)` as a nav dispatch regardless of whether the index is valid). Press `Esc` or any non-nav key to drop out.

### AC-011: Bounce to the previously-focused agent

**Given:**
- Two agents `A` and `B`. The user just focused `B` from `A`.

**When:**
1. Press the prefix, then `Tab`.

**Then:**
- Focus returns to `A`. Pressing `Tab` again returns to `B`. The two-slot bounce is symmetric.

**Failure modes:**
- **Only one agent exists, or no prior focus is recorded:** the chord is a no-op.

### AC-012: Switcher popup picks an agent by name

**Given:**
- Four agents spawned with distinct labels.

**When:**
1. Press the prefix, then `w`.
2. Use `↑`/`↓` to highlight a row.
3. Press `Enter`.

**Then:**
- The popup centers over the screen and lists each agent. Each row shows: a working spinner (when the agent is mid-turn), the host prefix (for SSH agents only), an attention dot (when the agent has emitted output the user hasn't seen), and the agent's label.
- `Enter` focuses the highlighted agent and closes the popup.
- `Esc` closes the popup without changing focus.
- If an agent is reaped while the popup is open, `Enter` clamps to a still-valid index (`selection.min(agents.len()-1)`), so a stale highlight never focuses a removed slot.

**Failure modes:** none. The popup cannot open with zero agents because the runtime returns from `event_loop` the moment `nav.agents.is_empty()` (see AC-036).

### AC-013: Toggle the navigator chrome

**Given:**
- codemux launched in the default `Popup` style. Two agents are spawned.

**When:**
1. Press the prefix, then `v`.

**Then:**
- The chrome flips to `LeftPane`: a 25-column navigator on the left, focused PTY on the right.
- The PTY is `SIGWINCH`'d to the new pane width; Claude redraws to fit.
- Pressing the chord again returns to `Popup`.
- `--nav left-pane` or `CODEMUX_NAV=left-pane` selects the same chrome at launch.

**Failure modes:**
- **Terminal too narrow for `LeftPane` to render usefully** (≤ ~50 cols): the chrome still flips, but the agent pane may render degraded; the user can flip back.

### AC-034: Spawning a new agent records the prior focus as the bounce slot

**Given:**
- Agent `A` is focused. The user opens the spawn modal and confirms a spawn for a new agent `B`.

**When:**
1. The new agent `B` appears and takes focus.
2. Press the prefix, then `Tab` (`FocusLast`).

**Then:**
- Focus returns to `A`. The prior-focus pointer was set by the spawn-time `change_focus(new_idx)`, so `B`'s bounce slot is `A` immediately on first frame.

**Failure modes:**
- **There were no agents before the spawn:** `previous_focused` stays `None` after the spawn (no prior to record); `prefix Tab` is a no-op until the user manually focuses something else.

### AC-035: Reaping the focused agent moves focus to the new tail, not to `previous_focused`

**Given:**
- Three agents `[A, B, C]`. The user just bounced from `B` to `C` (so `previous_focused = 1`, i.e. `B`). Focus is on `C` (slot 2).

**When:**
1. Press the prefix, then `x` (kill `C`).

**Then:**
- The tab vector becomes `[A, B]`.
- Focus clamps to slot index 1 (the new tail, `B`), not the bounce slot. The bounce slot would also be `B` here, but the mechanism is index-clamp, not bounce.
- `previous_focused` is cleared because it equaled the killed slot.

**Why this is pinned:** the kill-then-clamp behavior is deliberate and tested (`kill_focused_clamps_focus_when_killing_last_tab`). It's distinct from the `prefix Tab` bounce path (AC-011), which *does* honor `previous_focused`. Pin so a future "smart" kill-focus refactor doesn't silently change behavior under a passing test suite.

**Failure modes:** none.

---

## Agent lifecycle

### AC-014: Force-close a live agent

**Given:**
- The focused agent is in the `Ready` state with an active PTY.

**When:**
1. Press the prefix, then `x`.

**Then:**
- The transport's `Drop` impl reaps the child (local fork) or closes the daemon socket (SSH).
- The tab disappears from the navigator.
- Focus clamps to the same slot index in the new (shorter) tab list, i.e. the agent immediately to the right of the killed one becomes focused. If the killed tab was the last one, focus moves to the new last slot. (`previous_focused` is *not* preferred when the focused tab itself is the one removed; see AC-035.)

**Failure modes:**
- **Reap of a wedged transport** (e.g. a stuck remote SSH tunnel): `child.kill()` + `child.wait()` (or socket close) is best-effort and may block the calling thread. There is no cleanup timeout today; a fully wedged process can stall the tab removal until killed externally.

### AC-015: Dismiss a crashed or failed agent (no-op on live)

**Given:**
- Agent `A` is `Ready`. Agent `B` is `Failed` (bootstrap error). Agent `C` is `Crashed` (PTY died after Ready).

**When:**
1. Focus `A` and press the prefix, then `d`.
2. Focus `B`, press the prefix, then `d`.
3. Focus `C`, press the prefix, then `d`.

**Then:**
- Step 1 is a no-op: `A` stays focused and live. (`d` is the no-risk corpse-clearing key by design.)
- Step 2 removes `B`'s tab and the bootstrap error pane.
- Step 3 removes `C`'s tab and the frozen-at-death pane.

**Failure modes:** none. The gating against `Ready` is the design, not a failure mode.

### AC-016: Quit codemux cleanly

**Given:**
- Three agents are running, two with scrollback offset > 0.

**When:**
1. Press the prefix, then `q`.

**Then:**
- All transports are dropped (children reaped, daemon sockets closed) by the `NavState` drop on `event_loop` return.
- The `TerminalGuard` drop impl runs the teardown, in order: bracketed paste disabled, focus-change disabled, mouse capture released (only if it was acquired), OSC 22 mouse-pointer reset, KKP enhanced-keyboard flags popped (only if pushed), host title restored, raw mode disabled, alt screen exited.
- Stdin is then drained (`event::poll(Duration::ZERO)` loop) so KKP key-release bytes don't leak into the parent shell.
- The process exits 0 (no `std::process::exit`; the `Result<()>` chain unwinds cleanly through `main`).

**Failure modes:**
- **`child.wait()` blocks indefinitely** on a wedged child (e.g. uninterruptible sleep). The teardown is best-effort and never panics; a stuck reap will stall the codemux process until the user kills it externally. There is no cleanup timeout today.

### AC-036: Reaping the last agent auto-exits codemux

**Given:**
- One agent is focused. No others exist.

**When:**
1. The agent terminates (e.g. user types `/quit` inside Claude, or the user dismisses the last `Failed`/`Crashed` corpse with `prefix d`).

**Then:**
- The per-tick reap detects `nav.agents.is_empty()` and returns `Ok(())` from `event_loop`.
- The `TerminalGuard` drop runs the same teardown as AC-016 (alt screen exit, raw mode disable, mouse capture release, KKP pop, title restore, stdin drain).
- The process exits 0.

**Why this is pinned:** the auto-exit is silent. There's no "no agents left, quitting" toast. A user who didn't intend to drop the last agent might be surprised. Pin so a future "stay open with empty navigator" change is deliberate.

**Failure modes:** none.

### AC-037: A non-zero PTY exit transitions Ready → Crashed (not silent removal)

**Given:**
- A `Ready` agent is running.

**When:**
1. The PTY child exits with a non-zero status (or, for SSH agents, the daemon socket EOFs surfacing the `-1` sentinel).

**Then:**
- The reap loop calls `mark_crashed(agent)`. The transport is replaced with a synthetic dead one; the `Parser` is preserved so the user can scroll back through what the agent printed before it died.
- The pane shows a red crash banner with the configured dismiss chord (`d` by default).
- The tab stays in the navigator until dismissed.

By contrast, a clean `exit 0` triggers silent removal: the slot is reaped without ceremony, then AC-035 / AC-036 kicks in for focus / shutdown.

**Failure modes:** none.

### AC-038: A panic restores the terminal before the report is printed

**Given:**
- Codemux has entered raw mode.

**When:**
1. A panic occurs anywhere in the runtime (typically a bug; should not happen in normal use).

**Then:**
- The unwind drops the `TerminalGuard`, which runs the same teardown as AC-016 (alt screen, raw mode, mouse, KKP, title, stdin drain).
- The `color_eyre`-installed panic hook then prints the panic report to stderr in the now-cooked terminal.
- The user sees a readable stack trace, not garbled escape sequences over an alt-screen.

**Failure modes:**
- **Panic during `TerminalGuard::drop` itself:** the second panic aborts the process; the terminal may end up in a corrupted state. `Drop` impls in the guard are written to be infallible (no `unwrap`/`expect`), but external state (e.g. stdout closed) could still trigger this in theory.

---

## Scrollback

### AC-017: Enter scroll mode and navigate history

**Given:**
- The focused agent has produced enough output to fill its scrollback (default `scrollback_len` = 5000 rows).

**When:**
1. Scroll the mouse wheel up over the agent pane (one tick = `WHEEL_STEP` rows; default `WHEEL_STEP = 3`).
2. Press `PageUp`.
3. Press `g`.
4. Press `G`.

**Then:**
- Step 1 enters scroll mode: the floating "scroll N · esc" badge appears at the bottom-right of the pane; the visible rows shift up by `WHEEL_STEP` rows.
- Step 2 shifts up by one full page (`pane_rows - 1`, clamped to ≥ 1).
- Step 3 jumps to the top of scrollback (clamped to `scrollback_len`).
- Step 4 snaps back to the live view; the badge disappears.
- The PTY is **not** `SIGWINCH`'d at any point during this; scroll mode never re-lays-out the child. The only sites that resize agents are terminal-resize and chrome-toggle.

**Failure modes:**
- **Terminal does not deliver SGR mouse events** (e.g. Apple Terminal): wheel does nothing; `PageUp`/`PageDown`/`g`/`G` arrow-style bindings still work to enter scroll mode if the user can produce the chord some other way (most users will not; wheel is the realistic entry point).
- **`scrollback_len = 0`** (configured to disable history): the badge does not appear and the visible rows don't shift. Pinned by `runtime::tests::scrollback_zero_len_means_no_history`.

### AC-018: Typing snaps to live; navigation preserves scroll

**Given:**
- Agent `A` is scrolled back 50 lines (offset = 50).
- Agent `B` is at offset 0.

**When:**
1. With `A` focused, type any printable character.
2. Scroll `A` back to offset 50 again.
3. Press the prefix, then `2` (focus `B`).
4. Press the prefix, then `1` (focus `A` again).

**Then:**
- Step 1 snaps `A` to offset 0 *before* the byte is forwarded; the user never types into a window they can't see.
- Step 3 leaves `A`'s offset at 50 (navigation chords are non-snapping).
- Step 4 returns to `A` with the same offset 50 (per-agent state is implicit in each agent's `Parser`).

**Failure modes:** none. Both behaviors are pinned by tests in `apps/tui/src/runtime.rs`.

### AC-039: Pasting while scrolled-back snaps to live before the bracketed-paste write

**Given:**
- The focused agent is scrolled back (offset > 0).

**When:**
1. The user pastes (terminal sends a `Event::Paste` event with the bracketed-paste content).

**Then:**
- The runtime calls `snap_to_live()` *before* writing the paste payload to the PTY (same ordering rule as typing; see AC-018).
- The user never pastes into a window they cannot see.

**Failure modes:** none.

---

## Mouse

### AC-019: Click a tab to focus it

**Given:**
- Three agents in the navigator. The mouse is over a tab that is not focused.

**When:**
1. Left-click the tab.

**Then:**
- Focus moves to the clicked agent. The PTY is `SIGWINCH`'d to its pane.
- The hitbox is recorded by the renderer (`TabHitboxes`), so the click resolves to an agent *id*, not a slot index. A background reorder between press and release does not misroute the click.

**Failure modes:**
- **Click misses every tab hitbox:** no-op.

### AC-020: Drag a tab to reorder

**Given:**
- Agents `[A, B, C]`, focus on `B`.

**When:**
1. Press and hold left button on `A`'s tab.
2. Drag onto `C`'s tab slot.
3. Release.

**Then:**
- Order becomes `[B, C, A]` (browser-tab semantics: `remove(from) + insert(to)`, not swap).
- `B` stays focused; the focus index shifts with the moved agent so identity is preserved across the reorder.
- Same gesture works in both `Popup` (tabs in the status strip) and `LeftPane` (rows in the side nav).

**Failure modes:**
- **Release outside any tab hitbox:** drag cancels; no reorder.
- **The dragged agent is reaped mid-drag:** release resolves to `None` (`agents.iter().position(|a| a.id == id)`); the gesture cancels silently.

### AC-021: Drag-to-select and copy via OSC 52

**Given:**
- The terminal supports OSC 52 (iTerm2, Ghostty, Alacritty, WezTerm, Kitty; Apple Terminal does not).
- Agent pane has visible text.

**When:**
1. Press and hold left button on a starting cell inside the pane.
2. Drag to an ending cell.
3. Release.

**Then:**
- The selected cell range renders in reverse-video on each frame.
- On release, the extracted text (computed via `vt100::Screen::contents_between` for `Ready`/`Crashed` agents, or `failure_text_in_range` for `Failed`) is written to the system clipboard via `\x1b]52;c;<base64>\x07`.
- The clipboard write is silent: no toast on success or failure (the runtime only `tracing::debug!`s on failure).
- A frame later (focus change, agent reap, or terminal resize), the selection clears.

**Failure modes:**
- **Terminal does not support OSC 52** (e.g. Apple Terminal): the escape is silently swallowed. There is no capability probe, so codemux cannot detect this case or warn the user. Documented fallback: `⌥/Alt-drag` bypasses mouse capture so the host terminal's native selection takes over.
- **Selection spans scrollback rows:** `contents_between` walks `visible_rows()` so scrolled-back content is included. No special-casing required.

### AC-040: Mouse events are suppressed while an overlay is open

**Given:**
- An overlay is open: spawn minibuffer, switcher popup, or help screen.

**When:**
1. The user clicks a tab, drags inside a pane, or scrolls the wheel.

**Then:**
- The entire `Event::Mouse` branch is gated on `no_overlay_active(...)` and returns early. Click does not focus a tab. Drag does not start a selection or reorder. Wheel does not enter scroll mode.
- `Esc` (or any other dismissal mechanism for the active overlay) closes the overlay. After that, the next mouse event is handled normally.

**Why this is pinned:** there is no "click outside to dismiss" behavior, no "click wildmenu candidate to select", no "click switcher row to focus". The keyboard is the sole control surface while an overlay is up. Pin so a future "let users click overlays" change is deliberate.

**Failure modes:** none.

### AC-041: Ctrl+click on a URL hands it to the OS opener; Ctrl+hover shows underline + hand cursor

**Given:**
- A pane contains rendered text that includes a URL (e.g. `https://example.com`). The user holds Ctrl and moves the mouse over the URL.

**When:**
1. With Ctrl held, the cursor moves over the URL cell range.
2. With Ctrl still held, the user left-clicks.

**Then:**
- Step 1: the runtime computes the URL hitbox (`compute_hover`) and renders the matched cells with an underline; the host terminal is asked to render a hand cursor (OSC 22) over those cells.
- Step 2: the runtime hands the URL to the OS opener (`xdg-open` on Linux; the `open` crate equivalent on macOS / Windows). On opener failure, the URL is copied to the clipboard via OSC 52 and a toast is shown.
- Hover state clears on focus change, agent reap, or Ctrl release.

**Failure modes:**
- **Opener fails (no `xdg-open`, etc.):** the URL is copied to the clipboard and a toast confirms. (URL-open is the one place toasts are wired today; see AC-021's toast-less clipboard contrast.)

---

## Status bar

### AC-022: Configured segments render in order

**Given:**
- `config.toml` has `[ui] status_bar_segments = ["model", "tokens", "worktree", "branch", "prefix_hint"]` (the shipped default order).
- The focused agent is local, in a git checkout, and has produced at least one Claude turn (so `tokens` has data).

**When:**
1. Render a frame on a wide terminal.

**Then:**
- The right side of the status bar shows the segments in the configured order, separated by ` │ ` (space-bar-space).
- `model`, `branch`, and `tokens` are populated by a background worker that polls every 2 s for the focused agent only (`POLL_INTERVAL = 2_000ms`); the worker only emits an event when a value changes, so a `/model` change inside Claude can take up to 2 s to appear.
- `model` reads `~/.claude/settings.json` for the alias and effort.
- `worktree` shows the repo basename (hidden when the cwd basename equals the repo basename); `branch` shows the current branch (hidden when on a default branch; see `[ui.segments.branch] default_branches`); `tokens` shows the latest used/total + percentage.
- `prefix_hint` shows the configured prefix chord and `?` for help.

**Failure modes:**
- **Unknown segment ID in config:** silently skipped on render, logged once at startup. Other segments still render. (Soft failure to keep config edits non-fatal.)
- **`~/.claude/settings.json` missing or unparseable:** `model` segment renders nothing; the rest are unaffected.
- **Focused agent is SSH:** `model`, `branch`, and `tokens` all skip (the worker only sets a target when the focused agent has a local cwd; remote agents never get scanned by the local worker).
- **Statusline JSON missing** (Claude has not yet written for this agent): `tokens` renders nothing; populates after the first turn.

### AC-023: Customize the status bar via config

**Given:**
- `config.toml` has a `[ui]` section.
- The known segment IDs are `model`, `tokens`, `repo`, `worktree`, `branch`, `prefix_hint` (the closed set per AD-29; adding a new segment requires a Rust change).

**When:**
1. Set `status_bar_segments = ["repo", "tokens", "branch", "prefix_hint"]` (a custom subset and order, dropping `model` and `worktree` and opting `repo` in).
2. Optionally tune a segment under `[ui.segments.branch] default_branches = ["main", "develop"]` or `[ui.segments.tokens]`.
3. Restart codemux.

**Then:**
- The right side of the status bar renders only the listed segments, in the listed order, separated by `│`.
- Segment-specific sub-config under `[ui.segments.<id>]` is fed into the matching segment when it is built; segments not in `status_bar_segments` ignore their sub-config blocks.
- Setting `status_bar_segments = []` disables the right-side block entirely (no segments, no separators, no `prefix_hint`).
- Omitting `status_bar_segments` falls back to the default order: `model, tokens, worktree, branch, prefix_hint` (`repo` is opt-in, not in defaults).

**Failure modes:**
- **Unknown segment ID** (e.g. `status_bar_segments = ["model", "uptime"]`): the unknown ID is logged once at startup with the list of known IDs and skipped; the rest of the list still renders. The config does NOT fail to load over a typo.
- **Sub-config with invalid values** (e.g. malformed TOML in `[ui.segments.branch]`): falls under AC-030; the process exits non-zero before raw mode.

### AC-024: Segments drop from the left under width pressure

**Given:**
- The same config as AC-022.

**When:**
1. Resize the host terminal narrow enough that the full segment stack does not fit.

**Then:**
- Segments are dropped one at a time, *from the left first*, until the remaining stack fits.
- `prefix_hint` (rightmost) is the last to drop, so the user always keeps the help anchor visible until there's literally no width.
- Resizing back wide re-adds the dropped segments in reverse order.

**Failure modes:** none. The algorithm is deterministic.

### AC-042: Status bar is hidden in `LeftPane` chrome

**Given:**
- codemux is in `LeftPane` chrome (either via `prefix v` toggle, `--nav left-pane`, or `CODEMUX_NAV=left-pane`).
- One or more agents are spawned.

**When:**
1. Render a frame.

**Then:**
- The bottom status bar is not rendered. `model`, `worktree`, `branch`, `tokens`, and `prefix_hint` are all hidden, including the prefix-chord help anchor.
- The user discovers their bindings via the help screen (`prefix ?`) instead.

**Why this is pinned:** users who toggle chrome with AC-013's `prefix v` and don't read the rendered output carefully may not notice the segments disappeared. Either change the renderer to show segments above the left-pane footer, or accept this as the design. The AC pins the current design.

**Failure modes:** none.

---

## Help

### AC-025: Help screen reflects the live keymap

**Given:**
- `config.toml` rebinds `prefix` to `cmd+b` and `on_prefix.spawn_agent` to `s`.

**When:**
1. Press `Cmd+B`, then `?`.

**Then:**
- A centered modal (64 columns × 50 rows, `Clear`-ed background) overlays the navigator and lists every action grouped by scope (`prefix`, `direct (no prefix)`, `in agent switcher popup`, `in spawn minibuffer`, `in scroll mode`).
- The header shows `prefix:  super+b` (the renderer always normalizes `cmd` → `super`); the spawn-agent line shows `s`.
- Unbound actions render with their default-fallback chord (no dimming; the renderer does not differentiate user-bound from fallback). The `keymap.rs` doc-comment promises a dimmed "configure to enable" treatment that the renderer does not currently deliver. Closing this gap is a follow-up.
- A separate `mouse:` section lists the gesture lines (`click`, `drag tab`, `drag pane`, `alt+drag`); it is not interleaved with the keystroke bindings. The `wheel` and `type` lines (in scroll mode) and the prefix-mode `1`–`9` digit-jump line are hardcoded string literals in the renderer (not derived from the `Bindings` POD).
- *Any* key dismisses (not only `Esc`), including the prefix key itself and `?`. Mouse events are ignored while the help is open.

**Failure modes:** none for the keymap-derived rows. They're generated from the same `Bindings` POD that the runtime dispatches against, so they cannot drift. The hardcoded literal rows (`1-9`, `wheel`, `type`, mouse gestures) *can* drift silently from real behavior; closing this would require either moving them into `Bindings` or pinning them with a snapshot test.

---

## Daemon

### AC-027: Daemon serves a screen-state snapshot on every attach

**Given:**
- A remote agent is `Ready` on host `H`. The user has produced enough output that the daemon's mirrored screen has visible content.

**When:**
1. A client connects (or reconnects) to the daemon's socket and completes the `Hello` / `HelloAck` handshake.

**Then:**
- The daemon's first `PtyData` frame after `HelloAck` is a snapshot built from `Screen::state_formatted`: clear (`\x1b[H\x1b[J`), then per-cell positioned text with attributes, then any active input modes.
- If the agent was on the alt screen, the snapshot is prefixed with `\x1b[?1049h` to put the client parser on the right surface.
- The daemon drains buffered live bytes under the parser lock before sending the snapshot, so no duplicate-replay follows.
- The client treats the snapshot as ordinary `PtyData`; no special-case decoder is needed. The local `vt100::Parser` reproduces the daemon's grid before the next live byte arrives. There is no blank-screen window.
- Geometry: the daemon resizes its mirrored parser to the new client's `Hello` rows/cols *before* `state_formatted` runs, so the snapshot is encoded for the new client's grid.

**Failure modes:**
- **Wire-protocol mismatch on reconnect:** daemon sends `Message::Error{VersionMismatch}` (see AC-003; slot enters `Failed`, no retry on this path).

**Note:** This AC pins the daemon-side contract verified by `apps/daemon/src/supervisor.rs` tests. End-to-end "quit codemux, restart, reconnect to a previously-spawned agent" requires AD-7 (P1 persistence); see AC-028.

### AC-028: Reattach to a remote agent across TUI restart

**Status:** Not currently implementable. This AC is a forward-looking spec for AD-7 (P1 persistence) plus agent-id stability work.

**Why blocked:**
- `daemon_agent_id_for(tui_pid, spawn_counter)` namespaces agent ids by the TUI's process pid. This is a deliberate bug fix to prevent the bootstrap from silently re-attaching to a surviving daemon's socket and replaying its captured Claude PTY snapshot.
- A new TUI process generates a new pid and therefore new agent ids; it cannot construct the prior session's id and reattach.
- There is no persistence layer recording `(host, agent_id, spawn_path)` for restoration on launch.

**Eventual behavior** (when AD-7 lands):

**Given:**
- A remote agent was `Ready` on host `H`. Codemux has persisted `(host, agent_id, spawn_path)` to its session store.

**When:**
1. Quit codemux without killing the daemon. `prefix q` drops the local client only; the daemon (started under `setsid -f`) keeps the PTY alive.
2. Restart codemux.
3. Codemux reads its session store and re-bootstraps each persisted agent.

**Then:**
- For each persisted agent, the bootstrap re-handshakes against the existing daemon; AC-027's snapshot serves the latest screen.

**Failure modes** (designed, not implemented):
- **Daemon was killed between sessions:** the bootstrap finds no live socket; the slot enters `Failed` with the bootstrap error. AD-7 must guard against the silent-respawn path: if the bootstrap finds the daemon dead but restartable, a fresh `Session::spawn` would launch a brand-new Claude under the old label, a vision-principle-6 violation ("no surprise resurrection"). The expectation is that AD-7 surfaces this as `Failed` with a "session lost" reason, not a silent restart.
- **Wire-protocol mismatch on reconnect:** see AC-003.

### AC-043: Daemon survives SSH disconnect via `setsid -f`

**Given:**
- A remote agent is `Ready`. The bootstrap launched `codemuxd` under `setsid -f` so the daemon is detached from the SSH session's process group.

**When:**
1. The SSH `ControlMaster` socket dies (network drop, host sleep, SSH `~.`, or codemux exit).

**Then:**
- The daemon process keeps running on the remote. It has no controlling terminal and is reparented to PID 1, so a SIGHUP from the dying SSH session does not kill it.
- The Claude PTY child stays alive under the daemon.
- The next bootstrap attach (mid-session reconnect or AC-028's eventual TUI-restart attach) reconnects to the same daemon and gets the snapshot per AC-027.

**Failure modes:**
- **`setsid` not on the remote `$PATH`:** the bootstrap fails at `DaemonSpawn`; the slot enters `Failed`. (The bootstrap currently assumes a POSIX `setsid` is present.)

### AC-044: Stale daemon is killed and re-deployed on local-binary upgrade

**Given:**
- A remote daemon is running on host `H` with an older `bootstrap_version()` than the local binary.
- A `Ready` agent is currently attached to that daemon.

**When:**
1. The user spawns a new agent on the same host (which triggers `prepare_remote`).

**Then:**
- The probe step detects the version mismatch (`bootstrap_version()` differs from the remote's reported version).
- `prepare_remote` SIGTERMs the stale daemon, waits briefly, then SIGKILLs if needed.
- The killed daemon's PTY child (the in-flight Claude) dies with it. The pre-existing agent slot transitions to `Crashed`.
- The bootstrap re-deploys the matching daemon source, rebuilds, and spawns the new daemon.
- The new spawn proceeds normally against the fresh daemon.

**Why this is pinned:** the user-visible cost of an upgrade is "the in-flight Claude session on the remote dies." This is documented in code but a user upgrading codemux locally may not realize their remote work will be reaped on the next remote spawn.

**Failure modes:**
- **The stale daemon refuses SIGTERM and SIGKILL:** the bootstrap surfaces the kill error; the new spawn fails. (Should not happen for a normal `codemuxd` process.)

---

## Config and CLI

### AC-029: Missing config file falls back to defaults

**Given:**
- `$XDG_CONFIG_HOME/codemux/config.toml` does not exist (and neither does `~/.config/codemux/config.toml`), AND at least one of `$XDG_CONFIG_HOME` or `$HOME` is set so the lookup path can be resolved.

**When:**
1. Start codemux normally.

**Then:**
- The TUI starts with the default `Bindings`, default `[ui]` segment list, and default scrollback length (`scrollback_len = 5000`).
- No warning, no error on stderr (the missing-file branch logs only at `tracing::debug!`, which is suppressed by the default log filter).
- An empty file (zero bytes or whitespace only) is also treated as defaults; the deserializer accepts the empty input.

**Failure modes:**
- **Both `$XDG_CONFIG_HOME` and `$HOME` are unset:** the lookup fails loud with `$HOME is not set; cannot resolve config path` and the process exits non-zero before raw mode. (This is the one "missing-config" path that is *not* silent. There's no other writable path the runtime can fall back to.)

### AC-030: Invalid config fails loud before raw mode

**Given:**
- `~/.config/codemux/config.toml` exists but contains malformed TOML or an invalid value (e.g. `prefix = "ctrl+nonsense"`, or an unparseable hex color in `[ui.host_colors]`).

**When:**
1. Start codemux.

**Then:**
- The process exits non-zero before the terminal switches to raw mode (config-load runs in `main` before `enable_raw_mode`).
- Stderr contains a `color_eyre`-formatted error: a one-line summary plus the chained-cause stack. The path of the offending file is in the wrap context (e.g. `parse config at /home/.../config.toml`) and the offending key/value is in the inner cause from the deserializer (e.g. `unknown key code: nonsense` or `invalid hex color "ggffaa"; expected #rrggbb (six hex digits)`). The exact rendering is `color_eyre`'s default: multi-line with a backtrace when `RUST_BACKTRACE` is set, single-cause when it isn't.
- The user's terminal is left in its pre-launch state. No leftover alt-screen, no leftover raw mode.

**Failure modes:**
- **Unknown top-level keys are NOT errors.** `Config` is deliberately not `#[serde(deny_unknown_fields)]`; see the comment in `apps/tui/src/config.rs` trading typo-safety for forward-compat. A typo like `[bindng]` parses fine and silently binds nothing. Use `RUST_LOG=codemux=debug` to see what the deserializer accepted.

### AC-031: Invalid `[PATH]` arg fails loud before raw mode

**Given:**
- The user invokes `codemux /tmp/does-not-exist` or `codemux /etc/passwd`.

**When:**
1. Run the command.

**Then:**
- Exit non-zero before raw mode.
- Stderr message contains either ``invalid path `<path>` `` (missing; uses the user-supplied path) or `` `<path>` is not a directory `` (existing file; uses the *canonicalized* path because `fs::canonicalize` resolved it before the dir check). Note: backticks, not single quotes.
- `--nav <invalid>` fails at clap parse time with the standard `error: invalid value '<x>' for '--nav <NAV>' [possible values: left-pane, popup]`.

**Failure modes:** none.
