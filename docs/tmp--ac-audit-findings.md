# AC audit findings (temporary)

Audit of `003--acceptance-criteria.md` against the actual codebase. Nine parallel agents, one per section. Each section reports per-AC status (Implemented / Partial / Drift / Missing), suggested new ACs, and cross-cutting notes.

This doc is a working artifact for deciding which ACs to fix, which code to fix, and which behaviors to add to the spec. Delete once decisions are made.

---

## Headline issues

The drifts most worth deciding on first:

1. **AC-014 vs AC-NEW-N03 — focus-after-kill**. AC says bounce-to-previous; code does next-slot-clamp, with a test pinning the clamp. Both lifecycle and navigation agents flagged this. Pick one.
2. **AC-026 — daemon reattach is structurally impossible today**. Agent IDs are namespaced by TUI pid (`apps/tui/src/runtime.rs:1303-1311`) by deliberate bug fix. The "quit + restart + reconnect" scenario the AC describes cannot be exercised. Recommend splitting into AC-026a (mid-session reconnect, testable now) and AC-026b (TUI-restart, blocked on AD-7 / P1 persistence).
3. **AC-003 — bootstrap stages and design**. AC describes per-arch prebuilt binaries with stages `fetch uname → scp daemon → handshake → attach`. Code ships a source tarball and runs `cargo build` on the remote with stages `VersionProbe → TarballStage → Scp → RemoteBuild → DaemonSpawn → SocketTunnel → SocketConnect`. Reconcile or migrate.
4. **AC-004 — remote autocomplete ownership**. AC says daemon owns the index; code has the TUI's `index_manager.rs` walk via `RemoteFs` over an SSH ControlMaster. Daemon has no path-index endpoint.
5. **WHEEL_STEP cross-section**. AC-017 says "one tick = one line"; code has `WHEEL_STEP = 3` (`apps/tui/src/runtime.rs:90`). Mouse and scrollback agents both flagged.
6. **AC-021 — clipboard toasts**. The toast-on-success/failure claim is not implemented; OSC 52 capability detection is unimplementable without a terminfo probe.
7. **AC-025 — help screen drifts**. Not full-screen (centered 64×50). "Unbound dimmed with 'configure to enable'" is missing entirely. ~⅓ of rows are hardcoded strings, not bindings-driven.
8. **AC-028 — config validation**. `Config` is not `deny_unknown_fields` by deliberate design. Typos silently default. Either pin or flip the policy.
9. **AC-022 vs AC-023** disagree on shipped default segment order. Shipped default matches AC-023.
10. **No signal handlers anywhere**. SIGTERM/SIGHUP from outside corrupt the terminal. No AC pins current behavior either way.

Counts:

| Section | Implemented | Partial | Drift | Missing |
|---|---|---|---|---|
| Spawn (8) | 2 | 2 | 4 | 0 |
| Navigation (5) | 2 | 1 | 2 | 0 |
| Lifecycle (3) | 1 | 2 | 0 | 0 |
| Scrollback (2) | 1 | 0 | 1 | 0 |
| Mouse (3) | 2 | 0 | 1 | 0 |
| Status bar (3) | 3 | 0 | 0 | 0 |
| Help (1) | 0 | 1 | 0 | 0 |
| Daemon (1) | 0 | 1 | 0 | 0 |
| Config/CLI (3) | 1 | 1 | 1 | 0 |

---

## Spawn (AC-001 — AC-008)

### Per-AC findings

- **AC-001: Spawn the initial agent at launch** — **Partial / Drift**.
  - Initial agent created in `apps/tui/src/runtime.rs:1180-1189` via `spawn_local_agent` with `Some(initial_cwd)`; positional `[PATH]` canonicalized in `apps/tui/src/main.rs:125-128, 135-145` (`resolve_cwd`). Both relative-to-pwd and validation behaviors check out (tests at `apps/tui/src/main.rs:208-239`).
  - **Drift**: AC says "Claude binary not on `$PATH` → initial tab enters `Failed` state." Code does the opposite — `spawn_local_agent` returns `Result<RuntimeAgent>` (`apps/tui/src/runtime.rs:1402-1419`); `run` propagates via `?` (`runtime.rs:1180`); process exits non-zero before raw mode. There is no "Failed initial agent" path. `AgentState::Failed` at `runtime.rs:865-866` carries `codemuxd_bootstrap::Error`, not local spawn errors.
  - "~2 s render" is aspirational — no timing guarantee in code.

