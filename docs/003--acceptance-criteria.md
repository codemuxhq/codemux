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

**Tests:**
- `apps/tui/src/spawn.rs::empty_path_with_no_selection_emits_spawn_scratch` — pins the modal's scratch-fallback emit when the user presses Enter with nothing typed/selected against a remote host.
- `apps/tui/src/spawn.rs::empty_local_path_with_no_selection_emits_spawn_scratch` — same gesture for the local-host placeholder.
- `apps/tui/src/spawn.rs::auto_seeded_path_with_no_selection_emits_spawn_scratch` — covers the real-world fuzzy-mode flow where the path is auto-seeded but neither typed nor picked.
- `apps/tui/src/config.rs::spawn_scratch_dir_defaults_to_dotcodemux_scratch` — pins the default `~/.codemux/scratch` value.
- `apps/tui/src/config.rs::expand_scratch_expands_tilde_against_home`, `expand_scratch_bare_tilde_resolves_to_home`, `expand_scratch_absolute_path_passes_through`, `expand_scratch_returns_none_for_relative_path`, `expand_scratch_returns_none_when_tilde_but_no_home` — pin the path-resolution rules.
- `apps/tui/src/runtime.rs::resolve_remote_scratch_cwd_returns_none_when_path_unresolvable`, `resolve_remote_scratch_cwd_returns_dir_on_mkdir_success`, `resolve_remote_scratch_cwd_returns_none_on_mkdir_failure` — pin the runtime-side resolution / mkdir-on-demand behavior.
- (uncovered: full local-spawn flow that creates the scratch dir and lands a Ready tab; failure-mode fallback to platform default cwd on resolution failure.)

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

**Tests:**
- `crates/codemuxd-bootstrap/src/lib.rs::prepare_remote_happy_path_on_fresh_host` — pins the cold-start path: probe → tarball stage → SCP → remote build → daemon spawn against a fake `CommandRunner`.
- `crates/codemuxd-bootstrap/src/lib.rs::prepare_remote_skips_install_when_version_matches` — pins the warm-start optimization (no `TarballStage`/`Scp`/`RemoteBuild` when versions match).
- `crates/codemuxd-bootstrap/src/lib.rs::probe_returns_trimmed_version_on_success`, `probe_returns_none_version_when_agent_version_missing`, `probe_surfaces_spawn_failure_as_bootstrap_error`, `probe_surfaces_ssh_connection_failure_as_bootstrap_error`, `probe_surfaces_empty_stdout_as_bootstrap_error` — pin the `VersionProbe` stage outcomes.
- `crates/codemuxd-bootstrap/src/lib.rs::scp_failure_carries_scp_stage` — pins the `Scp` failure-mode tagging.
- `crates/codemuxd-bootstrap/src/lib.rs::remote_build_surfaces_cargo_not_found_hint`, `remote_build_surfaces_cargo_not_found_hint_on_zsh`, `remote_build_surfaces_cargo_not_found_hint_on_sh`, `remote_build_generic_failure_carries_stage`, `remote_build_uses_install_for_hardlink_safety` — pin the `RemoteBuild` failure modes including the cargo-missing diagnostic.
- `crates/codemuxd-bootstrap/src/lib.rs::spawn_remote_daemon_propagates_remote_stderr`, `spawn_remote_daemon_rejects_quote_in_cwd`, `spawn_remote_daemon_redirects_stdin_to_devnull_and_stderr_to_sibling_file`, `spawn_remote_daemon_failure_branch_tails_both_log_and_stderr`, `spawn_remote_daemon_omits_cwd_flag_when_none`, `spawn_remote_daemon_includes_cwd_flag_when_some`, `spawn_remote_daemon_verifies_socket_appearance_post_spawn` — pin the `DaemonSpawn` stage.
- `crates/codemuxd-bootstrap/src/lib.rs::open_ssh_tunnel_uses_absolute_remote_path_in_forward_spec`, `open_ssh_tunnel_bypasses_ssh_control_master`, `open_ssh_tunnel_sets_server_alive_opts` — pin the `SocketTunnel` stage.
- `crates/codemuxd-bootstrap/src/lib.rs::connect_socket_times_out_with_socket_connect_stage`, `connect_socket_succeeds_against_live_socket` — pin the `SocketConnect` stage.
- `crates/codemuxd-bootstrap/src/lib.rs::attach_socket_happy_path_against_fake_runner`, `attach_socket_expands_tilde_cwd_against_remote_home` — pin the end-of-bootstrap attach.
- `apps/daemon/src/supervisor.rs::handshake_version_mismatch_returns_error_frame` — pins the wire-protocol-mismatch error path.
- (uncovered: real end-to-end SSH cold-start that walks every stage indicator and surfaces a `HelloAck`-driven Ready tab.)

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

**Tests:**
- `apps/tui/src/spawn.rs::tab_in_path_zone_applies_highlighted_candidate` — pins Tab applying the highlighted wildmenu candidate to the path field.
- `apps/tui/src/spawn.rs::tab_in_path_zone_with_no_selection_is_noop` — pins the no-selection no-op.
- `apps/tui/src/spawn.rs::down_cycles_with_wrap` — pins arrow-key navigation across candidates.
- `apps/tui/src/spawn.rs::remote_completions_populates_wildmenu_from_list_dir`, `remote_completions_filters_by_basename_prefix`, `remote_completions_filters_out_files`, `remote_completions_caches_listing_and_filters_in_process_on_narrow`, `remote_completions_re_shells_when_user_crosses_a_slash`, `remote_completions_returns_empty_on_list_dir_error`, `remote_completions_show_dot_dirs_when_prefix_starts_with_dot` — pin remote `RemoteFs::list_dir`-backed completion shape.
- `apps/tui/src/spawn.rs::host_completions_returns_full_pool_for_empty_input`, `host_completions_with_typed_prefix_pins_local_first_when_it_matches`, `host_completions_filters_by_substring`, `host_completions_prefers_prefix_matches`, `host_completions_omits_local_when_input_does_not_match_it` — pin host-pool ranking.
- `apps/tui/src/spawn.rs::scan_dir_filters_out_files` — pins local `read_dir`-backed completion filtering.
- `apps/tui/src/spawn.rs::wildmenu_scroll_offset_no_selection_is_zero`, `wildmenu_scroll_offset_selection_within_window_is_zero`, `wildmenu_scroll_offset_slides_when_selection_below_window`, `wildmenu_scroll_offset_zero_usable_does_not_panic` — pin the six-row sliding-window math.
- `crates/codemuxd-bootstrap/src/remote_fs.rs::list_dir_invokes_ssh_with_correct_flags`, `list_dir_includes_batchmode_yes`, `list_dir_truncates_at_max_list_entries`, `list_dir_rejects_path_with_single_quote` — pin the `MAX_LIST_ENTRIES` cap and SSH wiring on the remote side.
- (uncovered: end-to-end with a real SSH ControlMaster socket.)

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

