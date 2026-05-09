# Acceptance criteria

## Spawn

### AC-001: Spawn the initial agent at launch

**Given:**
- codemux is launched from a shell whose current pwd is a valid directory.

**When:**
1. Run `codemux` with no positional argument (or `codemux <PATH>` where `<PATH>` is a valid directory).

**Then:**
- The TUI starts with one tab already in the navigator and focused; no minibuffer opens.
- The agent's cwd is the shell's pwd, or the canonicalized `<PATH>` if one was passed (relative paths resolve against the shell's pwd).
- The pane renders Claude's prompt screen within ~2 s.

**Failure modes:**
- **Claude binary not on `$PATH`:** the initial tab enters the `Failed` state; the pane shows a crash banner with the spawn error.
- **Invalid `<PATH>` argument** (missing or not a directory): see AC-028; the process exits non-zero before raw mode.

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
- The pane renders Claude's prompt screen within ~2 s.

**Failure modes:**
- **Claude binary not on `$PATH`:** the new tab enters the `Failed` state; the pane shows a crash banner with the spawn error; the navigator stays interactive.
- **Scratch path cannot be resolved or created** (e.g. `scratch_dir` is neither absolute nor `~`-prefixed, or `mkdir -p` fails): the runtime logs a tracing diagnostic and falls back to the platform default cwd so the user still gets an agent.

### AC-003: Spawn a remote agent over SSH (cold-start bootstrap)

**Given:**
- A reachable SSH host the user has never spawned an agent on (no `~/.cache/codemuxd/` on the remote).
- The host appears in `~/.ssh/config` or in codemux's saved hosts.

**When:**
1. Press the prefix, then `c`.
2. Press `@`, type the host name, press `Tab`.
3. Pick a remote path from the wildmenu, press `Enter`.

**Then:**
- The path zone locks with a stage indicator that walks through `prepare: fetch uname → prepare: scp daemon → prepare: handshake → attach`.
- A new tab appears once the daemon HELLO_ACK arrives.
- The pane renders Claude's prompt screen.
- A subsequent spawn on the same host skips the scp stage (daemon already cached).

**Failure modes:**
- **SSH auth fails:** the minibuffer returns to the host zone with an error; no agent slot created.
- **`uname` succeeds but no daemon binary is bundled for that target:** the slot enters `Failed`; the pane shows the bootstrap error verbatim.
- **Wire-protocol version mismatch** (cached daemon is older than the local binary expects): daemon disconnects with `ERROR`; codemux re-deploys the matching daemon and retries once. If the retry still mismatches, the slot ends in `Failed`.
- **Remote path does not exist:** daemon sends `ERROR` on attach; slot enters `Failed` with the daemon's error message.

### AC-004: Path-zone wildmenu autocompletes against the focused host

**Given:**
- The spawn minibuffer is open.
- The host zone holds a value (`local` or a committed remote host).

**When:**
1. Type a partial path fragment in the path zone (e.g. `code`).

**Then:**
- The wildmenu shows up to 6 candidates ranked by the active search mode (fuzzy by default; precise after `Ctrl+T`).
- `Down`/`Up` move the highlight; `Tab` applies the highlighted candidate to the path field.
- For a remote host, candidates are read from the daemon's index of the remote filesystem, not the local one.

**Failure modes:**
- **Directory scan exceeds the cap** (1024 entries): the wildmenu shows the first 1024 and a "more results truncated" hint; the user can refine the query to narrow.
- **Index not yet built** (fuzzy mode, fresh session): the wildmenu shows precise-mode candidates as a fallback while the index builds; `Ctrl+R` forces a rebuild.

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
- The named project appears at the top of the wildmenu, score-boosted above any fuzzy-matched directory.
- Pressing `Enter` spawns the agent on the project's configured host with the project's path.
- The host badge in the minibuffer reflects the project's bound host.

**Failure modes:**
- **Bound host unreachable:** falls into AC-003's SSH failure modes (minibuffer returns with error, no slot created).
- **Path expands to a no-longer-existing directory:** the minibuffer shows a path-zone error and stays open.

### AC-008: Cancel the spawn minibuffer

**Given:**
- The spawn minibuffer is open with text in either zone.

**When:**
1. Press `Esc`.

**Then:**
- The minibuffer closes; no agent slot is created.
- The previously-focused agent (if any) regains focus.
- No background work that was started by the minibuffer (index build, host probe) blocks the close.

---

## Navigation

### AC-009: Cycle focus between agents

**Given:**
- Three agents are spawned: `A` (focused), `B`, `C`.

**When:**
1. Press the prefix, then `n` (or `l`, `j`, `→`, `↓`; all are aliases per the keymap).

**Then:**
- Focus advances to `B`. The tab strip updates; the previous tab loses its focus indicator.
- The PTY is `SIGWINCH`'d to `B`'s pane geometry.
- Pressing the chord again advances to `C`; once more wraps back to `A`.
- The mirrored chords `p|h|k|←|↑` walk the cycle in the opposite direction.
- The direct chords `Cmd+'` (next) and `Cmd+;` (prev) walk the same cycle without the prefix.

**Failure modes:**
- **Only one agent exists:** the chord is a no-op; the PTY does not get spurious `SIGWINCH`.

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
- **Digit out of range** (e.g. `prefix 9` with 4 agents): the chord is a no-op; the prefix state still drops to `Idle`.

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
- The popup centers over the screen and lists each agent's label, host, and status.
- `Enter` focuses the highlighted agent and closes the popup.
- `Esc` closes the popup without changing focus.

**Failure modes:**
- **No agents exist:** the popup opens with an empty list and a "no agents" hint; `Esc` dismisses.

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
- Focus moves to the previously-focused agent (or to the next slot if there was none).

**Failure modes:**
- **Reap takes longer than the frame budget** (rare, e.g. a wedged remote SSH tunnel): the tab is removed immediately; cleanup proceeds in the background.

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
- All transports are dropped (children reaped, daemon sockets closed).
- The terminal exits the alt screen, raw mode is disabled, mouse capture is released, the Kitty Keyboard Protocol flags are popped, the host title is restored.
- Stdin is drained so KKP key-release bytes don't leak into the parent shell.
- The process exits 0.

**Failure modes:**
- **A child fails to reap within the cleanup timeout:** the process still exits; the orphan is logged to `~/.cache/codemux/logs/codemux.log`.

---

## Scrollback

### AC-017: Enter scroll mode and navigate history

**Given:**
- The focused agent has produced enough output to fill its scrollback (default `scrollback_len` = 5000 rows).

**When:**
1. Scroll the mouse wheel up over the agent pane (one tick = one line).
2. Press `PageUp`.
3. Press `g`.
4. Press `G`.

**Then:**
- Step 1 enters scroll mode: the floating "scroll N · esc" badge appears at the bottom-right of the pane; the visible rows shift up by one.
- Step 2 shifts up by one full page (the agent's row count).
- Step 3 jumps to the top of scrollback.
- Step 4 snaps back to the live view; the badge disappears.
- The PTY is **not** `SIGWINCH`'d at any point during this; scroll mode never re-lays-out the child.

**Failure modes:**
- **Terminal does not deliver SGR mouse events** (e.g. Apple Terminal): wheel does nothing; arrow keys still work as scroll bindings.
- **Claude switched to the alt screen** (does not happen today, guarded by a regression test): scrollback is empty; the badge would still appear but rows don't shift. The test in `runtime::tests::scrollback_zero_len_means_no_history` exists to catch this.

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
- On release, the extracted text (computed via `vt100::Screen::contents_between`) is written to the system clipboard via `\x1b]52;c;<base64>\x07`.
- A toast confirms the copy (or, on failure, an error toast).
- A frame later (focus change, agent reap, or terminal resize), the selection clears.

**Failure modes:**
- **Terminal does not support OSC 52:** an error toast appears: "Selection failed: clipboard unavailable". The user's documented fallback is `⌥/Alt-drag` to bypass mouse capture and use native terminal selection.
- **Selection spans scrollback rows:** `contents_between` walks `visible_rows()` so scrolled-back content is included. No special-casing required.

---

## Status bar

### AC-022: Configured segments render in order

**Given:**
- `config.toml` has `[ui] status_bar_segments = ["model", "worktree", "branch", "tokens", "prefix_hint"]`.
- The focused agent is local, in a git checkout, and has produced at least one Claude turn (so `tokens` has data).

**When:**
1. Render a frame on a wide terminal.

**Then:**
- The right side of the status bar shows the segments in the configured order, separated by `│`.
- `model` reads `~/.claude/settings.json` for the alias and effort (one poll per cycle, focused agent only).
- `worktree` shows the repo basename; `branch` shows the current branch; `tokens` shows the latest used/total + percentage.
- `prefix_hint` shows the configured prefix chord and `?` for help.

**Failure modes:**
- **Unknown segment ID in config:** silently skipped on render, logged once at startup. Other segments still render. (Soft failure to keep config edits non-fatal.)
- **`~/.claude/settings.json` missing or unparseable:** `model` segment renders nothing; the rest are unaffected.
- **Focused agent is SSH:** `model` and `branch` skip (v1 reads only the local user's settings/git, which don't reflect the remote agent's state).
- **Statusline JSON missing** (Claude has not yet written for this agent): `tokens` renders nothing; populates after the first turn.

### AC-023: Segments drop from the left under width pressure

**Given:**
- The same config as AC-022.

**When:**
1. Resize the host terminal narrow enough that the full segment stack does not fit.

**Then:**
- Segments are dropped one at a time, *from the left first*, until the remaining stack fits.
- `prefix_hint` (rightmost) is the last to drop, so the user always keeps the help anchor visible until there's literally no width.
- Resizing back wide re-adds the dropped segments in reverse order.

**Failure modes:** none. The algorithm is deterministic.

---

## Help

### AC-024: Help screen reflects the live keymap

**Given:**
- `config.toml` rebinds `prefix` to `cmd+b` and `on_prefix.spawn_agent` to `s`.

**When:**
1. Press `Cmd+B`, then `?`.

**Then:**
- A full-screen modal lists every binding grouped by scope (`Prefix`, `Direct`, `Popup`, `Modal`, `Scroll`).
- The displayed prefix chord is `Cmd+B`; the spawn-agent line shows `s`.
- Unbound actions (e.g. `direct.focus_last` by default) render dimmed with a "configure to enable" hint.
- The mouse-gesture lines (`click`, `drag`) are listed alongside the keystroke bindings.
- `Esc` (or any key not bound in the help scope) dismisses.

**Failure modes:** none. The help screen is generated from the same `Bindings` POD that the runtime dispatches against, so they cannot drift.

---

## Daemon

### AC-025: Reattach replays the screen state

**Given:**
- A remote agent is `Ready` on host `H`. The user has produced enough output that the visible grid is not empty.

**When:**
1. Quit codemux without killing the agent. `prefix q` only drops the local client; the daemon keeps the PTY alive.
2. Restart codemux.
3. Reconnect to the same agent slot (P1 persistence brings it back).

**Then:**
- The daemon's first `PtyData` frame after the new client's `HELLO/HELLO_ACK` is a snapshot built from `Screen::state_formatted`: clear, then per-cell positioned text with attributes, then any active input modes.
- The client's pane renders the snapshot before any live bytes arrive. No blank-screen window.
- If the agent was on the alt screen, the snapshot is prefixed with `?1049h` to put the client parser on the right surface.

**Failure modes:**
- **Daemon was killed between sessions:** reattach fails with `ERROR` from the bootstrap path; the slot enters `Failed`. No silent resurrection per vision principle 6 ("no surprise resurrection").
- **Wire-protocol version mismatch on reconnect:** AC-003's mismatch failure mode applies.

---

## Config and CLI

### AC-026: Missing config file falls back to defaults

**Given:**
- `$XDG_CONFIG_HOME/codemux/config.toml` does not exist (and neither does `~/.config/codemux/config.toml`).

**When:**
1. Start codemux normally.

**Then:**
- The TUI starts with the default `Bindings`, default `[ui]` segment list, and default scrollback length.
- No warning, no error. Defaults are the documented fallback.

**Failure modes:** none.

### AC-027: Invalid config fails loud before raw mode

**Given:**
- `~/.config/codemux/config.toml` exists but contains malformed TOML or an invalid value (e.g. `prefix = "ctrl+nonsense"`, or an unparseable hex color in `[ui.host_colors]`).

**When:**
1. Start codemux.

**Then:**
- The process exits non-zero before the terminal switches to raw mode.
- Stderr contains a readable, single-paragraph error citing the file path and the offending key/value.
- The user's terminal is left in its pre-launch state. No leftover alt-screen, no leftover raw mode.

**Failure modes:** none. The loud-fail is the design.

### AC-028: Invalid `[PATH]` arg fails loud before raw mode

**Given:**
- The user invokes `codemux /tmp/does-not-exist` or `codemux /etc/passwd`.

**When:**
1. Run the command.

**Then:**
- Exit non-zero before raw mode.
- Stderr message contains either `invalid path '<path>'` (missing) or `'<path>' is not a directory` (file).
- `--nav <invalid>` fails at clap parse time with a list of valid choices.

**Failure modes:** none.