- **AC-002: Spawn a local agent in the scratch directory** — **Implemented**.
  - `c` (prefix) and `Super+\` (direct) both bound (`apps/tui/src/keymap.rs:424, 759-762`); both route to `KeyDispatch::SpawnAgent` and open the modal (`runtime.rs:3548-3568`).
  - Empty-Enter on path zone emits `ModalOutcome::SpawnScratch { host }` (`apps/tui/src/spawn.rs:1090-1098`, test 3454-3465); runtime resolves via `resolve_local_scratch_cwd` → `SpawnConfig::local_scratch_dir` → `expand_scratch` and calls `mkdir -p` (`runtime.rs:3319-3361`, `1821-1828`, `apps/tui/src/config.rs:497-509, 521-539`).
  - Default `~/.codemux/scratch` matches (`config.rs:573`, test 1495-1499).
  - Failure mode: `expand_scratch` warns and returns `None`; runtime falls back to `cwd_path = None` so PTY inherits parent cwd.

- **AC-003: Spawn a remote agent over SSH (cold-start bootstrap)** — **Drift / Partial**.
  - Modal flow exists: `@` then host then `Tab` commits → `ModalOutcome::PrepareHost` (`spawn.rs:933-936`); SSH config loaded (`apps/tui/src/ssh_config.rs`).
  - **Stage labels disagree**. AC says `prepare: fetch uname → prepare: scp daemon → prepare: handshake → attach`. Code emits `VersionProbe → TarballStage → Scp → RemoteBuild → DaemonSpawn → SocketTunnel → SocketConnect` (`crates/codemuxd-bootstrap/src/error.rs:29-58`, labels at 78-88: `"probing host" | "preparing source" | "uploading source" | "building remote daemon" | "spawning daemon" | "opening tunnel" | "connecting"`). No "fetch uname" stage. `RemoteBuild` step missed entirely.
  - **"Subsequent spawn skips scp"** matches: `prepare_remote` at `crates/codemuxd-bootstrap/src/lib.rs:316-325` checks `installed_version` and short-circuits the install stages on a match.
  - **Wire-protocol-mismatch retry-once is aspirational**. `crates/wire/src/messages.rs:111` defines `ErrorCode::VersionMismatch`, but no code in `crates/codemuxd-bootstrap/` or `apps/tui/` triggers a re-deploy on receiving it.
  - Auth-fail / daemon-spawn-error verbatim are implemented via `unlock_back_to_host(Some(err))` at `spawn.rs:623-637` and `BootstrapError::user_message` at `error.rs:135-174`. "No daemon binary bundled for that target" is moot — current `RemoteBuild` step compiles from source on the remote.

- **AC-004: Path-zone wildmenu autocompletes against the focused host** — **Partial / Drift**.
  - `WILDMENU_ROWS = 7`, `usable = 6` at `spawn.rs:101, 1627`. **Cap on candidates** is `MAX_COMPLETIONS = 8` for precise local/remote (`spawn.rs:103`) and `MAX_FUZZY_RESULTS = 50` for fuzzy (`spawn.rs:109`). AC's "up to 6" conflates visible row budget with cap.
  - Search modes: fuzzy default, `Ctrl+T` toggle (`keymap.rs:582`, modal action at `spawn.rs:1285-1304`).
  - Down/Up navigate, Tab applies (`spawn.rs:722-729, 933-957`).
  - **Drift on "remote candidates from daemon's index of the remote filesystem"**: there is no daemon-side path index. Remote precise mode is `RemoteFs::list_dir` — per-keystroke `ssh -S {socket} ls` over the existing `ControlMaster` (`crates/codemuxd-bootstrap/src/remote_fs.rs:335-413`); fuzzy remote candidates come from a TUI-side per-host index walking remote roots through `RemoteFs`, coordinated by `apps/tui/src/index_manager.rs`. Neither path involves the daemon process.
  - **1024 cap matches** for local synchronous scan: `MAX_SCAN_ENTRIES = 1024` at `spawn.rs:129`, applied in `scan_dir` line 2345. Remote `list_dir` has its own cap, `MAX_LIST_ENTRIES`, in `remote_fs.rs:56`. AC's "more results truncated hint" is **not implemented** — the cap silently truncates (`scan_dir` just `.take(cap)`).
  - Index-not-yet-built / `Ctrl+R` rebuild: implemented (`ModalAction::RefreshIndex` at `keymap.rs:583`, runtime at `runtime.rs:3145-3183`). The "precise-mode fallback while index builds" is not how it works — in fuzzy, the wildmenu shows the indexer-state sentinel (`spawn.rs:1735-1771`), not precise-mode candidates.

- **AC-005: Quick-switch to precise mode by typing `~` or `/`** — **Implemented**.
  - Detection at `spawn.rs:777-791` covers both `~`, U+02DC, U+0303 plus `/`; `enter_navigation_mode_with_seed` (`spawn.rs:824-853`) seeds `/` or local/remote `$HOME`; falls back to literal `~/` when `$HOME` unset. `user_search_mode` intentionally not touched (`spawn.rs:815-817`).
  - Test for slash variant at `spawn.rs:3402-3406`. Tilde-compose-armed swallow space at `spawn.rs:752-754`.

- **AC-006: Drill into a folder, then spawn at the chosen depth** — **Implemented**.
  - `apply_path_completion` at `spawn.rs:972-990` performs the descend (sets path to candidate, clears selection, sets `just_descended` for one frame); `swap_field_outcome` routes Tab to it in path zone (`spawn.rs:933-956`); `confirm` routes Enter-with-selection-in-precise to it (`spawn.rs:1069-1075`); `confirm` with no selection emits `Spawn` (`spawn.rs:1100-1134`).
  - Fuzzy-mode-Tab-is-no-op at `spawn.rs:951`; fuzzy-mode-Enter-applies-and-spawns at `spawn.rs:1069`. Tests at `spawn.rs:3576-3604`.

- **AC-007: Saved project alias resolves through the minibuffer** — **Partial / Drift**.
  - `[[spawn.projects]]` parsed (`config.rs:434, 593-598`); modal carries the list (`spawn.rs:398, 473-490`); fuzzy scoring with `BOOST_NAMED = 1000` (`spawn.rs:115, 2066-2127`); host-bound entries route to `PrepareHostThenSpawn` at `spawn.rs:1128-1133`.
  - **Drift**: AC says "host badge in the minibuffer reflects the project's bound host." Inspecting `prompt_view` (`spawn.rs:1787-1912`) — the host span renders `self.host`, which is whatever the user typed, not the project's bound host. The bound host only surfaces in (a) the wildmenu row's `@host` badge via `named_project_row` (`spawn.rs:2604-2666`) and (b) the locked-spinner status row after `PrepareHostThenSpawn` triggers a prepare. Pre-Enter, the host badge does not change.
  - "Above any fuzzy-matched directory" → confirmed by `BOOST_NAMED = 1000` vs `BOOST_GIT = 300` / `BOOST_MARKER = 150` (`spawn.rs:115-125`); test at `spawn.rs:4979-4997`.
  - Failure: "Path expands to no-longer-existing directory → minibuffer shows error and stays open" — **not implemented**. `confirm` doesn't `stat` the path before emitting `Spawn`.

- **AC-008: Cancel the spawn minibuffer** — **Implemented (with a sub-state caveat)**.
  - `Esc` action `ModalAction::Cancel` (default `keymap.rs:577`); modal handler `spawn.rs:900-919`; runtime tear-down `runtime.rs:3184-3194` drops `spawn_ui`, drops `prepare`, cancels modal-owned attach via `cancel_modal_owned_attach` (`runtime.rs:1932-1939`).
  - "No background work blocks the close" structurally true (workers' `Drop` impl handles cleanup; runtime doesn't `join`).
  - Caveat: in path zone with selection or filter chars, the **first Esc** is "back out of selection/search" (`spawn.rs:905-917`), not close — the AC's "press Esc, modal closes" is only the second-Esc behavior in that sub-state.

### Suggested new ACs (Spawn)

- **AC-NEW-S01: Initial-spawn failure exits non-zero before raw mode** — pin actual current behavior. `runtime.rs:1180-1189`, `main.rs:93-130`.
- **AC-NEW-S02: Spawn modal always opens at the TUI's startup cwd** — `SpawnMinibuffer::open(initial_cwd, ...)` at `runtime.rs:3563-3567` uses `initial_cwd`, never the focused agent's cwd. Surprising for users who expect "spawn here".
- **AC-NEW-S03: Tab in host zone with empty / `local` text is a zone toggle, not a commit** — `swap_field_outcome` at `spawn.rs:938-945`. Contrast with non-empty non-`local` Tab (`spawn.rs:935-936`).
- **AC-NEW-S04: `@` in path zone clears auto-seeded path but preserves user-typed path** — `enter_host_zone` at `spawn.rs:1176-1183`, reads `path_origin`. Tests `spawn.rs:3863-3870`.
- **AC-NEW-S05: Modal swallows all keystrokes while open** — prefix-state never advances, direct chords don't fire while `spawn_ui.is_some()` (`runtime.rs:3129-3143`).
- **AC-NEW-S06: Esc in path zone with active selection/filter clears those first, then second Esc closes** — `spawn.rs:900-919`. Contradicts AC-008's "single Esc closes".
- **AC-NEW-S07: Relative paths in modal Spawn are NOT canonicalized** — `runtime.rs:3251-3255` passes `Path::new(&path)` straight to `spawn_local_agent`; only CLI positional arg is `canonicalize`d (`main.rs:135`).
- **AC-NEW-S08: Scratch dir relative-path config silently degrades to platform default cwd** — `config.rs:534-538` warns and returns `None`; runtime falls back to `None` cwd (`runtime.rs:1822-1830`). No toast, no user-visible signal.
- **AC-NEW-S09: TOML reload requires restart** — `config::load()` runs once at `main.rs:116`. Worth a one-liner if any user expects otherwise.
- **AC-NEW-S10: Bootstrap stage progression is `VersionProbe → [TarballStage → Scp → RemoteBuild →] DaemonSpawn → SocketTunnel → SocketConnect`** — replace AC-003's wrong stage list. Bracketed three skipped on cached daemons.

### Cross-cutting notes (Spawn)

- **AC-003 is the most drifted AC in the document.** Stage labels, retry-on-version-mismatch, and "scp daemon" framing all describe a different bootstrap design than `crates/codemuxd-bootstrap/src/lib.rs`. Real flow ships a *source tarball* and runs `cargo build` on the remote. Worth a deliberate decision: rewrite AC-003 to source-build reality, or open an issue to migrate to per-arch binaries (which would simplify the stage list and re-enable the "no daemon binary bundled" failure mode).
- **AC-004's "daemon's index of the remote filesystem" mis-attributes ownership.** Remote autocomplete is the TUI's `index_manager.rs` walking via `RemoteFs` (an SSH ControlMaster). Daemon has no path-index endpoint.
- **All numeric caps in ACs are off or imprecise.** `MAX_COMPLETIONS = 8`, `MAX_FUZZY_RESULTS = 50`, `MAX_SCAN_ENTRIES = 1024`. AC's "more results truncated hint" UI doesn't exist.
- **`Failed` state is SSH-only.** `AgentState::Failed { error: codemuxd_bootstrap::Error }` (`runtime.rs:865-866`) only carries bootstrap errors. Any AC that says "local spawn lands in Failed" is wrong by construction.
- No `--ssh` CLI flag exists.

---

## Navigation (AC-009 — AC-013)

### Per-AC findings

- **AC-009: Cycle focus between agents** — **Drift**.
  - Cycling chords `n|l|j|→|↓` and `p|h|k|←|↑`, plus direct `Cmd+'`/`Cmd+;`, all wired. Aliases resolve to `PrefixAction::FocusNext` (`apps/tui/src/keymap.rs:425-444`, test `prefix_focus_next_aliases_all_resolve_to_focus_next` at `keymap.rs:1201-1219`); prev mirror symmetric (`keymap.rs:438-444`); direct chords `keymap.rs:758-774`, dispatched in `Idle` at `apps/tui/src/runtime.rs:3818-3825`. Cycling is modulo on `agents.len()` at `runtime.rs:3569-3580`.
  - **The "PTY is `SIGWINCH`'d to `B`'s pane geometry" claim is wrong**. `change_focus` (`runtime.rs:392-401`) only mutates `focused`/`previous_focused`/`needs_attention`; `transport.resize` is never called on focus change. Resize call sites in `runtime.rs` are only at startup, attach, terminal resize, and `ToggleNav`. Geometry is shared across all panes (`pty_size_for` at `runtime.rs:1289-1301` is per-style, not per-agent), so a per-focus SIGWINCH would also be a no-op. **Drop the sentence from AC-009.**

- **AC-010: Focus an agent by ordinal digit** — **Drift on the failure-mode sub-claim**.
  - Digit-1..9 hardcoded at `runtime.rs:3862-3870`; sticky-on-digit pinned by `prefix_then_digit_stays_sticky` at `runtime.rs:6044-6055`; dispatch clamps via `if idx < nav.agents.len()` at `runtime.rs:3592-3596`.
  - **Drift**: out-of-range path: `compute_awaiting_dispatch` returns `KeyDispatch::FocusAt(d-1)` regardless of agent count; that variant is in `is_nav_dispatch` (`runtime.rs:3889-3897`), so **state stays `AwaitingCommand`**. AC's "the prefix state still drops to `Idle`" contradicts the code — chord is silently no-op'd but user remains in sticky mode.
  - Esc-exits-sticky correct via the unbound-key path (`runtime.rs:6097-6108`).

- **AC-011: Bounce to the previously-focused agent** — **Implemented**.
  - `previous_focused: Option<usize>` set inside `change_focus` (`runtime.rs:392-401`); `FocusLast` dispatched by `Ctrl-B Tab` (`keymap.rs:445`, dispatched at `runtime.rs:3581-3591`).
  - "Two-slot symmetric bounce" correct because each `change_focus` overwrites `previous_focused` with the prior `focused` slot. Single-agent / no-prior failure mode covered by `prev < nav.agents.len() && prev != nav.focused` guard at `runtime.rs:3585-3590` and `previous_focused` cleared on reap (`runtime.rs:474-483`).
  - Direct `Cmd`-chord for FocusLast unbound by default (`keymap.rs:771`, test `keymap.rs:1261-1268`).