**Tests:**
- `apps/tui/src/spawn.rs::tilde_in_fuzzy_with_empty_query_enters_navigation_at_home` — pins `~` switching from fuzzy to precise + seeding `$HOME`.
- `apps/tui/src/spawn.rs::tilde_in_precise_with_autoseeded_path_enters_navigation_at_home` — pins the same flow when the path was auto-seeded.
- `apps/tui/src/spawn.rs::tilde_in_precise_with_user_typed_path_is_literal` — pins that user-typed paths treat `~` as a literal char (no quick-switch).
- `apps/tui/src/spawn.rs::combining_tilde_in_fuzzy_with_empty_query_enters_navigation_at_home` — pins the U+0303 / U+02DC compose-key variants.
- `apps/tui/src/spawn.rs::space_after_combining_tilde_is_swallowed`, `literal_tilde_does_not_arm_compose_swallow` — pin the compose-arm follow-up gesture.
- `apps/tui/src/spawn.rs::tilde_in_fuzzy_remote_mode_expands_to_remote_home` — pins remote `$HOME` seeding for SSH agents.
- `apps/tui/src/spawn.rs::slash_in_fuzzy_with_empty_query_enters_navigation_at_root` — pins `/` switching modes and seeding `/`.
- `apps/tui/src/spawn.rs::slash_in_precise_with_autoseeded_path_enters_navigation_at_root` — same for already-precise mode.
- `apps/tui/src/spawn.rs::slash_in_precise_is_a_literal_char`, `slash_in_host_zone_is_a_literal_char_not_an_auto_switch` — pin the negative cases (no quick-switch when the field has been touched, or in the host zone).

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

**Tests:**
- `apps/tui/src/spawn.rs::tab_descends_into_folder_in_one_step` — pins step 2 (Tab descends, selection clears, wildmenu refreshes).
- `apps/tui/src/spawn.rs::descend_sets_just_descended_flag` — pins the post-descend visual flag.
- `apps/tui/src/spawn.rs::next_keystroke_clears_just_descended` — pins the one-frame lifetime of that flag.
- `apps/tui/src/spawn.rs::typing_slash_after_first_tab_also_descends` — pins the `/` keystroke as an alternative to Tab.
- `apps/tui/src/spawn.rs::enter_with_selection_in_precise_descends` — pins Enter on a highlighted candidate descending rather than spawning.
- `apps/tui/src/spawn.rs::enter_without_selection_in_precise_spawns` — pins the commit gesture (step 4).
- `apps/tui/src/spawn.rs::tab_is_no_op_in_fuzzy_path_zone` — pins that Tab does not descend in fuzzy mode.
- `apps/tui/src/spawn.rs::fuzzy_highlighted_candidate_spawns_on_enter` — pins fuzzy's apply-and-spawn-in-one-step Enter semantic.

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

**Tests:**
- `apps/tui/src/spawn.rs::build_project_meta_collapses_missing_or_empty_host_to_none`, `build_project_meta_keys_by_literal_path_for_host_bound_entries`, `build_project_meta_carries_name_for_both_local_and_host_bound`, `build_project_meta_first_wins_on_duplicate_path` — pin the named-project lookup table the renderer/scorer leans on.
- `apps/tui/src/spawn.rs::host_bound_tilde_project_round_trips_as_literal_path`, `local_only_tilde_project_emits_locally_expanded_candidate` — pin the host-bound vs local resolution rules.
- `apps/tui/src/spawn.rs::named_project_row_renders_host_badge_next_to_name`, `named_project_row_omits_badge_when_local`, `named_project_row_unselected_lays_out_star_name_dim_path`, `named_project_row_selected_applies_highlight_at_line_level` — pin the wildmenu row layout (star + name + `@host` badge).
- `apps/tui/src/config.rs::spawn_named_projects_round_trip`, `spawn_named_project_host_round_trips`, `spawn_named_project_empty_host_normalises_to_none`, `spawn_named_project_missing_path_is_an_error` — pin config parsing.
- (uncovered: end-to-end Enter-to-spawn ordering with the bound-host prepare path; the explicit `BOOST_NAMED = 1000` ranking against fuzzy directory hits.)

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

**Tests:**
- `apps/tui/src/spawn.rs::esc_in_nav_mode_with_no_selection_closes_modal` — pins the immediate-close gesture.
- `apps/tui/src/spawn.rs::esc_in_search_mode_clears_filter_chars` — pins the first-Esc-clears-filter, second-Esc-closes flow for the path zone search variant.
- `apps/tui/src/spawn.rs::esc_in_nav_mode_with_selection_clears_selection` — pins the selection-clear-before-close gesture.
- `apps/tui/src/spawn.rs::esc_in_host_zone_returns_to_path_with_cwd_reseeded`, `esc_in_host_zone_preserves_user_typed_path` — pin the host-zone Esc semantics (back to path, not close).
- `apps/tui/src/spawn.rs::lock_for_bootstrap_with_esc_emits_cancel_bootstrap` — pins Esc cancelling an in-flight bootstrap.
- `apps/tui/src/fuzzy_worker.rs::drop_disconnects_channel_and_exits_worker` — pins worker `Drop` cleanup.
- (uncovered: the runtime-level guarantee that closing returns focus to the previously-focused agent.)

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