- **AC-012: Switcher popup picks an agent by name** — **Partial**.
  - Open via `Ctrl-B w` → `KeyDispatch::OpenPopup` at `runtime.rs:3605-3609`; popup dispatch (Up/Down/Enter/Esc) at `runtime.rs:3452-3477`; centered render at `runtime.rs:5281-5306` (`centered_rect(50, 60, area)`).
  - **Sub-claim drift**: AC says rows show "label, host, and status" — `agent_label_spans` (`runtime.rs:5103-5122`) emits spinner (working = status proxy), host prefix (SSH only), attention dot, label body. There is no explicit `Failed`/`Crashed` text and no separate status column.
  - **Aspirational sub-claim**: "No agents exist: popup opens with an empty list and a 'no agents' hint" — **unreachable**. Event loop returns the moment `nav.agents.is_empty()` (`runtime.rs:2940-2948`), so the popup can never be opened with zero agents.

- **AC-013: Toggle the navigator chrome** — **Implemented**.
  - `Ctrl-B v` → `KeyDispatch::ToggleNav` (`keymap.rs:446`, dispatch at `runtime.rs:3597-3604`); `NavStyle::toggle` flips Popup ↔ LeftPane (`runtime.rs:185-192`); `resize_agents` IS called after the flip (`runtime.rs:3601-3603`) — SIGWINCH claim is correct here. `NAV_PANE_WIDTH = 25` (`runtime.rs:79`) matches.
  - CLI `--nav` and env `CODEMUX_NAV` wired via clap at `main.rs:47-49` with `value_enum`; `kebab-case` rendering of `NavStyle` gives `left-pane`/`popup`. No `nav_style` config-file key — AC doesn't claim one.

### Suggested new ACs (Navigation)

- **AC-NEW-N01: Spawning a new agent focuses it and records the prior focus as the bounce slot** — `runtime.rs:2903-2908` calls `change_focus` to the new tail; `change_focus` sets `previous_focused = old focused` (`runtime.rs:396`). Immediately after spawn, `Ctrl-B Tab` bounces back. Worth pinning because it interacts with AC-011.
- **AC-NEW-N02: Failed and Crashed agents participate in the focus cycle** — `FocusNext`/`FocusPrev`/`FocusAt` are pure index arithmetic; don't filter by `AgentState`. Intentional (you need to focus a Failed tab to dismiss it).
- **AC-NEW-N03: Reaping the focused agent moves focus to the new tail (NOT to `previous_focused`)** — `remove_at` clamps `focused = focused.min(len-1)` (`runtime.rs:466-473`) and clears `previous_focused == idx` (`474-483`). **Surfaces drift in AC-014**: AC-014 claims the opposite.
- **AC-NEW-N04: Out-of-range digit chord stays sticky, does not drop to `Idle`** — `runtime.rs:3862-3870` + `is_nav_dispatch` (`3889-3897`). Replaces or accompanies AC-010's failure-mode sub-claim.
- **AC-NEW-N05: Switcher popup confirm clamps a stale selection if the agent vector shrank while open** — `remove_at` clamps `selection.min(agents.len()-1)` (`runtime.rs:484-497`).
- **AC-NEW-N06: Direct-chord `Cmd`-prefixed nav does NOT enter sticky-prefix state** — Test `direct_cmd_backslash_spawns_agent_without_arming_prefix` (`runtime.rs:5964-5977`). Direct chords are always one-shot, even though same action over prefix is sticky.

### Cross-cutting notes (Navigation)

- Recurring drift: **AC text written against an idealized geometry/status model**. AC-009 and AC-013 both invoke SIGWINCH but only AC-013's geometry actually changes; AC-012 imagines status columns the renderer doesn't draw. Runtime is internally consistent (one global `pty_size_for(style, term_rows, term_cols)`); pull docs toward code.
- **Failure-mode sub-claims that the runtime structurally can't reach**: AC-012's "no agents" popup, AC-010's "drops to Idle" on out-of-range. Both reachable on paper but contradicted by explicit early-return / sticky-classification code.
- Sticky-prefix policy well-tested (`runtime.rs:6044-6133`) and consistent with AC-010's "any non-nav keystroke or Esc drops out". The one inconsistency is the in-range-vs-out-of-range digit asymmetry.

---

## Agent lifecycle (AC-014 — AC-016)

### Per-AC findings

- **AC-014: Force-close `prefix x`** — **Partial / Drift**.
  - Dispatch wired: `prefix x` → `PrefixAction::KillAgent` → `KeyDispatch::KillAgent` → `nav.kill_focused()` (`apps/tui/src/runtime.rs:6147` test, `:3616-3617`, `:433-439`).
  - Transport-Drop reap is real: `LocalPty::drop` calls `child.kill()` + `child.wait()` (`crates/session/src/transport.rs:369-384`); `SshDaemonPty::drop` kills the `ssh -N -L` tunnel and lets the socket EOF the daemon's framed reader (`transport.rs:574-594`).
  - **Drift**: AC says "focus moves to the previously-focused agent (or to the next slot if there was none)." Code does NOT bounce to `previous_focused`. `kill_focused → remove_at` (`runtime.rs:462-498`) clamps `focused` to `min(focused, len-1)` — i.e. the next slot at the same index, or the new last index. The `previous_focused` slot is *cleared* if it equals `focused` (`:481-483`), never used as a fall-through target. Pinned by `kill_focused_clamps_focus_when_killing_last_tab` (`runtime.rs:7662-7679`) which expects `focused = 1` after killing index 2 of `[a, b, c]`, not bounce-to-previous. **Pick one: drop the AC's bounce claim, or change runtime to match.**

- **AC-015: Dismiss `prefix d`, no-op on Ready** — **Implemented**.
  - Dispatch: `prefix d` → `PrefixAction::DismissAgent` → `KeyDispatch::DismissAgent` → `nav.dismiss_focused()` (`runtime.rs:6158-6165`, `:3613-3615`, `:411-423`).
  - Ready-guard real: `dismiss_focused` matches `AgentState::Failed | AgentState::Crashed` only; Ready returns `false` and slot stays (`:412-419`). Pinned by `dismiss_no_op_on_focused_ready_agent` (`:7538-7550`), `dismiss_removes_focused_failed_agent` (`:7528-7535`), `dismiss_removes_focused_crashed_agent_and_clamps_focus` (`:7511-7525`).
  - Failed has no transport so nothing to reap; Crashed's transport already dropped in `mark_crashed` (`runtime.rs:1083-1099`). All three `Then` branches verified.

- **AC-016: Quit cleanly `prefix q`** — **Partial — most of the checklist is real, two items are missing**.
  - Dispatch: `prefix q` → `PrefixAction::Quit` → `KeyDispatch::Exit` → `return Ok(())` from `event_loop` (`runtime.rs:3872`, `:3547`, test `prefix_q_exits` `:5913-5921`). Returning unwinds `_guard: TerminalGuard` (`runtime.rs:1255-1261`), whose `Drop` runs the teardown (`:117-170`).
  - Checklist:
    - **All transports dropped** — Implemented. `NavState` owned by `event_loop`; on return, every `RuntimeAgent` and its `AgentTransport` drops.
    - **Alt-screen exit** — Implemented (`runtime.rs:168` `LeaveAlternateScreen`).
    - **Raw mode disabled** — Implemented (`:167` `disable_raw_mode`).
    - **Mouse capture released** — Implemented, gated on the acquire flag (`:136-138` `DisableMouseCapture`).
    - **KKP flags popped** — Implemented, gated on `enhanced_keyboard` (`:145-147` `PopKeyboardEnhancementFlags`).
    - **Host title restored** — Implemented (`:153-155` `host_title::pop_title`, sequence `\x1b[23;0t` at `host_title.rs:84-87`).
    - **Stdin drained** — Implemented (`:162-166`, `event::poll(Duration::ZERO)` loop). Comment cites the KKP-key-release-leak motivation.
    - **Process exits 0** — Implemented implicitly: `event_loop` returns `Ok(())`, `run` returns `Ok(())`, `main` returns `Ok(())`; no `std::process::exit`.
    - **Failure mode "child fails to reap within cleanup timeout → orphan logged to `~/.cache/codemux/logs/codemux.log`"** — **Missing**. There is no cleanup timeout. `LocalPty::drop` calls `child.kill()` then `child.wait()` (which blocks indefinitely); on failure it `tracing::debug!`s with no special "orphan" framing (`transport.rs:377-382`). `SshDaemonPty::drop` does the same on the SSH tunnel (`:579-591`). Log file path correct for *general* tracing (`main.rs:148-156`), but no code path special-cases reap-timeout. **AC's failure-mode text describes behavior that does not exist.**
    - **Bonus implemented but not in the AC**: bracketed paste disable (`:133-135`), focus-change disable (`:130-132`), OSC 22 mouse-pointer reset (`:144`).

### Suggested new ACs (Lifecycle)

- **AC-NEW-L01: Last-tab reap auto-exits codemux** — `runtime.rs:2940-2948` returns `Ok(())` (so guard drops, terminal restored, exit 0) the moment `nav.agents.is_empty()` after the per-tick reap. Covers "last live agent typed `/quit`" and "user dismissed the last terminal-state corpse." Today this is silent; current AC-016 only covers explicit `prefix q`.
- **AC-NEW-L02: Ready→Crashed transition on non-zero PTY exit** — `runtime.rs:299-315` (`reap_dead_transports`) + `:1083-1099` (`mark_crashed`). Exit 0 silently removes; non-zero (and SSH `-1` sentinel) preserves the parser, replaces transport with synthetic, renders red banner with the configured dismiss chord. Pinned by `reap_transitions_ready_with_dead_transport_to_crashed` (`runtime.rs:7391-7401`).
- **AC-NEW-L03: `prefix x` works on Failed/Crashed too** — `kill_focused` has no state guard (`:433-439`), tested by `kill_focused_removes_failed_agent` (`:7643-7652`). User has two redundant chords on dead tabs (`d` and `x`); pin so a future "x is for live only" refactor doesn't regress it.
- **AC-NEW-L04: Panic restores the terminal** — `TerminalGuard::drop` runs on panic unwind (comment at `runtime.rs:117-122` anticipates this). `color_eyre::install()` called (`main.rs:105`). No explicit test; worth an AC since the failure mode (corrupted terminal) is severe.
- **AC-NEW-L05: Crashed slot's last screen content stays visible** — `mark_crashed` preserves the boxed `Parser` so the user can scroll back through what claude was doing pre-crash (`runtime.rs:883-898`, `:1083-1099`). Core UX argument for keeping Crashed slots vs auto-reaping.

### Cross-cutting notes (Lifecycle)

- **No signal handlers anywhere.** No `signal_hook`, no `ctrlc`, no SIGTERM/SIGHUP/SIGINT trap. SIGTERM/SIGHUP from outside kills the process before `TerminalGuard::drop` runs, leaving the host terminal in alt-screen + raw mode + mouse-capture. SIGINT (Ctrl+C) at the outer shell only fires before codemux enters raw mode; once raw, the bare `\x03` byte is forwarded as a key event. Worth either an AC pinning the current behavior or a follow-up.
- **No daemon-death detection on the TUI side.** If `codemuxd` dies while a remote agent is live, `SshDaemonPty` surfaces it via the `-1` exit-code sentinel and `mark_crashed` runs (`runtime.rs:283-310`). The user sees a Crashed banner with `-1`.
- **AC-016 failure-mode text is aspirational.** "Cleanup timeout" matches the comment ethic at `transport.rs:373-376` (best-effort, never panic, log debug) but no actual timeout, and "orphan" appears only in unrelated comments. Either rewrite the failure mode to "best-effort: a stuck `child.wait` blocks the exit; the user can `kill -9` the codemux process" or implement the timeout.
- **AC-014 focus-after-reap drift** is the most important finding — AC promises bounce-to-previous; code does next-slot-clamp; test pins the latter. **Pick one.**

---

## Scrollback (AC-017 — AC-018)

### Per-AC findings

- **AC-017: Enter scroll mode and navigate history** — **Implemented with one quantitative drift**.
  - Wheel-up → enter scroll mode: `apps/tui/src/runtime.rs:3653-3657` calls `nudge_scrollback(WHEEL_STEP)` on `MouseEventKind::ScrollUp` for the focused agent. Once `scrollback() > 0`, scope-gated dispatch at `runtime.rs:3495-3511` activates `g`/`G`/`PageUp`/`PageDown`/`Up`/`Down`/`Esc`.
  - **Drift**: AC says "one tick = one line". `WHEEL_STEP = 3` (`runtime.rs:90`). **Either AC or constant should change.**
  - PageUp page size: `runtime.rs:3499` uses `focused_agent.rows.saturating_sub(1).max(1)` — pane rows minus one, not "the agent's row count" exactly.
  - `g` jump-to-top: `jump_to_top` at `runtime.rs:1037` (`set_scrollback(usize::MAX)`).
  - `G` snap-to-live: `ScrollAction::Bottom | ExitScroll => snap_to_live()` at `runtime.rs:3506-3508`. Badge disappears because `render_agent_pane` only paints when `offset > 0` (`runtime.rs:4059-4061`).
  - **No-SIGWINCH invariant**: `resize_agents` is the only function that touches PTY size (`runtime.rs:1898-1923`), called from exactly two sites — `Event::Resize` (`runtime.rs:3621-3623`) and `KeyDispatch::ToggleNav` (`runtime.rs:3597-3604`). Scroll-mode arm calls only `nudge_scrollback`/`jump_to_top`/`snap_to_live`, none resize. AD-25 rationale at `runtime.rs:4037-4043`.
  - Floating badge bottom-right: `render_scroll_indicator` (`runtime.rs:4442-4464`) renders at `area.x + width - 24, area.y + height - 1`. Width 24. Painted in both Ready and Crashed states.
  - Named test exists: `runtime::tests::scrollback_zero_len_means_no_history` at `runtime.rs:8147-8161`.
  - Default `scrollback_len = 5000`: `config.rs:37` (`default_scrollback_len`), pinned by `config.rs:912 scrollback_len_defaults_to_five_thousand`.
  - Minor wording bug: AC-017's failure mode about "Claude switched to the alt screen" cites the regression test, but the test pins the **zero-len clamp**, not alt-screen behavior.

- **AC-018: Typing snaps to live; navigation preserves scroll** — **Implemented**.
  - **Snap-before-forward ordering**: `runtime.rs:3514-3528`. The `KeyDispatch::Forward(bytes)` arm calls `a.snap_to_live()` at line 3524 *before* `transport.write(&bytes)` at line 3527. Pinned ordering — correct.
  - **Navigation chords are non-snapping**: snap is only inside the `Forward` arm; `FocusNext`/`FocusPrev`/`FocusAt`/`FocusLast`/`OpenPopup`/`OpenHelp`/`SpawnAgent`/`Consume`/`ToggleNav`/`DismissAgent`/`KillAgent` all skip the snap (`runtime.rs:3569-3618`). Documented in comment at `runtime.rs:3515-3522`.
  - **Per-agent offset survives focus change**: scroll state lives in each agent's own `vt100::Parser` (`scrollback_offset` reads `self.state.screen()`, `runtime.rs:1000-1002`). `change_focus` (`runtime.rs:392-401`) only mutates indices and `needs_attention`. Pinned by `nudge_scrollback_only_touches_focused_agent` (`runtime.rs:8348-8362`) and contract test `scrollback_state_is_per_parser` (`runtime.rs:8217-8250`).
  - All claimed tests exist: `nudge_scrollback_moves_offset_into_history_then_back` (8324), `snap_to_live_resets_offset_to_zero` (8372), `nudge_scrollback_only_touches_focused_agent` (8348), four contract guards at 8147-8250.

### Suggested new ACs (Scrollback)

- **AC-NEW-SB01: Wheel = 3 lines per tick** — `WHEEL_STEP = 3` (`runtime.rs:90`). Either fix AC-017 or pin the choice.
- **AC-NEW-SB02: Pasting while scrolled-back snaps to live before the bracketed-paste write** — `runtime.rs:3774-3796`. Same ordering rule as typing but for `Event::Paste`; not covered by AC-018.
- **AC-NEW-SB03: Wheel and scroll-mode keys are inert while spawn modal / popup / help is open** — gated by `no_overlay_active(...)` on `Event::Mouse` arm (`runtime.rs:3642`); spawn/popup/help branches `continue` before reaching the scroll arm (`runtime.rs:3449, 3476`).
- **AC-NEW-SB04: Crashed agents remain scrollable; offset survives until dismissed** — `nudge_scrollback`/`snap_to_live`/`jump_to_top` all match `Crashed { parser, .. }` (`runtime.rs:1011, 1026, 1038`); badge renders on Crashed panes (`runtime.rs:4094-4097`); pinned by `nudge_scrollback_moves_offset_on_crashed_agent` (8460) and `jump_to_top_works_on_crashed_agent` (8484).
- **AC-NEW-SB05: Failed agents have no scroll** — `Failed` has no `Parser`, so `scrollback_offset` returns 0 and methods are no-ops (`runtime.rs:1000-1043`). Pinned by `nudge_scrollback_no_op_on_failed_agent` (8365), `snap_to_live_no_op_on_failed_agent` (8382), `scrollback_offset_returns_zero_for_failed_agent` (8402).
- **AC-NEW-SB06: Scrollback eviction holds the user's view in place** — when new rows arrive while scrolled-back, vt100 bumps the offset so same content stays under gaze. Pinned by `scrollback_offset_auto_bumps_when_new_rows_evict` (8177).
- **AC-NEW-SB07: `jump_to_top` clamps to `scrollback_len`, never exceeds it** — `runtime.rs:1037-1043` + test 8389. Default 5000-row cap is operational ceiling.
- **AC-NEW-SB08: Wheel-anywhere scrolls the focused agent (not the agent under the cursor)** — `runtime.rs:3643-3661` ignores `column`/`row` for `ScrollUp`/`ScrollDown` and always nudges `nav.agents.get_mut(nav.focused)`. Documented at `runtime.rs:3643-3651`. Worth pinning because LeftPane mode reads as if wheel-over-nav should scroll the nav.

### Cross-cutting notes (Scrollback)

- Runtime's scroll-mode dispatch is **focused-agent-only by design** (`runtime.rs:3495-3496`). Non-focused agents cannot be scrolled — wheel handler also only nudges the focused agent.
- **Search-in-scrollback is not implemented.** No `search`/`find_in_scrollback`/regex over rows.
- **Selection over scrollback** is covered by AC-021 (`contents_between` walks `visible_rows()`).

---

## Mouse (AC-019 — AC-021)

### Per-AC findings