**Tests:**
- `apps/tui/src/spawn.rs::open_seeds_path_with_cwd_and_marks_auto_seeded`, `open_does_not_double_slash_when_cwd_already_ends_in_slash`, `open_precise_seeds_path_with_cwd` — pin that `SpawnMinibuffer::open(cwd, …)` seeds the path zone with the supplied cwd. The runtime always passes the TUI startup cwd to `open`; this AC's "not the focused agent's cwd" assertion is a property of the call site, which is not directly pinned.
- (uncovered: the runtime-level call-site assertion that the cwd passed in is the startup cwd, never the focused agent's cwd.)

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

**Tests:**
- `apps/tui/src/spawn.rs::ctrl_modified_keys_are_dropped` — pins that direct chords (Ctrl+letter etc.) routed through the modal are dropped, not forwarded.
- `apps/tui/src/spawn.rs::lock_for_bootstrap_drops_typing_keys` — pins that typing keystrokes are dropped while the modal is locked for bootstrap.
- (uncovered: the runtime-level proof that an open modal short-circuits `dispatch_key` for prefix chord and `?`.)

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

**Tests:**
- `apps/tui/src/index_manager.rs::drain_on_building_appends_progress_batch_to_dirs`, `drain_on_refreshing_with_progress_drops_batch_and_preserves_cache`, `drain_on_building_done_ok_transitions_to_ready_and_returns_dirs`, `drain_on_refreshing_done_err_falls_back_to_cached_ready`, `drain_on_building_done_err_transitions_to_failed`, `drain_returns_none_for_settled_state`, `drain_on_refreshing_with_no_events_returns_none` — pin the manager-side state machine the runtime ticks via `try_recv`.
- `apps/tui/src/fuzzy_worker.rs::set_index_then_query_emits_matching_result`, `query_burst_collapses_to_latest`, `query_for_unknown_host_is_dropped`, `set_index_for_new_host_clears_pending_query`, `empty_query_is_a_no_op`, `drop_disconnects_channel_and_exits_worker` — pin the off-thread fuzzy worker.
- `apps/tui/src/index_worker.rs::cancel_during_walk_terminates_quickly` — pins that the walker drops promptly when the modal closes.
- `apps/tui/src/index_worker.rs::progress_events_emit_during_large_walk` — pins the progress-event stream feeding the spinner/count.
- `apps/tui/src/index_worker.rs::nonexistent_root_returns_no_roots_error`, `ignore_file_excludes_listed_dirs`, `tmpdir_with_nested_dirs_yields_all_dirs` — pin the local walker behavior.
- `apps/tui/src/spawn.rs::ctrl_t_toggles_fuzzy_to_precise_and_seeds_cwd`, `ctrl_t_toggles_precise_to_fuzzy` — pin the `Ctrl+T` synchronous-mode escape.
- `apps/tui/src/spawn.rs::ctrl_r_emits_refresh_index_outcome_in_fuzzy_mode` — pins `Ctrl+R` rebuild trigger.
- (uncovered: the modal-side guarantee that keystroke handling stays interactive while the worker is running, including the "no auto-refresh on index-done; user must press one more key" rule; spinner-sentinel rendering.)

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

**Tests:**
- `apps/tui/src/runtime.rs::prefix_l_via_alias_focuses_next`, `prefix_h_via_alias_focuses_prev` — pin the vim-style aliases dispatching to FocusNext / FocusPrev.
- `apps/tui/src/runtime.rs::direct_cmd_apostrophe_focuses_next_without_arming_prefix`, `direct_cmd_semicolon_focuses_prev` — pin the direct `Cmd+'` and `Cmd+;` chords.
- `apps/tui/src/runtime.rs::prefix_then_repeated_nav_keys_keeps_dispatching` — pins repeated nav keys staying sticky (h h h works without re-arming).
- `apps/tui/src/keymap.rs::prefix_focus_next_aliases_all_resolve_to_focus_next`, `prefix_focus_prev_aliases_all_resolve_to_focus_prev` — pin all alias chords (`n`/`l`/`j`/Right/Down for next; `p`/`h`/`k`/Left/Up for prev).
- (uncovered: the `NavState`-level wraparound on cycle (`A → B → C → A`) and the SIGWINCH-not-fired-on-focus-change invariant.)

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

**Tests:**
- `apps/tui/src/runtime.rs::prefix_digit_focuses_by_one_indexed_position` — pins the 1–9 dispatch into FocusAt with one-indexed mapping.
- `apps/tui/src/runtime.rs::prefix_zero_is_consumed_no_focus` — pins that `0` is consumed without a focus change.
- `apps/tui/src/runtime.rs::prefix_then_digit_stays_sticky` — pins the sticky-nav behavior after a digit dispatch.
- `apps/tui/src/runtime.rs::prefix_then_non_nav_command_exits_sticky`, `prefix_then_unbound_key_exits_sticky`, `prefix_then_esc_exits_sticky_via_unbound_path` — pin the drop-out paths.
- (uncovered: the AC's specific failure mode — out-of-range digit (e.g. `prefix 9` with 4 agents) staying sticky.)

### AC-011: Bounce to the previously-focused agent

**Given:**
- Two agents `A` and `B`. The user just focused `B` from `A`.

**When:**
1. Press the prefix, then `Tab`.

**Then:**
- Focus returns to `A`. Pressing `Tab` again returns to `B`. The two-slot bounce is symmetric.

**Failure modes:**
- **Only one agent exists, or no prior focus is recorded:** the chord is a no-op.

**Tests:**
- `apps/tui/src/runtime.rs::prefix_tab_dispatches_focus_last`, `prefix_then_tab_stays_sticky` — pin the prefix+Tab → FocusLast dispatch.
- `apps/tui/src/runtime.rs::change_focus_lets_alt_tab_bounce_via_two_calls` — pins the symmetric two-slot bounce: A→B→A→B all driven by `previous_focused`.
- `apps/tui/src/runtime.rs::change_focus_records_previous_when_focus_moves`, `change_focus_is_a_noop_when_target_is_already_focused` — pin the `previous_focused` accounting that powers the bounce.
- `apps/tui/src/keymap.rs::direct_binding_for_focus_last_falls_back_to_tab_when_unbound` — pins the keymap fallback for the bounce action.

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

**Tests:**
- `apps/tui/src/runtime.rs::snapshot_navigator_popup` — insta snapshot of the popup overlay rendered against two agents; pins layout including the focused-row marker.
- `apps/tui/src/runtime.rs::dismiss_clamps_open_popup_selection`, `remove_at_decrements_popup_selection_when_removing_an_earlier_index`, `remove_at_closes_popup_when_last_agent_removed` — pin the "stale highlight never focuses a removed slot" invariant when the popup is open across reaps.
- `apps/tui/src/runtime.rs::no_overlay_active_returns_false_when_popup_open` — pins the popup-as-overlay flag.
- `apps/tui/src/keymap.rs::popup_lookup_round_trip` — pins the popup-scope key lookup.
- `apps/tui/src/runtime.rs::label_spans_renders_spinner_glyph_when_working`, `label_spans_omits_spinner_when_not_working`, `label_spans_renders_focused_spinner_with_reverse_style`, `label_spans_renders_host_prefix_when_provided`, `label_spans_omits_host_prefix_when_absent`, `label_spans_renders_spinner_before_host`, `label_spans_renders_attention_dot_when_unfocused_and_flagged`, `label_spans_omits_attention_dot_when_focused`, `label_spans_omits_attention_dot_when_not_flagged` — pin the row composition (spinner + host prefix + attention dot + label).
- (uncovered: end-to-end Enter-to-focus-and-close gesture; Esc-to-close-without-changing-focus.)

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

**Tests:**
- `apps/tui/src/runtime.rs::render_left_pane_records_one_hitbox_per_agent`, `render_left_pane_hitboxes_skip_borders_and_advance_one_row_per_agent`, `render_left_pane_drops_rows_that_overflow_the_pane` — pin the LeftPane chrome layout.
- `apps/tui/tests/pty_nav.rs::chrome_flips_from_popup_to_leftpane_on_prefix_v` — boots codemux in an 80x24 PTY, asserts the initial Popup chrome (no ` agents ` navigator title), sends `Ctrl+B v`, and asserts the LeftPane chrome appears (the navigator's bordered ` agents ` block lands on screen). Pins the `prefix v` dispatch path through the real keymap.
- (uncovered: the SIGWINCH on chrome flip, the return-to-Popup on a second `prefix v`, and the `--nav left-pane` / `CODEMUX_NAV` launch-time selectors.)

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

**Tests:**
- `apps/tui/src/runtime.rs::change_focus_records_previous_when_focus_moves` — pins the `previous_focused` accounting that fires at the spawn-time `change_focus(new_idx)`.
- `apps/tui/src/runtime.rs::change_focus_lets_alt_tab_bounce_via_two_calls` — pins that the recorded prior focus drives a subsequent FocusLast bounce.
- (uncovered: the spawn-site call into `change_focus` itself; the no-prior-agent edge case staying `None`.)

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

**Tests:**
- `apps/tui/src/runtime.rs::kill_focused_clamps_focus_when_killing_last_tab` — pins the kill-then-clamp behavior named in the AC body.
- `apps/tui/src/runtime.rs::dismiss_removes_focused_crashed_agent_and_clamps_focus` — pins the same clamp invariant for the dismiss path.
- `apps/tui/src/runtime.rs::dismiss_clears_previous_focused_when_it_collides_with_focused`, `dismiss_clears_stale_previous_focused`, `remove_at_clears_previous_focused_when_it_points_at_removed_slot` — pin the `previous_focused` cleanup when the removed slot equals (or invalidates) the bounce slot.

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

**Tests:**
- `apps/tui/src/runtime.rs::prefix_x_dispatches_kill_agent` — pins the prefix+x dispatch into KillAgent.
- `apps/tui/src/runtime.rs::kill_focused_removes_ready_agent` — pins that kill works on a live Ready agent (no Ready guard).
- `apps/tui/src/runtime.rs::kill_focused_removes_failed_agent` — pins parity with terminal-state agents.
- `apps/tui/src/runtime.rs::kill_focused_no_op_on_empty_vec` — pins the empty-list no-op.
- `apps/tui/src/runtime.rs::kill_focused_clamps_focus_when_killing_last_tab` — pins the focus-clamp on tail removal.
- `apps/tui/src/runtime.rs::remove_at_decrements_focused_when_removing_an_earlier_index` — pins focus following the agent across an upstream removal.
- `apps/tui/tests/pty_lifecycle.rs::kill_last_agent_auto_exits_codemux` — boots codemux in an 80x24 PTY, waits for the fake agent's prompt, sends `Ctrl+B x`, and asserts codemux exits cleanly. Pins the chord-to-`KillAgent` dispatch end-to-end (and the AC-036 auto-exit-on-empty-vec path it cascades into).

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

**Tests:**
- `apps/tui/src/runtime.rs::prefix_d_dispatches_dismiss_agent` — pins the prefix+d dispatch into DismissAgent.
- `apps/tui/src/runtime.rs::dismiss_no_op_on_focused_ready_agent` — pins step 1 (no-op on live).
- `apps/tui/src/runtime.rs::dismiss_removes_focused_failed_agent` — pins step 2 (removes Failed).
- `apps/tui/src/runtime.rs::dismiss_removes_focused_crashed_agent_and_clamps_focus` — pins step 3 (removes Crashed and clamps focus).
- `apps/tui/src/runtime.rs::dismiss_leaves_empty_vec_when_last_agent_dismissed`, `dismiss_removes_crashed_zero_slot` — pin the edge cases.

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

**Tests:**
- `apps/tui/src/runtime.rs::prefix_q_exits` — pins the prefix+q dispatch into Exit.
- `apps/tui/src/runtime.rs::user_can_remap_quit_to_a_different_key` — pins that the Exit dispatch follows the configured chord.
- (uncovered: the `TerminalGuard::drop` teardown sequence (alt screen, mouse, KKP, title, stdin drain) and exit-code 0 outcome.)

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

**Tests:**
- `apps/tui/src/runtime.rs::reap_silently_removes_clean_exit` — pins exit-0 silently shrinking the agent vec.
- `apps/tui/src/runtime.rs::reap_clean_exit_clamps_focus_to_surviving_agent` — pins focus following the surviving agent.
- `apps/tui/src/runtime.rs::dismiss_leaves_empty_vec_when_last_agent_dismissed` — pins the empty-vec post-condition that triggers `event_loop` exit.
- `apps/tui/tests/pty_lifecycle.rs::kill_last_agent_auto_exits_codemux` — drives the `Ctrl+B x` chord at the only agent through a real PTY and asserts the codemux process exits 0 within a timeout. Pins the `event_loop` empty-vec → `return Ok(())` branch end-to-end. (Same test also covers AC-014.)
- (uncovered: the full TerminalGuard teardown sequence — alt screen, KKP pop, stdin drain — at the byte level. The PTY test asserts process exit but does not parse the cleanup escape sequences emitted on the way out.)

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

**Tests:**
- `apps/tui/src/runtime.rs::reap_transitions_ready_with_dead_transport_to_crashed` — pins the Ready → Crashed transition on non-zero exit.
- `apps/tui/src/runtime.rs::reap_leaves_alive_ready_agent_in_ready_state`, `reap_leaves_failed_agent_alone`, `reap_leaves_already_crashed_agent_alone` — pin the negative cases.
- `apps/tui/src/runtime.rs::mark_crashed_transitions_ready_to_crashed_preserving_parser_and_exit_code` — pins parser preservation for post-crash scrollback.
- `apps/tui/src/runtime.rs::nudge_scrollback_moves_offset_on_crashed_agent`, `snap_to_live_resets_offset_on_crashed_agent`, `jump_to_top_works_on_crashed_agent`, `is_working_returns_false_for_crashed_agent`, `title_returns_last_title_on_crashed_agent` — pin scrollback access continuing after the crash transition.
- `apps/tui/src/runtime.rs::render_agent_pane_paints_red_banner_for_nonzero_exit_code` — pins the red crash-banner render.
- `apps/tui/src/runtime.rs::render_agent_pane_paints_connection_lost_banner_for_minus_one` — pins the daemon-EOF (`-1`) variant.
- `apps/tui/src/runtime.rs::render_agent_pane_falls_through_to_red_banner_for_synthetic_zero_exit` — pins the synthetic-zero (kill-by-codemux) banner case.
- `apps/tui/src/runtime.rs::render_agent_pane_banner_uses_configured_dismiss_chord` — pins that the banner shows the rebound dismiss chord.

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

**Tests:**
- `apps/tui/src/runtime.rs::scrollback_zero_len_means_no_history` — pins the `scrollback_len = 0` failure-mode clause named in the AC.
- `apps/tui/src/runtime.rs::scrollback_set_back_round_trips`, `scrollback_offset_auto_bumps_when_new_rows_evict`, `scrollback_clamps_to_buffer_length_at_top`, `scrollback_state_is_per_parser` — pin the vt100 invariants codemux's scroll mode leans on.
- `apps/tui/src/runtime.rs::nudge_scrollback_moves_offset_into_history_then_back` — pins wheel-up / PageUp navigation.
- `apps/tui/src/runtime.rs::nudge_scrollback_saturates_at_zero_on_negative_overflow` — pins the wheel-down floor.
- `apps/tui/src/runtime.rs::nudge_scrollback_only_touches_focused_agent`, `nudge_scrollback_no_op_on_failed_agent` — pin scope.
- `apps/tui/src/runtime.rs::jump_to_top_clamps_to_buffer_length` — pins step 3 (`g` jumps to top).
- `apps/tui/src/runtime.rs::snap_to_live_resets_offset_to_zero`, `snap_to_live_no_op_on_failed_agent`, `scrollback_offset_returns_zero_for_failed_agent` — pin step 4 (`G` snaps to live).
- `apps/tui/src/runtime.rs::render_scroll_indicator` (callers); `apps/tui/src/keymap.rs::scroll_lookup_round_trip`, `scroll_defaults_cover_arrow_pgup_pgdn_g_capital_g_esc` — pin the scroll-mode keymap.
- (uncovered: the no-SIGWINCH-fired-during-scroll invariant.)

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

**Tests:**
- `apps/tui/src/runtime.rs::snap_to_live_resets_offset_to_zero` — pins step 1 (typing snaps to live before the byte is forwarded; method-level guarantee).
- `apps/tui/src/runtime.rs::nudge_scrollback_only_touches_focused_agent` — pins that scroll state is per-agent (step 4 returns to A's offset 50).
- `apps/tui/src/runtime.rs::scrollback_state_is_per_parser` — pins the cross-parser independence the per-agent guarantee leans on.
- (uncovered: the dispatch-order assertion that `snap_to_live` runs *before* the byte write at the runtime layer; the navigation-chord-non-snap path.)

### AC-039: Pasting while scrolled-back snaps to live before the bracketed-paste write

**Given:**
- The focused agent is scrolled back (offset > 0).

**When:**
1. The user pastes (terminal sends a `Event::Paste` event with the bracketed-paste content).

**Then:**
- The runtime calls `snap_to_live()` *before* writing the paste payload to the PTY (same ordering rule as typing; see AC-018).
- The user never pastes into a window they cannot see.

**Failure modes:** none.

**Tests:**
- `apps/tui/src/runtime.rs::snap_to_live_resets_offset_to_zero` — pins the underlying snap operation.
- `apps/tui/src/runtime.rs::wrap_paste_emits_brackets_around_plain_text`, `wrap_paste_strips_embedded_esc_to_block_end_marker_injection` — pin the bracketed-paste payload that runs *after* the snap.
- (uncovered: the dispatch-order assertion that `snap_to_live` runs before the paste payload is written at the runtime layer.)

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

**Tests:**
- `apps/tui/src/runtime.rs::tab_mouse_dispatch_down_on_tab_returns_press`, `tab_mouse_dispatch_up_same_tab_is_a_click` — pin the press → release-on-same-tab click flow.
- `apps/tui/src/runtime.rs::tab_mouse_dispatch_down_outside_tabs_returns_none`, `tab_mouse_dispatch_up_outside_tabs_cancels`, `tab_mouse_dispatch_up_with_no_press_is_none`, `tab_mouse_dispatch_non_left_buttons_are_ignored` — pin the negative cases.
- `apps/tui/src/runtime.rs::tab_hitboxes_at_finds_recorded_rect`, `tab_hitboxes_at_misses_outside_recorded_rects`, `tab_hitboxes_clear_drops_all_recorded_rects`, `tab_hitboxes_record_rejects_zero_sized_rect` — pin the hitbox lookup that resolves clicks to agent ids (not slot indices).
- `apps/tui/src/runtime.rs::render_status_bar_records_one_hitbox_per_agent_in_order`, `render_status_bar_hitboxes_sit_on_the_status_row_and_are_non_overlapping`, `render_status_bar_clears_stale_hitboxes_first` — pin the renderer recording the hitboxes.

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

**Tests:**
- `apps/tui/src/runtime.rs::tab_mouse_dispatch_up_different_tab_is_a_reorder` — pins press-on-A → release-on-C dispatching as a reorder.
- `apps/tui/src/runtime.rs::reorder_agents_drag_right_inserts_at_destination`, `reorder_agents_drag_left_inserts_at_destination`, `reorder_agents_noop_on_self_or_out_of_range` — pin the browser-tab `remove + insert` semantics.
- `apps/tui/src/runtime.rs::reorder_followed_by_shift_index_keeps_focus_on_the_moved_agent`, `reorder_a_non_focused_tab_past_the_focused_one_keeps_focus_pinned_to_its_agent` — pin focus identity preservation across the reorder.
- `apps/tui/src/runtime.rs::shift_index_*` — pin the focus-following arithmetic.
- `apps/tui/src/runtime.rs::tab_mouse_dispatch_drag_is_none_so_event_loop_keeps_state` — pins the drag-in-flight no-op until release.
- `apps/tui/src/runtime.rs::render_left_pane_records_one_hitbox_per_agent` — pins LeftPane sharing the same hitbox path (so the gesture works in both chromes).

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

**Tests:**
- `apps/tui/src/runtime.rs::pane_mouse_dispatch_down_inside_pane_arms_selection`, `pane_mouse_dispatch_drag_extends_when_selection_active`, `pane_mouse_dispatch_drag_outside_pane_clamps`, `pane_mouse_dispatch_drag_without_selection_is_none`, `pane_mouse_dispatch_up_with_selection_commits`, `pane_mouse_dispatch_up_without_selection_is_none`, `pane_mouse_dispatch_down_outside_pane_returns_none`, `pane_mouse_dispatch_right_button_is_none` — pin the selection lifecycle from press through commit.
- `apps/tui/src/runtime.rs::pane_hitbox_cell_at_translates_to_pane_relative`, `pane_hitbox_cell_at_returns_none_outside_pane`, `pane_hitbox_clamped_cell_at_clamps_to_nearest_edge`, `pane_hitbox_no_record_means_no_dispatch` — pin the cell-coordinate translation.
- `apps/tui/src/runtime.rs::normalized_range_handles_inverted_drag`, `normalized_range_preserves_already_ordered_drag`, `row_bounds_*` — pin the selection-range math.
- `apps/tui/src/runtime.rs::write_clipboard_to_emits_osc_52_with_base64_payload`, `write_clipboard_to_handles_empty_string_as_empty_payload`, `write_clipboard_to_round_trips_multibyte_utf8` — pin the OSC 52 payload framing.
- `apps/tui/src/runtime.rs::vt100_contents_between_extracts_selection_substring`, `vt100_contents_between_handles_multirow_selection`, `failure_text_in_range_extracts_centered_content`, `failure_text_in_range_spans_multiple_rows`, `failure_text_in_range_returns_empty_for_pure_padding` — pin the text extraction for both Ready/Crashed and Failed agents.
- `apps/tui/src/runtime.rs::paint_selection_if_active_flips_reversed_modifier_on_selected_cells`, `paint_selection_if_active_skips_when_agent_id_does_not_match` — pin the reverse-video render.

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

**Tests:**
- `apps/tui/src/runtime.rs::no_overlay_active_returns_true_when_nothing_open`, `no_overlay_active_returns_false_when_help_open`, `no_overlay_active_returns_false_when_popup_open` — pin the gate-check helper.
- (uncovered: the runtime-level proof that the `Event::Mouse` branch returns early when `no_overlay_active(...)` is false; the spawn-modal-open variant of the gate.)

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

**Tests:**
- `apps/tui/src/runtime.rs::compute_hover_returns_url_under_pane_cell`, `compute_hover_returns_none_when_screen_cell_outside_pane`, `compute_hover_returns_none_when_no_url_under_cell` — pin the hover-detection at the cell level.
- `apps/tui/src/runtime.rs::update_hover_returns_activated_on_none_to_some`, `update_hover_returns_deactivated_on_some_to_none`, `update_hover_returns_unchanged_when_active_state_unchanged`, `apply_hover_cursor_emits_pointer_on_activated_default_on_deactivated_nothing_on_unchanged` — pin the hover-state transitions and cursor-shape emission.
- `apps/tui/src/runtime.rs::paint_hover_url_if_active_underlines_url_range_and_tints_cyan`, `paint_hover_url_if_active_skips_when_agent_id_does_not_match`, `paint_hyperlinks_post_draw_emits_osc_8_wrap_around_cell_walk`, `paint_hyperlinks_post_draw_translates_pane_offset_into_terminal_cup`, `paint_hyperlinks_post_draw_is_a_no_op_when_no_urls_present`, `paint_hyperlinks_post_draw_does_not_drop_chars_adjacent_to_url_boundaries` — pin the underline/OSC 8 render.
- `apps/tui/src/runtime.rs::url_opener_trait_supports_recording_mock_implementations`, `project_url_open_report_opened_yields_confirm`, `project_url_open_report_failed_yields_fallback_with_error`, `url_open_toast_confirm_returns_none`, `url_open_toast_fallback_with_clipboard_success_is_warning`, `url_open_toast_fallback_with_clipboard_failure_is_error` — pin the open-or-fallback-to-clipboard policy and toast wiring.
- `apps/tui/src/url_scan.rs::finds_https_url_and_returns_pane_relative_columns`, `covers_every_column_of_the_url`, `returns_none_for_columns_outside_url`, `trims_trailing_punctuation`, `url_in_parentheses_drops_the_close_paren`, `ignores_bare_scheme_with_no_authority`, `handles_multiple_schemes`, `out_of_bounds_target_returns_none`, `file_and_ftp_schemes_are_recognised`, `respects_terminator_chars_in_markdown_links`, `find_urls_in_screen_returns_every_url_across_rows`, `find_urls_in_screen_returns_multiple_urls_on_one_row`, `find_urls_in_screen_returns_empty_when_no_urls` — pin the URL detector.
- (uncovered: end-to-end test against a real OS opener and a real terminal Ctrl+click. The trait + mock seam exists; the integration is what's missing.)

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

**Tests:**
- `apps/tui/src/status_bar/mod.rs::build_segments_default_set_is_model_tokens_worktree_branch_hint` — pins the default segment set and order.
- `apps/tui/src/status_bar/mod.rs::render_segments_renders_all_when_space_is_ample` — pins the ` │ ` separator + ordered render on a wide terminal.
- `apps/tui/src/status_bar/segments.rs::model_segment_shortens_anthropic_model_ids`, `model_segment_passes_through_unknown_model_names`, `model_segment_returns_none_when_model_is_unknown`, `model_segment_strips_bracketed_context_window_suffix`, `model_segment_appends_bracketed_effort_badge_when_non_default`, `model_segment_hides_effort_badge_when_medium_default`, `model_segment_hides_effort_badge_when_field_empty_or_absent` — pin the model segment.
- `apps/tui/src/status_bar/segments.rs::worktree_segment_hides_in_main_checkout`, `worktree_segment_renders_when_cwd_differs_from_repo`, `worktree_segment_renders_when_repo_is_unknown`, `worktree_segment_returns_none_when_cwd_basename_unknown` — pin the worktree segment.
- `apps/tui/src/status_bar/segments.rs::branch_segment_renders_when_branch_is_not_default`, `branch_segment_hides_when_branch_is_in_default_list`, `branch_segment_renders_main_when_default_list_is_empty`, `branch_segment_returns_none_when_no_branch` — pin the branch segment.
- `apps/tui/src/status_bar/segments.rs::token_segment_returns_none_when_no_usage_yet`, `token_segment_with_percent_renders_tok_count_and_percentage`, `token_segment_compact_format_omits_percentage`, `token_segment_with_bar_renders_fixed_width_progress_bar`, `token_segment_color_threshold_yellow_at_200k`, `token_segment_color_threshold_orange_at_300k`, `token_segment_color_threshold_red_at_360k`, `token_segment_uses_cache_tokens_in_total_and_percentage`, `token_segment_trusts_reported_window_for_large_context_models` — pin the tokens segment.
- `apps/tui/src/status_bar/segments.rs::prefix_hint_segment_renders_help_label_when_idle`, `prefix_hint_segment_renders_nav_badge_when_awaiting_command`, `prefix_hint_segment_styles_idle_text_via_span`, `prefix_hint_segment_never_returns_none` — pin the prefix-hint segment.
- `apps/tui/src/status_bar/segments.rs::repo_segment_renders_repo_name`, `repo_segment_returns_none_when_no_repo` — pin the (opt-in) repo segment.

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

**Tests:**
- `apps/tui/src/status_bar/mod.rs::build_segments_skips_unknown_ids_and_warns` — pins the unknown-id-is-skipped failure mode.
- `apps/tui/src/status_bar/mod.rs::build_segments_handles_empty_list` — pins the empty `status_bar_segments = []` disable.
- `apps/tui/src/status_bar/mod.rs::build_segments_recognises_repo_when_user_opts_in` — pins the opt-in `repo` segment.
- `apps/tui/src/config.rs::ui_status_bar_segments_default_includes_all_five_built_ins`, `ui_status_bar_segments_round_trips_user_override`, `ui_status_bar_segments_empty_list_disables_right_side_block` — pin config parsing for the segment list.
- `apps/tui/src/config.rs::ui_default_branches_defaults_to_main_and_master`, `ui_default_branches_round_trips_user_override`, `ui_default_branches_empty_list_means_show_every_branch` — pin sub-config for the branch segment.
- `apps/tui/src/config.rs::ui_segments_tokens_defaults_match_aifx_thresholds`, `ui_segments_tokens_round_trips_user_overrides`, `ui_segments_tokens_format_accepts_all_three_variants`, `ui_segments_tokens_section_is_optional`, `ui_segments_section_is_optional_and_defaults_apply` — pin sub-config for the tokens segment.

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

**Tests:**
- `apps/tui/src/status_bar/mod.rs::render_segments_drops_leftmost_first_when_space_is_tight` — pins the leftmost-first drop policy.
- `apps/tui/src/status_bar/mod.rs::render_segments_keeps_only_the_rightmost_when_space_is_very_tight` — pins that `prefix_hint` is preserved last.
- `apps/tui/src/status_bar/mod.rs::render_segments_returns_empty_when_nothing_fits`, `render_segments_returns_empty_for_empty_segment_list`, `render_segments_skips_segments_that_render_none`, `render_segments_returns_empty_when_all_segments_yield_none` — pin the edge cases.

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

**Tests:**
- `apps/tui/src/runtime.rs::snapshot_help_screen` — insta snapshot of `render_help` against the default keymap on an 80×24 backend.
- `apps/tui/src/runtime.rs::prefix_question_mark_opens_help` — pins the prefix+? dispatch into OpenHelp.
- `apps/tui/src/keymap.rs::cmd_prefix_in_config_triggers_super_detection`, `cmd_action_in_config_triggers_super_detection`, `cmd_scroll_chord_triggers_super_detection`, `defaults_use_super_modifier_via_direct_binds`, `ctrl_only_overrides_do_not_trigger_super_detection` — pin the SUPER auto-detection that drives the help header's `cmd → super` normalization.
- `apps/tui/tests/pty_help.rs::help_overlay_opens_and_closes_on_chord` — boots codemux in an 80x24 PTY, sends `Ctrl+B ?`, asserts the ` codemux help ` overlay block lands on screen, then sends `Esc` and asserts the overlay goes away. Pins the chord-to-overlay dispatch and the "any key closes" return path through the real keymap.
- (uncovered: a snapshot of `render_help` against a *rebound* prefix to prove the live keymap is reflected, and a test that the help-screen modal swallows mouse events.)

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

**Tests:**
- `apps/daemon/src/supervisor.rs::snapshot_replays_screen_state_on_reattach` — pins that the first `PtyData` after `HelloAck` on a reattach contains the prior session's screen content.
- `apps/daemon/src/supervisor.rs::snapshot_drain_avoids_duplicate_replay_of_buffered_bytes` — pins the drain-before-snapshot ordering so buffered bytes don't double-paint.
- `apps/daemon/src/supervisor.rs::session_survives_client_disconnect_and_reattach` — pins the reattach-and-write end-to-end through a real `cat` PTY.
- `apps/daemon/src/session.rs::take_snapshot_primary_screen_omits_alt_screen_toggle` — pins that primary-mode snapshots skip the `?1049h` toggle.
- `apps/daemon/src/session.rs::take_snapshot_alt_screen_includes_alt_screen_toggle` — pins that alt-screen snapshots lead with `\x1b[?1049h`.
- `apps/daemon/src/supervisor.rs::handshake_version_mismatch_returns_error_frame`, `handshake_with_non_hello_first_frame_returns_handshake_missing`, `handshake_with_eof_before_hello_returns_handshake_incomplete` — pin the failure modes around handshake.
- `apps/daemon/src/supervisor.rs::inbound_resize_applies_to_master` — pins the geometry-resize-before-snapshot rule (Hello rows/cols flow into the master before the next attach).

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

**Tests:**
- `crates/codemuxd-bootstrap/src/lib.rs::spawn_remote_daemon_redirects_stdin_to_devnull_and_stderr_to_sibling_file` — pins the load-bearing stdio redirect that lets `setsid -f` actually detach without keeping the SSH pipes open. Without it, AC-043 silently regresses to "ssh hangs after disconnect."
- (uncovered: end-to-end "kill the SSH ControlMaster, observe daemon survives" — needs a real sshd. The matrix preamble flags this row as uncertain.)

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

**Tests:**
- `crates/codemuxd-bootstrap/src/lib.rs::spawn_remote_daemon_runs_kill_prelude_when_force_respawn` — pins the SIGTERM-then-SIGKILL prelude on force-respawn.
- `crates/codemuxd-bootstrap/src/lib.rs::spawn_remote_daemon_omits_kill_prelude_when_not_force_respawn` — pins the negative case.
- `crates/codemuxd-bootstrap/src/lib.rs::prepare_remote_skips_install_when_version_matches`, `prepare_remote_happy_path_on_fresh_host` — pin that the version probe drives the redeploy decision.
- `crates/codemuxd-bootstrap/src/lib.rs::bootstrap_version_is_stable_and_well_formed`, `embedded_tarball_contains_required_files`, `bootstrap_manifest_mirrors_every_workspace_dep_used_by_daemon`, `stage_tarball_writes_embedded_bytes` — pin the version + embedded source machinery.
- (uncovered: the runtime-side observation that a pre-existing `Ready` agent on the same host transitions to `Crashed` when the new spawn forces redeploy.)

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

**Tests:**
- `apps/tui/src/config.rs::empty_toml_yields_default_config` — pins that an empty file deserializes as defaults.
- `apps/tui/src/config.rs::missing_ui_section_keeps_defaults`, `spawn_section_defaults_when_absent`, `ui_segments_section_is_optional_and_defaults_apply` — pin that omitted sub-sections apply defaults.
- `apps/tui/src/config.rs::xdg_config_home_wins_when_set`, `falls_back_to_home_dot_config_on_macos_and_linux`, `empty_xdg_is_treated_as_unset`, `errors_when_neither_xdg_nor_home_is_set` — pin the lookup-path resolution and the loud-fail when neither is set.
- `apps/tui/src/config.rs::scrollback_len_defaults_to_five_thousand` — pins the scrollback default value.
- `apps/tui/src/keymap.rs::missing_config_returns_default_bindings` — pins that an empty `[bindings]` table yields the default `Bindings`.

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

**Tests:**
- `apps/tui/src/config.rs::invalid_chord_in_config_propagates_as_a_parse_error` — pins that a malformed prefix chord exits at parse time.
- `apps/tui/src/config.rs::unknown_top_level_key_is_an_error` — name is misleading; the test body actually `unwrap()`s the parse with an unknown table and asserts success, pinning the failure-mode "Unknown top-level keys are NOT errors." Test name is overdue for a rename.
- `apps/tui/src/config.rs::host_colors_unknown_name_is_an_error`, `host_colors_malformed_hex_is_an_error`, `host_colors_xterm_index_out_of_range_is_an_error` — pin invalid colors propagating as parse errors.
- `apps/tui/src/config.rs::spawn_unknown_default_mode_is_an_error`, `spawn_named_project_missing_path_is_an_error` — pin spawn-section validation.
- `apps/tui/src/keymap.rs::invalid_chord_in_config_is_an_error`, `focus_next_array_with_an_invalid_chord_is_an_error` — pin keymap validation.
- (uncovered: the runtime-level proof that the parse error fires *before* `enable_raw_mode`, leaving the terminal in its pre-launch state.)

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

**Tests:**
- `apps/tui/src/main.rs::resolve_cwd_returns_canonicalized_directory` — pins the happy path.
- `apps/tui/src/main.rs::resolve_cwd_errors_when_path_is_missing` — pins the missing-path failure with `invalid path` message.
- `apps/tui/src/main.rs::resolve_cwd_errors_when_path_is_a_file` — pins the not-a-directory failure with `is not a directory` message (canonicalized path).
- (uncovered: the `--nav <invalid>` clap-parse error and the runtime-level proof that the exit happens *before* raw mode.)