- **AC-019: Click a tab to focus it** — **Implemented**.
  - `TabHitboxes` exists at `apps/tui/src/runtime.rs:701-726`; `Hitbox { rect, agent_id }` at `runtime.rs:514-517` records the agent **id** (not slot index).
  - Both nav styles populate hitboxes: `render_status_bar` (`runtime.rs:4839`) and `render_left_pane` (`runtime.rs:4691`).
  - Hitboxes cleared at top of every `render_frame` (`runtime.rs:2661`, `TabHitboxes::clear` at `runtime.rs:706-708`) — no stale-rect bleed.
  - Click resolution: `tab_mouse_dispatch` at `runtime.rs:2023-2049` returns `Click(AgentId)`; loop resolves `id → idx` via `nav.agents.iter().position(|a| a.id == id)` at the moment of focus mutation (`runtime.rs:3753`). Background reorder between press and release cannot misroute.
  - Failure mode "click misses every tab" → `tab_mouse_dispatch` returns `None`, no-op, covered by `tab_mouse_dispatch_down_outside_tabs_returns_none` (`runtime.rs:6576`).
  - SIGWINCH on focus change happens via `nav.change_focus(idx)` (`runtime.rs:3754`). (Note: AC-009 audit found `change_focus` does NOT resize — but the click path doesn't promise it either.)
  - **Not in the AC but worth flagging**: mouse events are *entirely* dropped when an overlay is open (`no_overlay_active` guard at `runtime.rs:3642`).

- **AC-020: Drag a tab to reorder** — **Implemented**.
  - `reorder_agents` at `runtime.rs:1950-1956` does exactly `agents.remove(from); agents.insert(to, agent);` — browser-tab semantics, not swap. Pinned by `reorder_agents_drag_right_inserts_at_destination` (`runtime.rs:6299`) and `_drag_left_inserts_at_destination` (`runtime.rs:6312`).
  - Identity-preserving focus via `shift_index` (`runtime.rs:1971-1981`), applied to both `nav.focused` and `nav.previous_focused` at `runtime.rs:3763-3765`. End-to-end pin: `reorder_followed_by_shift_index_keeps_focus_on_the_moved_agent` (`runtime.rs:6336`).
  - Both nav styles supported.
  - Release-outside cancels: `TabMouseDispatch::Cancel` at `runtime.rs:2040`, dispatched at `runtime.rs:3768`. Test `tab_mouse_dispatch_up_outside_tabs_cancels` at `runtime.rs:6627`.
  - Mid-drag-reap silent cancel: `Reorder` arm at `runtime.rs:3759-3766` does `let from_idx = nav.agents.iter().position(|a| a.id == from); let to_idx = ...; if let (Some(f), Some(t)) = ... { reorder_agents(...) }` — missing id resolves to `None`, gesture silently no-ops.

- **AC-021: Drag-to-select and copy via OSC 52** — **Drift**.
  - Selection state machine: `Selection { agent, anchor, head }` at `runtime.rs:622-627`; `pane_mouse_dispatch` at `runtime.rs:2085-2111` produces `Arm`/`Extend`/`Commit`; loop wires them at `runtime.rs:3717-3737`.
  - Reverse-video render via `paint_selection_if_active` (test `runtime.rs:9333`).
  - `commit_selection` (`runtime.rs:2133-2160`) extracts via `parser.screen().contents_between(start.row, start.col, end.row, end.col + 1)` for `Ready`/`Crashed` — exact AC wording. `vt100::Screen::contents_between` walks `visible_rows()`, so scrollback included; pinned by `vt100_contents_between_extracts_selection_substring` (`runtime.rs:9287`) and `_handles_multirow_selection` (`runtime.rs:9311`).
  - `Failed` agents have a parallel extraction path via `failure_text_in_range` (`runtime.rs:2151`) — **AC does not mention this**.
  - OSC 52 emission: `write_clipboard_to` at `runtime.rs:2495-2501` writes exactly `\x1b]52;c;{base64}\x07`. Pinned byte-for-byte by `write_clipboard_to_emits_osc_52_with_base64_payload` (`runtime.rs:9256`, `b"\x1b]52;c;aGk=\x07"`).
  - Selection cleared on focus change / reap (`runtime.rs:2959-2966`) and on resize (`runtime.rs:3629`).
  - Alt-drag fallback documented in help screen (`runtime.rs:5398-5401`).
  - **Drift / not implemented**: AC says "A toast confirms the copy (or, on failure, an error toast)." Reality: `commit_selection` writes silently; only `tracing::debug` on failure (`runtime.rs:2157-2159`). Only toast emission in runtime is for URL-open outcomes (`runtime.rs:2752`). **No success toast. No "Selection failed: clipboard unavailable" toast.**
  - **Drift**: AC says "Terminal does not support OSC 52 → an error toast appears." Crossterm/codemux **cannot detect OSC 52 capability** — terminal silently swallows the escape. The "error toast on unsupported" claim is unimplementable without a terminfo probe or DA1 query.

### Suggested new ACs (Mouse)

- **AC-NEW-M01: Wheel scrolls the focused agent regardless of cursor position** — `MouseEventKind::ScrollUp/Down` arms at `runtime.rs:3653-3662` ignore cursor's column/row and always nudge `nav.focused`'s scrollback by `WHEEL_STEP=3` rows (`runtime.rs:90`). LeftPane: wheel-over-nav-strip still scrolls the agent. Documented in code comments at `runtime.rs:3643-3651` but not in any AC.
- **AC-NEW-M02: Mouse events are suppressed while an overlay is open** — entire `Event::Mouse` arm gated on `no_overlay_active` (`runtime.rs:3642`, helper at `runtime.rs:2508-2516`). Click on a tab while spawn/popup/help is up = no-op.
- **AC-NEW-M03: Right-click and middle-click on tabs are explicit no-ops** — pinned by `tab_mouse_dispatch_non_left_buttons_are_ignored` (`runtime.rs:6675-6692`). The AD-25 minimal-mouse posture is referenced in code (`runtime.rs:2107-2108`).
- **AC-NEW-M04: Ctrl+click on a URL hands it to the OS opener; Ctrl+hover shows underline + hand cursor** — `compute_hover` at `runtime.rs:2175-2190`, Ctrl-click branch at `runtime.rs:3691-3702`, hover state cleanup on focus-change at `runtime.rs:2991-2998`, fallback-to-clipboard toast at `runtime.rs:2307-2326`. Real, tested, user-visible mouse behavior with **zero AC coverage**.
- **AC-NEW-M05: Drag clamps to pane edges so overshoot still selects to the boundary** — `PaneHitbox::clamped_cell_at` at `runtime.rs:593-603`, called by `pane_mouse_dispatch` on `Drag(Left)` (`runtime.rs:2102`).
- **AC-NEW-M06: Selection on a `Failed` agent extracts text from the centered failure layout** — `Failed` arm of `commit_selection` (`runtime.rs:2142-2152`) calls `failure_text_in_range`. Pinned by tests at `runtime.rs:9489-9560`.
- **AC-NEW-M07: `mouse_yield_on_failed = true` releases mouse capture while a Failed agent is focused** — `MouseCaptureState` at `runtime.rs:2406-2480`, config knob at `apps/tui/src/config.rs:71` and tests `:1609`/`:1622`. State machine toggles capture via real `EnableMouseCapture`/`DisableMouseCapture` calls (`runtime.rs:2467-2472`).

### Cross-cutting notes (Mouse)

- **Identity-not-index discipline is consistent throughout.** Every mouse code path stores `AgentId`, resolves to `idx` only at the mutation site, `if let Some(...)` guards the lookup. Pattern (drag, click, selection, hover) is uniform; ACs implicitly assume it but discipline is wider than AC-019/020 acknowledge.
- **Toast surface is underused for mouse.** Only URL-open uses it. AC-021's "toast confirms the copy" assumes a surface the runtime never wires up. Either drop the toast claim or grow `commit_selection` a `&mut ToastDeck` parameter.
- **Keymap has zero mouse entries.** AC-025 says "the mouse-gesture lines (`click`, `drag`) are listed alongside the keystroke bindings" — those lines are hardcoded strings in help renderer (`runtime.rs:5385-5401`), not generated from `Bindings`. AC-025's "cannot drift" claim is technically false for mouse rows.
- **No mouse interaction with overlays.** Spawn modal, switcher popup, help screen all swallow mouse events because of `no_overlay_active` gate. No "click outside to dismiss", no "click wildmenu candidate to select", no "click switcher row to focus".
- **Wheel step (`WHEEL_STEP=3`) is hardcoded at `runtime.rs:90`.** AC-017 says "one tick = one line" — contradicts implementation.

---

## Status bar (AC-022 — AC-024)

### Per-AC findings

- **AC-022: Configured segments render in order** — **Implemented (with one drift)**.
  - Default segment list `["model", "tokens", "worktree", "branch", "prefix_hint"]` at `apps/tui/src/status_bar/mod.rs:78-86`. Pinned at `mod.rs:528-545`.
  - **Drift from AC-022 example**: AC-022's example config order is `["model", "worktree", "branch", "tokens", "prefix_hint"]`, but actual default puts `tokens` next to `model`. AC-023's stated default *does* match shipped default — **the two ACs disagree.**
  - Right-side stack rendered with `" │ "` (3-cell) separator at `status_bar/mod.rs:232-233`, joined at `mod.rs:300-307`. `render_status_bar` consumes at `apps/tui/src/runtime.rs:4799`.
  - `model` reads `~/.claude/settings.json` via `current_model_and_effort()` at `apps/tui/src/agent_meta_worker.rs:486-491`, parsed at `agent_meta_worker.rs:498-512`.
  - **"One poll per cycle" claim**: polling cycle is **2000 ms**, not "per render frame" — `POLL_INTERVAL = Duration::from_millis(2_000)` at `agent_meta_worker.rs:59`. Worker polls all three (branch + model + tokens) on same 2s tick at `agent_meta_worker.rs:336-389`. AC reads as if it means "per render cycle".
  - **"Focused agent only"**: enforced by `sync_meta_worker_target` at `runtime.rs:1844-1874`.
  - `worktree`, `branch`, `tokens`, `prefix_hint` content: `WorktreeSegment` at `segments.rs:186-202`, `BranchSegment` at `segments.rs:219-247`, `TokenSegment` at `segments.rs:272-403`, `PrefixHintSegment` at `segments.rs:461-494`.
  - **Failure modes**: all four implemented.
    - Unknown ID skipped + logged once at startup: `tracing::warn!` at `mod.rs:217-222`, called only at startup at `runtime.rs:1267`, pinned by test at `mod.rs:511-520`.
    - Missing/unparseable `~/.claude/settings.json`: returns `None`, `ModelSegment.render` returns `None` at `segments.rs:65`.
    - SSH-focused agent skips model/branch/tokens: `sync_meta_worker_target` only sends `set_target` when `cwd.is_some()` (`runtime.rs:1854-1858`).
    - Missing statusline JSON: `read_token_usage(path)` returns `None`; `TokenSegment.render` returns `None` at `segments.rs:354` and `:360-365`.

- **AC-023: Customize the status bar via config** — **Implemented**.
  - Closed set `model | tokens | repo | worktree | branch | prefix_hint` matches AD-29: built-in arms in `status_bar/mod.rs:202-223`, ID constants at `mod.rs:54-59`. Mirrors AD-29 at `docs/architecture.md:651-706`.
  - `[ui] status_bar_segments` is `Vec<String>` parsed at `apps/tui/src/config.rs:168`; default fills via `Ui::default()` at `config.rs:185-195`.
  - Empty list disables right-side block: `render_segments` early-returns `(Line::default(), 0)` at `status_bar/mod.rs:247-249`; `render_status_bar` keeps `right_area = None` at `runtime.rs:4803-4811`. Pinned by config test at `config.rs:997-1003` and segment test at `mod.rs:462-469`.
  - Default fallback when omitted: `default_segment_ids()` at `mod.rs:78-86` returns `model, tokens, worktree, branch, prefix_hint` — matches AC-023's stated default exactly. Pinned at `config.rs:965-981`.
  - Per-segment sub-config: `SegmentConfig` at `config.rs:201-210` holds `branch: BranchSegmentConfig` and `tokens: TokensSegmentConfig`. `BranchSegmentConfig.default_branches` at `config.rs:215-240` plumbed into `BranchSegment::new` at `mod.rs:210-212`. Tests at `config.rs:1015-1043`.

- **AC-024: Segments drop from the left under width pressure** — **Implemented**.
  - Drop algorithm: `render_segments` walks rendered slots **right-to-left**, accumulating width + separator, breaks on overflow, drops everything to the left of the break (`status_bar/mod.rs:266-298`). `prefix_hint` (rightmost in defaults) is preserved last by construction.
  - Pinned by tests at `mod.rs:393-420` (drops leftmost first), `mod.rs:422-445` (keeps only rightmost when very tight), `mod.rs:447-459` (returns empty when even rightmost doesn't fit — AC says "always keeps the help anchor visible until there's literally no width," matches this last case).
  - Resizing back wide re-adds segments: implicit because `render_segments` is called every frame from a fresh layout — no persistent "dropped" state.
  - `prefix_hint` always returns `Some` (`segments.rs:1222-1230` test), so it's the only segment never `None`.

### Suggested new ACs (Status bar)

- **AC-NEW-B01: Status bar is hidden in `LeftPane` chrome** — `runtime.rs:3964-3997` only calls `render_popup_style` (which calls `render_status_bar`) for `NavStyle::Popup`; `render_left_pane` at `runtime.rs:4644-4705` has no status bar at all. Currently undocumented behavior — users who toggle nav chrome via `prefix v` (AC-013) lose all segment data. **Either document this or fix it.**
- **AC-NEW-B02: Right-side stack capped at 3/5 of status-bar width** — `runtime.rs:4780` `let max_right = area.width.saturating_mul(3) / 5`. Drop algorithm may fire even on a wide terminal if tab strip needs the room.
- **AC-NEW-B03: Worker poll cadence is 2s; segments lag a `/model` change by up to 2s** — `agent_meta_worker.rs:59`. AC-022's "one poll per cycle" is ambiguous.
- **AC-NEW-B04: Worker emits one event only when a value changes** — `agent_meta_worker.rs:342-388` — every poll skips emission if value equals `last_*`.
- **AC-NEW-B05: `tokens` `refresh_interval_secs` knob is forwarded to Claude's `statusLine.refreshInterval`** — `config.rs:286-301`; `runtime.rs:1404` (`build_claude_args(&statusline_path, tokens_cfg)`) consumes it.
- **AC-NEW-B06: `tokens` color thresholds (yellow/orange/red) and `auto_compact_window` override** — `config.rs:269-286`, `segments.rs:335-345`. Pinned by inline tests at `segments.rs:975-1078` but no AC documents user-visible thresholds or `$CLAUDE_CODE_AUTO_COMPACT_WINDOW` env-var fallback (`segments.rs:290-300`).
- **AC-NEW-B07: `branch` segment hides when on a default branch; `worktree` hides when cwd basename matches repo basename** — both intentional "ambient noise reduction" behaviors (`segments.rs:237-241`, `:197-200`) that users hit immediately.
- **AC-NEW-B08: `tokens.format = "with_bar"` renders a fixed-width 9-cell bar** — `segments.rs:408-428`, pinned at `segments.rs:951-972`. Width-stability is what makes AC-024 deterministic.

### Cross-cutting notes (Status bar)

- AD-29 is fully aligned with code: closed set, drop-from-left, right-edge anchor preserved, no shell-out. Model carve-out bounded by `agent_meta_worker.rs` and never touches `apps/tui` rendering.
- `host_colors` (`config.rs:125`) does NOT apply to status-bar segments — only to tab labels.
- No status-bar visibility toggle, no separator customization knob.
- AC-022 example string should be updated for consistency with AC-023.
- `apps/tui/tests/` does not exist; all status-bar coverage is inline.

---

## Help (AC-025)

### AC-025 findings — **Partial**

- **Modal exists, wired to `prefix ?`** — Implemented. `apps/tui/src/runtime.rs:744` defines `OpenHelp`; `:3881` maps `PrefixAction::Help`; `:3610-3612` flips `help_state` to `Open`; `:4004-4006` calls `render_help`. Default chord `?` (`apps/tui/src/keymap.rs:450`). Test at `runtime.rs:8003-8012` (`prefix_question_mark_opens_help`).
- **"Full-screen" modal** — **Drift**. `render_help` uses `centered_rect_with_size(64, 50, area)` (`runtime.rs:5309`, helper at `:5436-5445`) — fixed 64-col x 50-row centered popup with a `Clear`-ed background, **not full-screen**.
- **Iterates the `Bindings` POD as single source of truth** — Implemented for keymap-derived rows. `render_help` (`runtime.rs:5308-5406`) iterates `DirectAction::ALL`, `PrefixAction::ALL`, `PopupAction::ALL`, `ModalAction::ALL`, `ScrollAction::ALL` (defined in `keymap.rs:242,280,312,356`), resolves each via `bindings.<scope>.binding_for(*action)`.
- **All five scopes** — Implemented, with labels `direct (no prefix)`, `in prefix mode`, `in agent switcher popup`, `in spawn minibuffer`, `in scroll mode` (`runtime.rs:5328,5337,5350,5359,5368`).
- **Configured prefix chord shown** — Implemented. `runtime.rs:5320-5323` renders `prefix:  {bindings.prefix}` via `KeyChord::Display`. With `prefix = "cmd+b"`, renders `super+b` (`keymap.rs:75-77` always prints `super`, not `cmd` — consistency choice at `keymap.rs:131-132`). AC says "Cmd+B" but screen will say `super+b`.
- **Custom binding (`spawn_agent = "s"`) shows `s`** — Implemented via `binding_for`.
- **Unbound actions render dimmed with "configure to enable"** — **Missing entirely**. No dimming, no hint. `binding_line` (`runtime.rs:5408-5410`) is `Line::raw(...)` with default style. `DirectBindings::binding_for(FocusLast)` falls back to `Tab` (`keymap.rs:813-815`) regardless of whether user bound it; help row is indistinguishable from a real binding. Keymap doc-comment at `keymap.rs:803-808` claims "the help line still renders, just dimmed by the runtime" — **renderer never delivers this**.
- **Mouse gestures alongside keystrokes** — Implemented as a separate `mouse:` section (`runtime.rs:5385-5401`) with `click`, `drag tab`, `drag pane`, `alt+drag`. **Drift**: AC says "alongside the keystroke bindings", but they live in their own section. Also a `wheel` line in scroll mode (`:5369-5372`) and a `type` line (`:5379-5382`).
- **`Esc` dismisses + any unbound key dismisses** — Implemented. `runtime.rs:3104-3111`: any `Press`/`Repeat` `Event::Key` while `HelpState::Open` flips to `Closed`. **Even the prefix key closes** (documented at `:3105-3107`); every key — including bound ones like `?` itself — dismisses.
- **Other gaps**: Help shows hardcoded `1-9` line (`runtime.rs:5345-5347`) and `wheel`/`type`/`alt+drag` rows that are NOT in any `Bindings` scope — string literals. AC-025's "every binding grouped by scope" doesn't acknowledge these. No tests assert rendered help text contains right chords; only help test (`prefix_question_mark_opens_help`) checks dispatch, not content.

### Suggested new ACs (Help)

- **AC-NEW-H01: Custom prefix chord renders in help header** — `runtime.rs:5320-5323` reads `bindings.prefix`; pin a test that `prefix = "cmd+b"` produces `super+b` (and document the `cmd → super` rendering normalization at `keymap.rs:75-77`).
- **AC-NEW-H02: Help is suppressed inside the spawn modal** — `runtime.rs:3129-3143`: when `spawn_ui` is `Some`, modal handler runs first; `?` becomes typed text in path field.
- **AC-NEW-H03: Help is suppressed inside the switcher popup** — `runtime.rs:3452-3475`: when `popup_state == Open`, popup keymap runs first; `?` doesn't open help.
- **AC-NEW-H04: Any keystroke dismisses help (not only Esc)** — `runtime.rs:3104-3111` discards every Press/Repeat. Promote to first-class assertion.
- **AC-NEW-H05: Prefix-mode digit-jump (`1`–`9`) appears in help** — Hardcoded at `runtime.rs:5345-5347`; lives outside the keymap. Document that this row exists and is non-rebindable today.
- **AC-NEW-H06: Mouse gestures are listed in the help screen** — `runtime.rs:5385-5401` — `click`, `drag tab`, `drag pane`, `alt+drag`, plus `wheel` (`:5369`) and `type` (`:5379`). Dedicated AC keeps them visible since they aren't keymap-driven.

### Cross-cutting notes (Help)

- The "`Bindings` POD is the single source of truth" claim at the bottom of AC-025 is **mostly true for keymap rows** but incomplete: roughly a third of the help screen (digit jump, wheel, type, mouse gestures) is hardcoded string literals in `render_help`. Those rows can drift from real behavior without any test catching it.
- The "full-screen" wording is aspirational; the code is a centered 64x50 popup. Either pick one as desired behavior and update the other, or relax AC text.
- The "unbound dimmed with 'configure to enable'" rendering is genuinely **missing** — `keymap.rs:803-808` documents intent the renderer never delivers. **Most concrete drift in AC-025**: the doc-comment promise is unfulfilled.
- No tests exercise `render_help` output. A snapshot-style test against a `TestBackend` (the pattern other parts of `runtime.rs` use, e.g. `:8795`) would close the loop and make every claim above falsifiable.

---

## Daemon (AC-026)

### Stage check

`docs/codemuxd-stages.md` is **deleted in working tree** (`git status`: `D codemuxd-stages.md`) but the last committed version (commit `46516d8`) shows **all six stages — 0 through 6 — checked off** as shipped. Stage 4 wired snapshot replay end-to-end on the daemon; Stage 5 wired SSH transport into the spawn modal; Stage 6 added the prepare/attach split with remote folder picker.

What AC-026 actually needs: Stages 0–4 cover daemon-side machinery (handshake, snapshot, alt-screen prefix, version-mismatch error path) — all shipped. Stages 5–6 cover bootstrap-driven attach — shipped. **The missing piece is "P1 persistence brings it back"** (the AC's step 3): a TUI restart that knows which agent to reconnect to, what host, and what `agent_id`. That work is **not in any stage**, was deferred per the stage-tracker's verification step 6 ("Reattach across TUI restart needs P1 persistence work — AD-7, separately scoped"), and is **actively defeated** by `daemon_agent_id_for` (see findings).

So: **daemon side is shipped; client-restart-and-reattach is unbuilt and structurally blocked.**

### AC-026 findings — **Partial**

Daemon-side mechanics are Implemented; the user-visible scenario in the AC is Missing.

- **Snapshot via `Screen::state_formatted`**: **Implemented**. `apps/daemon/src/session.rs:168-182` (`take_snapshot`) calls `screen.state_formatted()`, alt-screen check at `:177-179` prefixes `\x1b[?1049h`. Daemon-side parser fed at `apps/daemon/src/pty.rs:92-93` with `scrollback_len = 0` (visible grid only — comment at `pty.rs:86-91` explains why).
- **Snapshot is the first `PtyData` post-handshake**: **Implemented**. `session.rs:114-126` builds the snapshot, wraps in `Message::PtyData`, writes to socket *before* `conn::run_io_loops` starts. Test: `supervisor.rs:269-329` `snapshot_replays_screen_state_on_reattach`.
- **Drain-before-snapshot dedup**: **Implemented** (better than AC requires). `session.rs:174` drains `rx` under the parser lock so snapshot doesn't get followed by a duplicate replay of buffered bytes. Pinned by `supervisor.rs:344-398`.
- **Alt-screen `?1049h` prefix**: **Implemented**. `session.rs:177-179`. Tests: `session.rs:261-280` (primary screen omits prefix), `:287-314` (alt screen includes it).
- **Hello/HelloAck handshake**: **Implemented**. Wire at `crates/wire/src/messages.rs:35-48`. Daemon perform/dispatch at `apps/daemon/src/conn.rs:164-237` (10s timeout, version check with structured `Error{VersionMismatch}` reply). Client side at `crates/session/src/transport.rs:599-670`. Round-trip tests in `transport.rs:944-967`.
- **Client renders snapshot before live bytes — no blank-screen window**: **Implemented (free)**. Client treats snapshot as ordinary `PtyData`. Because `state_formatted` begins with `\x1b[H\x1b[J` (clear + home) and `?1049h` precedes alt-screen content, local vt100 parser reproduces daemon's grid before next live byte arrives.
- **"P1 persistence brings it back"**: **Missing AND structurally blocked**. No persistence module exists in `apps/tui/src/`, no `~/.cache/codemux/sessions.toml` reader, no agent-restoration on launch. Worse, `apps/tui/src/runtime.rs:1303-1311` `daemon_agent_id_for(tui_pid, spawn_counter)` formats agent id as `agent-{tui_pid}-{counter}`; comment at `:1305-1307` says: *"the bug being fixed here was the bootstrap silently re-attaching to the surviving daemon's socket and replaying its captured Claude PTY snapshot."* **The current design deliberately prevents the AC-026 scenario** — a TUI restart gets a new pid, generates a new agent id, bootstrap will spawn a fresh daemon (or hit the existing one's `PidFileLocked`).

#### Failure modes

- **"Daemon was killed between sessions → ERROR from bootstrap, slot enters Failed"**: **Cannot fail in the AC's scenario** because the AC's scenario doesn't exist. If, hypothetically, the user constructed an agent id manually and the daemon were dead, `apps/daemon/src/bootstrap.rs:210-250` would let the new daemon acquire the pid file (stale-reap path at `:240-246`) and `Session::spawn` would launch a **fresh** Claude — supervisor's `session_mut` at `supervisor.rs:124-139` always respawns on `child_exited()`. **This is itself a vision-principle-6 violation in waiting**: a reattach that finds a dead-but-restartable daemon would silently get a brand-new Claude session under the old label. No code today guards against it.
- **Wire-protocol mismatch on reconnect**: **Implemented** symmetrically. Daemon sends `Message::Error{VersionMismatch}` at `conn.rs:192-213`; client surfaces as `Error::Handshake` at `transport.rs:636-645`. Bootstrap's `prepare_remote` (`crates/codemuxd-bootstrap/src/lib.rs:309-330`) has redeploy path for *binary-version* mismatch (probe vs `bootstrap_version()`), and `force_respawn = binary_was_updated` (`:317`) kills stale daemon (`:790-869`). This matches AC-003's "re-deploy and retry once" promise on the binary side, but **wire-protocol mismatch from a still-current binary has no redeploy** — just bubbles up as `Error::Handshake` and slot enters `Failed`.

### Suggested new ACs (Daemon)

- **AC-NEW-D01: Daemon respawns Claude when child exits between attaches** — `supervisor.rs:124-139` silently spawns fresh `Session::spawn` if `child_exited()`. Vision principle 6 says this should be visible to the user. Either gate behind explicit "restart" action or send `Message::Error` so client can surface it. Currently undocumented.
- **AC-NEW-D02: Second client attaching to the same agent-id is sequenced, not rejected** — `ErrorCode::AlreadyAttached` (`crates/wire/src/messages.rs:113`) defined but supervisor's accept loop is sequential (`supervisor.rs:96-100`). Second concurrent client will block in `accept` until first disconnects. AC should pin "FIFO queue" or change to "second attach gets `AlreadyAttached`".
- **AC-NEW-D03: Snapshot honors client geometry on reattach** — `session.rs:97-112` resizes master to new client's `Hello` rows/cols, then `:173` resizes parser before `state_formatted`. Worth pinning: a client that reattaches at different size gets snapshot encoded for its own grid.
- **AC-NEW-D04: Daemon survives SSH disconnect via `setsid -f`** — listed in `codemuxd-stages.md` Stage 5 verification (step 5) and core to daemon's purpose, but nothing in `003` pins it. Code: `crates/codemuxd-bootstrap/src/lib.rs:746-918`.
- **AC-NEW-D05: Stale daemon is killed on binary upgrade** — `force_respawn` path at `lib.rs:860-873` SIGTERMs then SIGKILLs the prior daemon when `bootstrap_version()` differs. Doc-comment at `:751-769` calls out user-visible cost ("the in-flight Claude session on the remote dies").
- **AC-NEW-D06: Daemon binary build requires `cargo` on the remote** — `bootstrap.rs` builds via plain `cargo build --release`; no musl/static fallback.

**Deliberately NOT suggested**: idle-agent reap, max-agents-per-host, log rotation, daemon graceful shutdown on host reboot, version-skew compatible-but-different. Daemon is one-process-per-agent (AD-3), so resource limits are implicit in PTY/socket exhaustion.

### Cross-cutting notes (Daemon)

The gap between the AC and reality is **structural, not aspirational**: daemon-side reattach machinery is fully built and well-tested, but the client-side scenario the AC describes ("Quit codemux. Restart codemux. Reconnect to the same agent slot") is **deliberately impossible** today because `daemon_agent_id_for(tui_pid, spawn_counter)` namespaces ids by the TUI's pid. A new TUI process cannot generate the same agent id as the prior one. AC-026 depends on **AD-7 / P1 persistence**, which is not in `codemuxd-stages.md` and not in the roadmap excerpt I read. Until that ships, AC-026 only tests:

1. Mid-session SSH disconnect + bootstrap retry (re-handshake against same daemon, snapshot served) — **already covered by daemon tests** at `supervisor.rs:213-258, 269-329`.
2. The TUI-restart variant **cannot be exercised** without manual workarounds.

Recommend either (a) splitting AC-026 into "AC-026a: snapshot replay on mid-session reconnect" (testable today) and "AC-026b: TUI-restart reattach" (depends on P1 persistence + agent-id stability), or (b) marking AC-026 explicitly as "blocked on AD-7". AC-026 also currently swallows AC-NEW-D01 (silent Claude resurrection on dead-child reattach).

---

## Config and CLI (AC-027 — AC-029)

### Per-AC findings

- **AC-027: missing config → silent defaults** — **Implemented**.
  - Lookup at `apps/tui/src/config.rs:795-813`: `$XDG_CONFIG_HOME/codemux/config.toml` first, else `$HOME/.config/codemux/config.toml`. Empty `XDG_CONFIG_HOME` correctly treated as unset (test `empty_xdg_is_treated_as_unset`, `config.rs:899`).
  - Missing-file branch at `config.rs:777-780` returns `Config::default()` and logs only `tracing::debug!` — silent under default `codemux=info,codemux_tui=info,warn` filter (`main.rs:188`). No warning, no error.
  - Sub-claim: AC says "default `Bindings`, default `[ui]` segment list, and default scrollback length" — all three confirmed via `Config::default()` (`config.rs:74-84`) and unit tests.
  - **Edge not covered by AC-027**: `$HOME` AND `$XDG_CONFIG_HOME` both unset → `eyre!("$HOME is not set; cannot resolve config path")` at `config.rs:808` — this is a *loud* failure for a "missing config" scenario, **contradicts AC-027's "no error" promise**.

- **AC-028: invalid config fails loud before raw mode** — **Partial / Drift**.
  - Ordering verified: in `main.rs:116` `config::load()?` runs *before* `enable_raw_mode()` at `runtime.rs:1191`. Initial agent spawn (`runtime.rs:1180`) also pre-raw-mode, so config errors propagate up cleanly through `color_eyre`'s `Result` from `main`. Terminal stays in cooked mode on failure.
  - Malformed TOML → `toml::from_str` returns `Err`, wrapped by `wrap_err_with(|| format!("parse config at {}", path.display()))` at `config.rs:784`. Path *is* in message; toml's error includes offending key/value with line/col.
  - `prefix = "ctrl+nonsense"` → caught by `KeyChord::FromStr` at `keymap.rs:111-139`. String `nonsense` is the key part, falls into `parse_key_code`'s catch-all at `keymap.rs:169-176` and errors `"unknown key code: nonsense"`. Wrapped through serde's `de::Error::custom` (`keymap.rs:192`).
  - Malformed hex color in `[ui.host_colors]` → `ChromeColorVisitor::visit_str` at `config.rs:682-706` produces `de::Error::custom(format!("invalid hex color {value:?}; expected #rrggbb (six hex digits)"))`. Bad value IS quoted. Test `host_colors_malformed_hex_is_an_error` (`config.rs:1244`) pins rejection but not message format.
  - **Drift / risk**: AC-028 says stderr is "single-paragraph" with file path AND offending key/value. `color_eyre` actually renders multi-line backtraces (chained causes + spantrace + backtrace by default). On a fresh install without `RUST_BACKTRACE=0`, output is several lines, not a paragraph. Cause chain DOES include path and key/value, but formatting isn't what AC literally describes.
  - **Drift**: `Config` is **not** `#[serde(deny_unknown_fields)]` — see `config.rs:858-861` comment explicitly trading typo-safety for forward-compat. A typo like `[bindng]` would silently parse to defaults, NOT fail loud. AC-028 only covers "malformed TOML or invalid value" so this is in scope of the AC's silence, but worth flagging.

- **AC-029: invalid `[PATH]` fails loud + `--nav <invalid>` clap parse error** — **Drift on exact message text**, otherwise Implemented.
  - Validation at `main.rs:135-145` (`resolve_cwd`): `fs::canonicalize` failure → `wrap_err_with(|| format!("invalid path \`{}\`", path.display()))`. **Backticks, not single quotes** — AC literally says `invalid path '<path>'`. `is_dir()` failure → `eyre!("\`{}\` is not a directory", resolved.display())`. **Same drift**: backticks vs single quotes.
  - Tests at `main.rs:217-239` only assert `msg.contains("invalid path")` and `msg.contains("is not a directory")` — they DON'T pin the quoting style, so drift is invisible to test suite. Either fix AC to say "backticks" or change format strings.
  - Ordering: `resolve_cwd` runs at `main.rs:125-128`, before raw mode (`runtime.rs:1191`).
  - **Subtle bug**: `fs::canonicalize` resolves the path *before* the dir check, so the "is not a directory" message contains the *canonicalized* path (e.g. `/private/etc/passwd` on macOS), while "invalid path" contains the *user-supplied* path. Inconsistent surface.
  - `--nav <invalid>` → `NavStyle` derives `clap::ValueEnum` (`runtime.rs:179`), so clap rejects unknown values at parse time with standard `error: invalid value '<x>' for '--nav <NAV>' [possible values: left-pane, popup]`. Handled by framework; no test pins it but it's a clap guarantee.

### Suggested new ACs (Config and CLI)

- **AC-NEW-C01: `$HOME` and `$XDG_CONFIG_HOME` both unset → exit non-zero with readable error** — `config.rs:808` returns `eyre!("$HOME is not set; cannot resolve config path")`. Currently *contradicts* AC-027's "missing config = silent defaults" promise. Should be a separate AC pinning loud-fail behavior, or AC-027 should add it as a failure mode.
- **AC-NEW-C02: Empty config file (zero bytes or whitespace only) is treated as defaults** — Pinned by `empty_toml_yields_default_config` at `config.rs:821-826`. Distinct from "missing file"; users sometimes `touch ~/.config/codemux/config.toml` and expect it to work.
- **AC-NEW-C03: Unknown top-level config key is silently ignored, not an error** — Drift from typical "fail-loud" expectations. `config.rs:849-868` documents this explicitly (no `deny_unknown_fields`). User typo like `[bindng]` parses fine and binds nothing. If intentional, AC should pin it.
- **AC-NEW-C04: Config file unreadable (permissions) → exit non-zero before raw mode citing path** — Covered implicitly by `config.rs:781-782` (`read_to_string` wrap_err with path). Not currently tested.
- **AC-NEW-C05: `CODEMUX_NAV` env var is overridden by explicit `--nav` flag** — `main.rs:48` sets `env = "CODEMUX_NAV"` on clap arg, establishes precedence (CLI wins over env, env wins over default). AC-013's "Then" mentions both but doesn't pin precedence. Same applies to `CODEMUX_LOG` vs `--log`/`-l` (`main.rs:57`).
- **AC-NEW-C06: Hidden `statusline-tee` subcommand short-circuits before tracing/eyre/config** — `main.rs:100-103`. Per-turn invocation budget depends on this.
- **AC-NEW-C07: Default log path `~/.cache/codemux/logs/codemux.log` is created on first run; init failure exits non-zero before raw mode** — `main.rs:177-186`, runs before `config::load()` and well before `enable_raw_mode`.
- **AC-NEW-C08: Tilde expansion in config string fields (`scratch_dir`, `search_roots`) resolves at use-time, not load-time** — `config.rs:521-539` `expand_scratch`, `index_worker::expand_remote_roots`. User-visible because tilde in `[spawn] scratch_dir` survives a `$HOME` change between TUI restart and spawn, but a relative path silently falls back to platform default (`config.rs:534-538` is `tracing::warn!`, not startup error).
- **AC-NEW-C09: Conflicting bindings (two actions on the same chord) are NOT detected at load time** — Searched `keymap.rs` for conflict detection; none exists. Runtime's `dispatch_key` resolves by lookup order, which means whichever action is checked first wins. Either add detection or pin the silent-resolution behavior.

### Cross-cutting notes (Config and CLI)

- The binary edge uses `color_eyre` (`main.rs:105`), installed *after* the statusline-tee fast path but *before* tracing/config. All `Result<()>` errors out of `main` get color-eyre's chained-cause + backtrace formatting on stderr. This satisfies "loud failure with context" but **does not produce the "single-paragraph" output AC-028 promises**.
- Error-message string drift on `[PATH]` validation: code uses backticks, AC text uses single quotes. Existing tests use `.contains()` so drift is invisible.
- There are **no integration tests at the binary level** (`apps/tui/tests/` doesn't exist) — every assertion is a unit test on the deserializer or `resolve_cwd`. AC-028's "the user's terminal is left in its pre-launch state" claim is therefore unverified by any test; it depends solely on the ordering in `main.rs`.
- `Config` lacks `#[serde(deny_unknown_fields)]` by deliberate design (`config.rs:858-861`) — forward-compat over typo-safety. **The single biggest gap between "fail-loud config" rhetoric in AC-028 and actual behavior**: typos in section names or scalar field names silently become defaults.
