//! Spawn-agent UI: minibuffer-style bottom prompt with structured host/path
//! zones.
//!
//! Layout when active (full-width, bottom of screen):
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────┐
//! │ ▸ /home/user/workbench/repositories/codemux              │  ← wildmenu
//! │   /home/user/workbench/repositories/codemux/apps/        │     for the
//! │   /home/user/workbench/repositories/codemux/crates/      │     focused
//! │ ──────────────────────────────────────────────────────── │     zone
//! │ spawn: @local : /home/user/workbench/repositories/co█    │  ← prompt
//! └──────────────────────────────────────────────────────────┘
//! ```
//!
//! Two zones in the prompt:
//! - **path** (default focus) — live `read_dir` scan as you type, like shell
//!   tab completion. Wildmenu shows full paths.
//! - **host** — autocompletes against `~/.ssh/config` `Host` entries
//!   (wildcards skipped). `Include` directives are followed recursively
//!   with glob and `~/` expansion, so layouts like Uber's
//!   `Include config.d/*` work out of the box. Empty host → spawns locally.
//!
//! Zone navigation:
//! - `@` typed in the path zone jumps the cursor to the host zone. The
//!   only way to cross from path to host — `Tab` in the path zone is
//!   reserved for completion (see below).
//! - `Tab` in the path zone applies the highlighted wildmenu candidate to
//!   the path field (autocomplete). No-op when nothing is selected.
//! - `Tab` in the host zone with non-empty non-"local" text commits the
//!   host (emits `PrepareHost`); with empty / "local" text it switches
//!   focus to the path zone.
//! - `@` typed in the host zone is a literal char (`user@hostname` works).
//! - `Down` / `Up` move within the wildmenu. `Enter` spawns using the
//!   highlighted candidate's value if any, otherwise the literal text.
//! - `Esc` cancels.
//!
//! ## Locked path zone (bootstrap in progress)
//!
//! When the user "commits" a remote host (Tab from host zone with text, or
//! Enter on host with empty path), the runtime starts a prepare worker and
//! locks the path zone via [`SpawnMinibuffer::lock_for_bootstrap`]. While
//! locked, the path zone renders as a status row (spinner + stage label)
//! and the only accepted keys are Cancel / `SwapToHost` — both produce a
//! [`ModalOutcome::CancelBootstrap`] so the runtime can drop the worker
//! and unlock back to the host zone.
//!
//! Once the prepare phase completes, the runtime calls
//! [`SpawnMinibuffer::unlock_for_remote_path`] which switches the
//! `path_mode` to [`PathMode::Remote`] and seeds the cursor at the remote
//! `$HOME`. From there the user picks a remote folder and Enter triggers
//! the attach phase.
//!
//! Path-mode and bootstrap-view are intentionally separate concerns: a
//! single struct can be in `Local`/`Remote` x `Locked`/`Unlocked`. Locked +
//! Remote happens during the second lock (attach phase, after the user
//! picked a folder). The two-axis state stays a single struct because it's
//! still one *shape* of UI — the comment below about converting to an enum
//! dispatcher applies only when a second *shape* (e.g. phone view) lands.
//!
//! ## Future variants
//!
//! When a second spawn-UI variant earns its keep, convert `SpawnMinibuffer`
//! into an enum dispatcher; the compiler will guide every call site. Do NOT
//! introduce a `SpawnUi` trait until there is a real second consumer of the
//! abstraction (e.g. the future phone-view per AD-23, or third-party plugins).
//!
//! Per the architecture-guide review (NLM 2026-04-24):
//! *"the last acceptable moment to introduce the abstraction is the day you
//! actually decide to build the second variant."* Premature traits are the
//! Needless Complexity smell; premature `Box<dyn Trait>` is on the Rust Cheat
//! Sheet's "avoid" list. Stick to the concrete struct until evidence forces
//! otherwise. If the day comes, watch out for `large_enum_variant` clippy
//! lint and `Box` the heavier variant if needed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use codemuxd_bootstrap::{CommandRunner, DirEntry, Error as BootstrapError, RemoteFs, Stage};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::keymap::{ModalAction, ModalBindings};
use crate::ssh_config::load_ssh_hosts;

const WILDMENU_ROWS: u16 = 4;
const STRIP_ROWS: u16 = WILDMENU_ROWS + 1;
const MAX_COMPLETIONS: usize = 8;
/// Cap for the synchronous `read_dir` scan that runs on every keystroke in
/// the path zone. Without this guard, landing the prompt in a huge directory
/// (`/usr/lib`, `node_modules`, mailbox) would block the render loop.
const MAX_SCAN_ENTRIES: usize = 1024;
const HOST_PLACEHOLDER: &str = "local";
const PATH_PLACEHOLDER: &str = "<cwd>";

/// What the spawn UI tells the event loop after handling a key.
#[derive(Debug, Eq, PartialEq)]
pub enum ModalOutcome {
    None,
    Cancel,
    Spawn {
        host: String,
        path: String,
    },
    /// User committed a non-local host while the path zone is empty
    /// (or while focused on the host zone with text). The runtime
    /// should kick off the prepare phase and call
    /// [`SpawnMinibuffer::lock_for_bootstrap`] to lock the path zone
    /// with the in-progress status row.
    PrepareHost {
        host: String,
    },
    /// User pressed Cancel (Esc) or `SwapToHost` (`@`) while the path
    /// zone was locked for bootstrap. The runtime should drop the
    /// in-flight worker and call
    /// [`SpawnMinibuffer::unlock_back_to_host`] (focus returns to host
    /// zone with text preserved).
    CancelBootstrap,
}

/// Path-zone backing source. `Local` is today's behavior (live
/// `read_dir`). `Remote` is reached after the prepare phase completes
/// and carries the remote `$HOME` for cursor seeding plus a
/// directory-keyed cache so prefix-narrowing keystrokes don't re-shell.
///
/// The cache is owned by the modal because it's tied to the modal's
/// lifetime: closing the modal drops the cache, opening it again
/// starts fresh. The `RemoteFs` (the actual ssh `ControlMaster`
/// subprocess) lives in the runtime alongside the prepare worker, and
/// is borrowed into the modal as a `&mut DirLister` per keystroke.
#[derive(Debug)]
pub enum PathMode {
    Local,
    Remote {
        /// Remote `$HOME` returned from `prepare_remote`. Used to seed
        /// the path-zone cursor and as the default scan root when the
        /// path field is empty.
        //
        // Read by the runtime when constructing the `PreparedHost`
        // for the attach phase; the modal itself only writes it
        // during `unlock_for_remote_path`.
        #[allow(dead_code)]
        remote_home: PathBuf,
        /// Directory → entries cache. Hit rate is high because the
        /// user's typing pattern is mostly prefix-narrowing within one
        /// directory; we re-shell only when the user crosses `/`.
        /// Cleared when the modal closes.
        cache: HashMap<PathBuf, Vec<DirEntry>>,
    },
}

/// Per-keystroke I/O surface the runtime hands the modal so the path
/// zone can complete against either the local filesystem or a live
/// SSH `ControlMaster`. Borrow-only: the cache the [`Remote`] arm
/// reads/writes lives inside [`PathMode::Remote`] on the modal
/// itself, while the [`RemoteFs`] is owned by `runtime::PendingPrepare`
/// (its `Drop` tears down the ssh subprocess).
///
/// We deliberately use an enum rather than `Box<dyn DirLister>` —
/// this is constructed on every keystroke into the spawn modal, and
/// the `Box` allocation would be unnecessary churn.
///
/// [`Remote`]: DirLister::Remote
pub enum DirLister<'a> {
    Local,
    Remote {
        fs: &'a RemoteFs,
        runner: &'a dyn CommandRunner,
    },
}

/// Pure render state for the path-zone-locked-during-bootstrap UI.
/// The runtime sets this when it starts a prepare or attach worker
/// and clears it via [`SpawnMinibuffer::unlock_for_remote_path`] /
/// [`SpawnMinibuffer::unlock_back_to_host`] when the worker finishes.
#[derive(Debug)]
pub struct BootstrapView {
    /// Host whose bootstrap is in flight. Rendered in the status row
    /// so the user can confirm what they're waiting on.
    pub host: String,
    /// Most-recent stage reported by the worker, fed into the spinner
    /// label. `None` until the very first event arrives (typically
    /// within a few ms).
    pub current_stage: Option<Stage>,
    /// When the lock started. Drives the spinner phase via
    /// [`spinner_frame`].
    pub started_at: Instant,
}

/// Which zone of the structured prompt is currently accepting input.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Zone {
    #[default]
    Path,
    Host,
}

#[derive(Debug)]
pub struct SpawnMinibuffer {
    host: String,
    path: String,
    focused: Zone,
    /// SSH `Host` entries from `~/.ssh/config`. Cached at `open()` since the
    /// config file does not change mid-session. Empty if the file is missing.
    ssh_hosts: Vec<String>,
    /// Filtered candidates for whichever zone is focused. Refreshed on every
    /// keystroke and on every zone toggle.
    filtered: Vec<String>,
    selected: Option<usize>,
    /// Backing source for path-zone autocomplete. `Local` until the
    /// runtime calls [`Self::unlock_for_remote_path`] with a
    /// `PreparedHost`'s remote `$HOME`.
    path_mode: PathMode,
    /// `Some` while the path zone is locked for an in-flight bootstrap
    /// worker (prepare or attach). Render data only — the worker
    /// itself lives in the runtime.
    bootstrap_view: Option<BootstrapView>,
    /// One-shot error banner shown in the wildmenu region after a
    /// prepare failure. Set by the runtime via
    /// [`Self::unlock_back_to_host`]; cleared on the next keystroke.
    /// Mutually exclusive with `bootstrap_view`.
    ///
    /// Stored as the structured [`BootstrapError`] so the modal owns
    /// presentation — the stage-keyed head line and source-chain walk
    /// happen at render time via [`BootstrapError::user_message`].
    prepare_error: Option<BootstrapError>,
}

impl SpawnMinibuffer {
    pub fn open() -> Self {
        // Path defaults to empty. The placeholder (`PATH_PLACEHOLDER`) shows
        // the user that an empty submission means "use a sensible default":
        //   - local agent → inherit the TUI's cwd
        //   - SSH agent  → inherit the remote shell's login cwd ($HOME)
        //
        // Pre-filling with the local TUI's cwd was wrong for SSH targets:
        // the daemon validates the path with `cwd.exists()` and exits before
        // binding the socket if it doesn't (which is almost always the case
        // when sending a local laptop path to a remote host). The runtime
        // maps empty → None on both branches; see runtime.rs.
        let mut m = Self {
            host: String::new(),
            path: String::new(),
            focused: Zone::Path,
            ssh_hosts: load_ssh_hosts(),
            filtered: Vec::new(),
            selected: None,
            path_mode: PathMode::Local,
            bootstrap_view: None,
            prepare_error: None,
        };
        // Initial wildmenu lists local cwd entries — modal opens in
        // PathMode::Local so the lister doesn't matter.
        m.refresh(&mut DirLister::Local);
        m
    }

    /// Lock the path zone with a spinner + stage label while the
    /// runtime drives a prepare or attach worker. Idempotent — calling
    /// twice in a row replaces the existing view (e.g. switching from
    /// the prepare lock to the attach lock once the user picks a
    /// remote folder).
    pub fn lock_for_bootstrap(&mut self, host: String, started_at: Instant) {
        self.bootstrap_view = Some(BootstrapView {
            host,
            current_stage: None,
            started_at,
        });
    }

    /// Update the rendered stage label. Called by the runtime when
    /// the worker emits a `Stage(_)` event. No-op if the path zone
    /// is not locked (the runtime has already cancelled).
    pub fn set_bootstrap_stage(&mut self, stage: Stage) {
        if let Some(view) = self.bootstrap_view.as_mut() {
            view.current_stage = Some(stage);
        }
    }

    /// Unlock the path zone after a successful prepare. Switches the
    /// path mode to [`PathMode::Remote`] so subsequent keystrokes
    /// scan the remote filesystem via the supplied [`DirLister`];
    /// seeds the path field with the remote `$HOME` so the user can
    /// edit from there. Overwrites the host text with the runtime's
    /// canonical value so a partial-typed prefix (e.g. user typed
    /// "web", selected "devpod-web" from the wildmenu) is replaced
    /// with the resolved alias the bootstrap actually targeted.
    pub fn unlock_for_remote_path(
        &mut self,
        host: String,
        remote_home: PathBuf,
        lister: &mut DirLister<'_>,
    ) {
        self.bootstrap_view = None;
        self.host = host;
        // Seed the path zone with the remote $HOME as a trailing-slash
        // path so the wildmenu lists $HOME's entries directly. The
        // user can edit forward or backspace up to /.
        let home_str = remote_home.to_string_lossy();
        self.path = if home_str.ends_with('/') {
            home_str.into_owned()
        } else {
            format!("{home_str}/")
        };
        self.path_mode = PathMode::Remote {
            remote_home,
            cache: HashMap::new(),
        };
        self.focused = Zone::Path;
        self.refresh(lister);
    }

    /// Unlock the path zone back to the host-pick state. Used on
    /// cancel or on a prepare error. Preserves the host text so the
    /// user can edit it; resets `path_mode` to `Local` because the
    /// remote home is no longer trustworthy.
    ///
    /// Pass `Some(err)` for prepare failures (surfaces a red banner
    /// in the wildmenu, formatted via [`BootstrapError::user_message`]
    /// at render time) and `None` for cancel paths.
    pub fn unlock_back_to_host(
        &mut self,
        lister: &mut DirLister<'_>,
        error: Option<BootstrapError>,
    ) {
        self.bootstrap_view = None;
        self.path_mode = PathMode::Local;
        self.focused = Zone::Host;
        self.prepare_error = error;
        self.refresh(lister);
    }

    pub fn handle(
        &mut self,
        key: &KeyEvent,
        bindings: &ModalBindings,
        lister: &mut DirLister<'_>,
    ) -> ModalOutcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return ModalOutcome::None;
        }

        // Path zone locked by an in-flight bootstrap. Only Cancel and
        // SwapToHost are accepted; both produce CancelBootstrap so the
        // runtime can drop the worker and unlock back to the host
        // zone (with text preserved). Every other key is dropped to
        // make the lock obvious to the user.
        if self.bootstrap_view.is_some() {
            return match bindings.lookup(key) {
                Some(ModalAction::Cancel | ModalAction::SwapToHost) => {
                    ModalOutcome::CancelBootstrap
                }
                _ => ModalOutcome::None,
            };
        }

        // Read-once banner: any actionable keystroke dismisses a stale
        // prepare error so the user doesn't see a frozen banner from a
        // previous attempt while editing the host name.
        self.prepare_error = None;

        // SwapToHost (`@`) is dual-purpose: an action in the path zone
        // (jump to host) but a literal char in the host zone (so
        // `user@hostname` works). Resolve that up front so the action
        // match below never has to "fall through" — the original implicit
        // fall-through pattern was flagged in the architecture-guide
        // review (NLM 2026-04-24) as not idiomatic Rust.
        let action = bindings.lookup(key).and_then(|a| match a {
            ModalAction::SwapToHost if self.focused == Zone::Host => None,
            other => Some(other),
        });

        if let Some(action) = action {
            return match action {
                ModalAction::Cancel => ModalOutcome::Cancel,
                ModalAction::Confirm => self.confirm(),
                ModalAction::SwapField => self.swap_field_outcome(lister),
                ModalAction::SwapToHost => {
                    self.enter_host_zone(lister);
                    ModalOutcome::None
                }
                ModalAction::NextCompletion => {
                    self.move_selection_forward();
                    ModalOutcome::None
                }
                ModalAction::PrevCompletion => {
                    self.move_selection_backward();
                    ModalOutcome::None
                }
            };
        }

        match key.code {
            KeyCode::Char(c) => {
                self.current_field_mut().push(c);
                self.refresh(lister);
                ModalOutcome::None
            }
            KeyCode::Backspace => {
                self.current_field_mut().pop();
                self.refresh(lister);
                ModalOutcome::None
            }
            _ => ModalOutcome::None,
        }
    }

    /// Tab dispatch. Behavior depends on the focused zone:
    /// - **Path zone** → apply the highlighted wildmenu candidate to the
    ///   path field (autocomplete). Never crosses into the host zone —
    ///   only `@` does that. Reasoning: `@` is the mnemonic for "I'm
    ///   specifying a host"; Tab in a path field should mean "complete
    ///   what I just typed", not silently jump out of the field.
    /// - **Host zone with non-empty non-"local" text** → emit
    ///   [`ModalOutcome::PrepareHost`] so the runtime can start the
    ///   prepare worker.
    /// - **Host zone otherwise (empty or "local")** → switch focus to
    ///   the path zone. There's no host to commit, so this is just a
    ///   plain zone toggle.
    fn swap_field_outcome(&mut self, lister: &mut DirLister<'_>) -> ModalOutcome {
        match self.focused {
            Zone::Host if self.is_remote_host_committed() => ModalOutcome::PrepareHost {
                host: self.commit_resolved_host(),
            },
            Zone::Host => {
                self.focused = Zone::Path;
                self.refresh(lister);
                ModalOutcome::None
            }
            Zone::Path => {
                self.apply_path_completion(lister);
                ModalOutcome::None
            }
        }
    }

    /// Replace the path field with the highlighted wildmenu candidate
    /// and refresh so the wildmenu shows entries inside the new
    /// directory (typical when the candidate is a directory ending in
    /// `/`). No-op when nothing is selected — the user can hit Down
    /// to start cycling, or just type a prefix which auto-highlights
    /// the first match.
    fn apply_path_completion(&mut self, lister: &mut DirLister<'_>) {
        if let Some(idx) = self.selected
            && let Some(candidate) = self.filtered.get(idx).cloned()
        {
            self.path = candidate;
            self.refresh(lister);
        }
    }

    /// Whether the host field (or its highlighted candidate) names a
    /// remote SSH host. Empty / "local" / whitespace are local.
    fn is_remote_host_committed(&self) -> bool {
        let resolved = self.resolved_host();
        let trimmed = resolved.trim();
        !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case(HOST_PLACEHOLDER)
    }

    /// Host text after applying the highlighted wildmenu candidate
    /// (if any). Used by Tab/Enter logic to decide whether to commit
    /// or just toggle zones.
    fn resolved_host(&self) -> String {
        if self.focused == Zone::Host
            && let Some(c) = self.selected.and_then(|i| self.filtered.get(i))
        {
            return c.clone();
        }
        self.host.clone()
    }

    /// Resolve the host (typed text or highlighted wildmenu candidate)
    /// AND write it back to `self.host` so subsequent renders show the
    /// canonical name, not the typed prefix. The `resolved_host`
    /// reader-only sibling shares the resolution path; every commit
    /// site (Tab/Enter → `PrepareHost`, Enter → Spawn) calls this so
    /// the locked-spinner view and any failure pane stay consistent.
    ///
    /// The write is gated on `Zone::Host` because `selected` only
    /// points at host candidates while the host zone is focused.
    fn commit_resolved_host(&mut self) -> String {
        let resolved = self.resolved_host();
        if self.focused == Zone::Host {
            self.host.clone_from(&resolved);
        }
        resolved
    }

    fn confirm(&mut self) -> ModalOutcome {
        // Enter on the host zone with an empty path field commits the
        // host like Tab does — the user wants to pick a remote folder
        // next, not spawn at the remote $HOME with no further input.
        // Enter on the host zone with a non-empty path falls through
        // to Spawn (today's escape hatch for power users).
        if self.focused == Zone::Host
            && self.path.trim().is_empty()
            && self.is_remote_host_committed()
        {
            return ModalOutcome::PrepareHost {
                host: self.commit_resolved_host(),
            };
        }

        // Apply highlighted wildmenu candidate to the focused field if any —
        // this lets the user arrow-down + Enter without an extra Tab step.
        // Commit the host write first (when focused on Host) so the
        // post-Spawn renders and any failure-pane text show the
        // resolved host instead of the partial typed prefix.
        self.commit_resolved_host();
        let (host, path) = self.resolved_values();
        let host = if host.trim().is_empty() {
            "local".into()
        } else {
            host.trim().to_string()
        };
        // Empty path is meaningful — it tells the runtime "use the
        // appropriate default for this transport" (local cwd for local
        // spawns, remote $HOME for SSH spawns). We deliberately don't
        // fall back to local cwd here: doing so would defeat the
        // empty-path → remote-default mapping that makes SSH spawns
        // work without the user typing a remote path explicitly.
        let path = path.trim().to_string();
        ModalOutcome::Spawn { host, path }
    }

    /// Resolve `(host, path)` honoring the highlighted wildmenu item for the
    /// focused zone. Used both by `confirm` and by tests.
    fn resolved_values(&self) -> (String, String) {
        let chosen = self.selected.and_then(|i| self.filtered.get(i)).cloned();
        match (self.focused, chosen) {
            (Zone::Host, Some(h)) => (h, self.path.clone()),
            (Zone::Path, Some(p)) => (self.host.clone(), p),
            (_, None) => (self.host.clone(), self.path.clone()),
        }
    }

    /// Enter the host zone without preselecting a host. Empty `host` keeps
    /// showing the dim `local` placeholder; the wildmenu lists every SSH
    /// entry because `host_completions("", _)` returns the full pool.
    ///
    /// Don't add prefill back: committing to a specific host before the
    /// user has expressed intent makes `@ Enter` silently spawn on
    /// whichever host happens to sort first, with the prompt showing that
    /// host while the user thinks they're just opening the picker.
    fn enter_host_zone(&mut self, lister: &mut DirLister<'_>) {
        self.focused = Zone::Host;
        self.refresh(lister);
    }

    fn current_field(&self) -> &str {
        match self.focused {
            Zone::Host => &self.host,
            Zone::Path => &self.path,
        }
    }

    fn current_field_mut(&mut self) -> &mut String {
        match self.focused {
            Zone::Host => &mut self.host,
            Zone::Path => &mut self.path,
        }
    }

    fn move_selection_forward(&mut self) {
        if self.filtered.is_empty() {
            self.selected = None;
            return;
        }
        let len = self.filtered.len();
        self.selected = Some(match self.selected {
            None => 0,
            Some(i) => (i + 1) % len,
        });
    }

    fn move_selection_backward(&mut self) {
        if self.filtered.is_empty() {
            self.selected = None;
            return;
        }
        let len = self.filtered.len();
        self.selected = Some(match self.selected {
            None | Some(0) => len - 1,
            Some(i) => i - 1,
        });
    }

    fn refresh(&mut self, lister: &mut DirLister<'_>) {
        self.filtered = match self.focused {
            Zone::Path => match &mut self.path_mode {
                PathMode::Local => path_completions(&self.path),
                PathMode::Remote { cache, .. } => match lister {
                    DirLister::Local => {
                        // Mode mismatch: modal is in Remote mode but
                        // the runtime supplied a Local lister
                        // (RemoteFs::open failed earlier or the
                        // prepare slot was dropped). Render an empty
                        // wildmenu — the user can still type a
                        // literal remote path and hit Enter.
                        Vec::new()
                    }
                    DirLister::Remote { fs, runner } => {
                        remote_path_completions(&self.path, fs, *runner, cache)
                    }
                },
            },
            Zone::Host => host_completions(&self.host, &self.ssh_hosts),
        };
        // No implicit selection when the focused field is empty: the user
        // hasn't expressed a choice, so auto-highlighting the first wildmenu
        // entry would silently commit it on Enter. Once they type a prefix,
        // the first match is highlighted as you'd expect from any
        // autocomplete.
        self.selected = if self.filtered.is_empty() || self.current_field().is_empty() {
            None
        } else {
            Some(0)
        };
    }

    /// Render the minibuffer at the bottom of `area`. The widget
    /// self-positions: it carves `STRIP_ROWS` off the bottom and renders
    /// over whatever the parent has already drawn there (typically the
    /// running claude PTY).
    ///
    /// Note: the architecture-guide review (NLM 2026-04-24) flagged this
    /// self-positioning as an Encapsulation smell ("the widget is aware of
    /// its layout context"). The general advice — caller passes a `Rect`
    /// — is right for compositional layouts but wrong for overlays like
    /// this one: the spawn UI is *defined* by where it appears relative
    /// to the active screen, the same way the help and switcher popups in
    /// `runtime.rs` are. Inverting the control would force the runtime to
    /// know each widget's layout grammar. Trade-off accepted.
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect, bindings: &ModalBindings) {
        if area.height < STRIP_ROWS + 4 {
            self.render_fallback_popup(frame, area, bindings);
            return;
        }

        let strip = Rect {
            x: area.x,
            y: area.y + area.height - STRIP_ROWS,
            width: area.width,
            height: STRIP_ROWS,
        };
        frame.render_widget(Clear, strip);

        let [wildmenu_area, prompt_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(WILDMENU_ROWS), Constraint::Length(1)])
            .areas(strip);

        frame.render_widget(
            self.wildmenu_view(wildmenu_area.width as usize),
            wildmenu_area,
        );
        frame.render_widget(self.prompt_view(bindings), prompt_area);
    }

    fn wildmenu_view(&self, width: usize) -> Paragraph<'_> {
        let block = Block::default().borders(Borders::TOP);

        // Locked-for-bootstrap takes precedence over everything: the
        // path zone is showing a status row in the prompt area, so
        // the wildmenu region renders a dim "preparing remote shell
        // on {host}…" hint as visual continuity for the lock.
        if let Some(view) = self.bootstrap_view.as_ref() {
            let msg = format!(" preparing remote shell on {}…", view.host);
            return Paragraph::new(Line::styled(
                msg,
                Style::default().add_modifier(Modifier::DIM),
            ))
            .block(block);
        }

        // Prepare-failure banner. Up to `WILDMENU_ROWS - 1` lines fit;
        // anything beyond is dropped (full detail still lands in the
        // tracing log).
        if let Some(err) = self.prepare_error.as_ref() {
            const HEAD: &str = " ✗ ";
            const CONT: &str = "   ";
            let formatted = err.user_message();
            let usable = WILDMENU_ROWS as usize - 1;
            let lines: Vec<Line> = formatted
                .lines()
                .take(usable)
                .enumerate()
                .map(|(i, line)| {
                    let prefix = if i == 0 { HEAD } else { CONT };
                    let display = clip_middle(line, width.saturating_sub(prefix.len()));
                    Line::styled(
                        format!("{prefix}{display}"),
                        Style::default().fg(Color::Red),
                    )
                })
                .collect();
            return Paragraph::new(lines).block(block);
        }

        if self.filtered.is_empty() {
            let msg = match self.focused {
                Zone::Path => match &self.path_mode {
                    PathMode::Local => " (no matches — Enter spawns at literal path)".into(),
                    PathMode::Remote { .. } => format!(
                        " (remote: {} — autocomplete pending; Enter spawns at literal path)",
                        self.bootstrap_host_label()
                    ),
                },
                Zone::Host => {
                    if self.ssh_hosts.is_empty() {
                        " (no hosts found via ~/.ssh/config + Includes — type a name; SSH lands in P1.4)".into()
                    } else {
                        " (no matching SSH host — Enter spawns on this literal name)".into()
                    }
                }
            };
            return Paragraph::new(Line::styled(
                msg,
                Style::default().add_modifier(Modifier::DIM),
            ))
            .block(block);
        }

        let usable = WILDMENU_ROWS as usize - 1; // top border eats one row
        let lines: Vec<Line> = self
            .filtered
            .iter()
            .take(usable)
            .enumerate()
            .map(|(i, c)| {
                let is_selected = Some(i) == self.selected;
                let marker = if is_selected { " ▸ " } else { "   " };
                let style = if is_selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let display = clip_middle(c, width.saturating_sub(3));
                Line::styled(format!("{marker}{display}"), style)
            })
            .collect();
        Paragraph::new(lines).block(block)
    }

    /// Best-effort host label for the "(remote: ...)" wildmenu hint.
    /// We don't carry the host inside `PathMode::Remote` (the runtime
    /// owns the canonical host string via the `RemoteFs`), so we fall
    /// back to the host field. Empty / "local" should be unreachable
    /// here because `PathMode::Remote` is set only after a successful
    /// prepare against a real host.
    fn bootstrap_host_label(&self) -> &str {
        if self.host.is_empty() {
            HOST_PLACEHOLDER
        } else {
            &self.host
        }
    }

    fn prompt_view(&self, bindings: &ModalBindings) -> Paragraph<'_> {
        let label_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let placeholder_style = Style::default().add_modifier(Modifier::DIM);
        let separator_style = placeholder_style;
        let cursor_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::SLOW_BLINK);

        let mut spans = vec![
            Span::styled("spawn: ", label_style),
            Span::styled("@", host_marker_style(self.focused == Zone::Host)),
        ];

        // Path zone is locked: render host plainly, then the spinner +
        // stage label in place of the path zone. The lock reuses the
        // host span (so the user can see what host they're targeting)
        // and replaces the entire `: <path>` segment with the status.
        if let Some(view) = self.bootstrap_view.as_ref() {
            spans.extend(zone_spans(
                false, // host can't be focused while path is locked
                &self.host,
                HOST_PLACEHOLDER,
                placeholder_style,
                cursor_style,
                false,
            ));
            spans.push(Span::styled(" : ", separator_style));
            let frame = spinner_frame(view.started_at);
            let stage_label = view.current_stage.map_or("starting…", |s| s.label());
            spans.push(Span::styled(
                format!("{frame} {stage_label}"),
                Style::default().fg(Color::Yellow),
            ));
            let hint = format!(
                "  [{} cancel · {} back to host]",
                bindings.binding_for(ModalAction::Cancel),
                bindings.binding_for(ModalAction::SwapToHost),
            );
            spans.push(Span::styled(hint, placeholder_style));
            return Paragraph::new(Line::from(spans));
        }

        spans.extend(zone_spans(
            self.focused == Zone::Host,
            &self.host,
            HOST_PLACEHOLDER,
            placeholder_style,
            cursor_style,
            false,
        ));
        spans.push(Span::styled(" : ", separator_style));
        spans.extend(zone_spans(
            self.focused == Zone::Path,
            &self.path,
            PATH_PLACEHOLDER,
            placeholder_style,
            cursor_style,
            true,
        ));

        // Hint reflects what `Tab` does in the focused zone — different
        // semantics per zone now that path-Tab is autocomplete and
        // host-Tab commits-or-switches. `@` is also only meaningful in
        // the path zone (literal char in the host zone), so it shows up
        // in the path-zone hint only.
        let tab = bindings.binding_for(ModalAction::SwapField);
        let pick = bindings.binding_for(ModalAction::NextCompletion);
        let spawn = bindings.binding_for(ModalAction::Confirm);
        let cancel = bindings.binding_for(ModalAction::Cancel);
        let hint = match self.focused {
            Zone::Path => format!(
                "  [{tab} complete · {at} host · {pick} pick · {spawn} spawn · {cancel} cancel]",
                at = bindings.binding_for(ModalAction::SwapToHost),
            ),
            Zone::Host => {
                format!("  [{tab} next · {pick} pick · {spawn} spawn · {cancel} cancel]")
            }
        };
        spans.push(Span::styled(hint, placeholder_style));
        Paragraph::new(Line::from(spans))
    }

    /// Tiny terminal escape hatch: when the screen is too short for the
    /// minibuffer + wildmenu, fall back to a centered popup so the variant
    /// remains usable.
    fn render_fallback_popup(&self, frame: &mut Frame<'_>, area: Rect, bindings: &ModalBindings) {
        let popup = centered_rect_with_size(60, 8, area);
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" spawn (fallback) ");
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let [wm, p] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .areas(inner);
        frame.render_widget(self.wildmenu_view(wm.width as usize), wm);
        frame.render_widget(self.prompt_view(bindings), p);
    }
}

/// Build the spans for one zone of the prompt.
///
/// Placeholder semantics: when `value` is empty, render `placeholder` in
/// dim style. When focused, the cursor overlays the FIRST character of the
/// placeholder — like a terminal block cursor sits ON the next character
/// to be typed (vim, emacs, the standard `█` in shells), not beside it.
/// So `local` becomes `█ocal` with a cyan block where the `l` would be.
/// Real input gets the focused/non-focused value style and the cursor at
/// the end, where typing extends it.
///
/// `highlight_basename` is true for the path zone: when the value is
/// non-empty, focused, and contains a `/`, only the trailing component
/// (after the last `/`) is rendered in the focused style — the rest is
/// default. This matches how shells highlight the in-progress path
/// segment without drowning out the parent directory context. False for
/// the host zone, which has no analog (a hostname is a single segment).
///
/// Lifetime note: `value` and `placeholder` share `'a` so the returned
/// `Span`s can borrow from both. ratatui is immediate-mode and redraws on
/// every event/tick, so we deliberately avoid `to_string()` /
/// `chars().collect()` allocations on the render path.
fn zone_spans<'a>(
    focused: bool,
    value: &'a str,
    placeholder: &'a str,
    placeholder_style: Style,
    cursor_style: Style,
    highlight_basename: bool,
) -> Vec<Span<'a>> {
    const CURSOR: &str = "█";
    let mut out = Vec::with_capacity(3);
    if value.is_empty() {
        if focused {
            out.push(Span::styled(CURSOR, cursor_style));
            // Drop the first char without allocating: advance the iterator
            // and re-borrow what's left as a `&str` slice. Direct
            // `&placeholder[1..]` would panic on multi-byte first chars.
            let mut chars = placeholder.chars();
            chars.next();
            let rest = chars.as_str();
            if !rest.is_empty() {
                out.push(Span::styled(rest, placeholder_style));
            }
        } else {
            out.push(Span::styled(placeholder, placeholder_style));
        }
    } else {
        // Path basename split: when focused on the path zone and the
        // value has at least one `/`, render the parent prefix
        // (including the trailing slash) in the default style and
        // only the trailing component in the focused style. `rfind`
        // returns a byte index; `/` is a single byte in UTF-8 so
        // `slash + 1` is always a valid char boundary regardless of
        // any multi-byte chars elsewhere in the path.
        if focused
            && highlight_basename
            && let Some(slash) = value.rfind('/')
        {
            let split = slash + 1;
            let (prefix, tail) = value.split_at(split);
            out.push(Span::styled(prefix, Style::default()));
            if !tail.is_empty() {
                out.push(Span::styled(tail, value_style(true)));
            }
        } else {
            out.push(Span::styled(value, value_style(focused)));
        }
        if focused {
            out.push(Span::styled(CURSOR, cursor_style));
        }
    }
    out
}

fn value_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn host_marker_style(focused: bool) -> Style {
    let s = Style::default().fg(Color::Cyan);
    if focused {
        s.add_modifier(Modifier::BOLD)
    } else {
        s.add_modifier(Modifier::DIM)
    }
}

/// Path-zone completions: scan `read_dir(parent)` for entries matching the
/// trailing component of the typed path. Returns *full* paths (parent +
/// entry) so the wildmenu shows the user where the candidate actually
/// lands and so applying a candidate produces a complete path string.
///
/// Note: this is intentionally a separate function from
/// `host_completions`. Both produce a `Vec<String>` for the wildmenu, but
/// the underlying logic differs in three ways: the data source (live
/// filesystem vs in-memory list), the matching rule (prefix-on-basename
/// vs substring-fuzzy-on-full-name), and the post-processing (path
/// joining vs identity). The architecture-guide review (NLM 2026-04-24)
/// flagged this as Needless Repetition; on closer reading the only thing
/// they share is the wrapper signature, and a generic "filter" abstraction
/// would either swallow these differences or grow a configuration enum
/// that re-introduces the same branching one level up. Kept separate.
fn path_completions(input: &str) -> Vec<String> {
    let (dir, prefix) = split_path_for_completion(input);
    let entries = scan_dir(&dir, &prefix, MAX_COMPLETIONS);
    let dir_str = dir.to_string_lossy();
    entries
        .into_iter()
        .map(|e| {
            if dir_str == "." && !input.starts_with("./") {
                e
            } else if dir_str.ends_with('/') {
                format!("{dir_str}{e}")
            } else {
                format!("{dir_str}/{e}")
            }
        })
        .collect()
}

/// Remote path completion. Same input/output contract as
/// [`path_completions`] but the underlying directory scan goes
/// through [`RemoteFs::list_dir`] (a single `ssh -S {socket} -- ls`
/// over the live `ControlMaster`). The cache is keyed by parent
/// directory so the typical user pattern — "land at a directory, then
/// narrow with a prefix" — re-shells once and filters in-process for
/// every subsequent keystroke.
///
/// Errors from `list_dir` (network blip, permission denied, weird
/// chars in the path) degrade silently to an empty wildmenu, mirroring
/// the local [`scan_dir`] policy. A `tracing::debug!` event is emitted
/// so `RUST_LOG=codemux=debug` reveals what went wrong.
fn remote_path_completions(
    input: &str,
    fs: &RemoteFs,
    runner: &dyn CommandRunner,
    cache: &mut HashMap<PathBuf, Vec<DirEntry>>,
) -> Vec<String> {
    let (dir, prefix) = split_path_for_completion(input);
    // Cache lookup. On miss we shell out and insert. We do the
    // contains-then-insert dance instead of `entry().or_insert_with`
    // because the initializer is fallible (network errors). On
    // failure return early — empty wildmenu — rather than poisoning
    // the cache with a placeholder.
    if !cache.contains_key(&dir) {
        match fs.list_dir(runner, &dir) {
            Ok(listed) => {
                cache.insert(dir.clone(), listed);
            }
            Err(e) => {
                tracing::debug!(dir = %dir.display(), error = %e, "remote list_dir failed");
                return Vec::new();
            }
        }
    }
    let Some(entries) = cache.get(&dir) else {
        // Unreachable: we just inserted (or it was already there).
        // Bail rather than panic so a future refactor can't ship a
        // crash if this invariant breaks.
        return Vec::new();
    };
    let dir_str = dir.to_string_lossy();
    let mut out: Vec<String> = entries
        .iter()
        .filter(|e| e.name.starts_with(&prefix))
        .filter(|e| !e.name.starts_with('.') || prefix.starts_with('.'))
        .map(|e| {
            let name = if e.is_dir {
                format!("{}/", e.name)
            } else {
                e.name.clone()
            };
            if dir_str == "." && !input.starts_with("./") {
                name
            } else if dir_str.ends_with('/') {
                format!("{dir_str}{name}")
            } else {
                format!("{dir_str}/{name}")
            }
        })
        .take(MAX_COMPLETIONS)
        .collect();
    out.sort();
    out
}

/// Split a partial path into (parent dir, basename prefix) for completion.
fn split_path_for_completion(input: &str) -> (PathBuf, String) {
    if input.is_empty() {
        return (PathBuf::from("."), String::new());
    }
    let path = Path::new(input);
    if input.ends_with('/') {
        return (path.to_path_buf(), String::new());
    }
    if let Some(parent) = path.parent() {
        let dir = if parent.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            parent.to_path_buf()
        };
        let prefix = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        return (dir, prefix);
    }
    (PathBuf::from("."), input.to_string())
}

/// Scan `dir` for entries matching `prefix`, returning at most `cap` names
/// (directories with a trailing slash). Hidden entries are kept only if the
/// prefix itself starts with a dot.
///
/// I/O failures (missing directory, permission denied, non-utf8) degrade
/// silently to an empty Vec — autocomplete should never crash the TUI. A
/// `tracing::debug!` event is emitted so the operator can investigate via
/// `RUST_LOG=codemux=debug`.
fn scan_dir(dir: &Path, prefix: &str, cap: usize) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        tracing::debug!("read_dir({}) failed", dir.display());
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .take(MAX_SCAN_ENTRIES)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with(prefix) {
                return None;
            }
            if name.starts_with('.') && !prefix.starts_with('.') {
                return None;
            }
            let is_dir = e.file_type().ok()?.is_dir();
            Some(if is_dir { format!("{name}/") } else { name })
        })
        .collect();
    out.sort();
    out.truncate(cap);
    out
}

/// Host-zone completions: filter the cached SSH `Host` list against the
/// typed prefix. Returns the full pool when the input is empty (so the user
/// can browse).
fn host_completions(input: &str, hosts: &[String]) -> Vec<String> {
    let needle = input.trim().to_lowercase();
    if needle.is_empty() {
        return hosts.to_vec();
    }
    let mut scored: Vec<(usize, &String)> = hosts
        .iter()
        .filter_map(|c| score(&c.to_lowercase(), &needle).map(|s| (s, c)))
        .collect();
    scored.sort_by_key(|(s, _)| *s);
    scored.into_iter().map(|(_, c)| c.clone()).collect()
}

fn score(haystack: &str, needle: &str) -> Option<usize> {
    haystack.find(needle).map(|pos| {
        let prefix_bonus = if pos == 0 { 0 } else { 100 };
        prefix_bonus + pos + haystack.len() / 8
    })
}

fn clip_middle(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let len = s.chars().count();
    if len <= width {
        return s.to_string();
    }
    let take = width.saturating_sub(1);
    let mut out = String::from("…");
    out.extend(
        s.chars()
            .rev()
            .take(take)
            .collect::<Vec<_>>()
            .into_iter()
            .rev(),
    );
    out
}

fn centered_rect_with_size(width: u16, height: u16, r: Rect) -> Rect {
    let x = r.x + (r.width.saturating_sub(width)) / 2;
    let y = r.y + (r.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width: width.min(r.width),
        height: height.min(r.height),
    }
}

/// Single-character braille spinner frame keyed off the elapsed time
/// since `started_at`. Rotates every `SPINNER_PERIOD_MS`; the runtime
/// polls render at 20 Hz (`runtime::FRAME_POLL` = 50 ms) which means
/// every other frame steps the spinner. The start instant is owned by
/// the caller (typically [`BootstrapView::started_at`]) so concurrent
/// bootstraps each animate independently.
//
// Lives next to `BootstrapView` because it's the only consumer; if a
// second spinner site appears (e.g. an `AgentState::Failed` reload
// attempt) it should call this rather than re-deriving the frames.
fn spinner_frame(started_at: Instant) -> char {
    const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    const SPINNER_PERIOD_MS: u128 = 80;
    let frames_len = u128::try_from(FRAMES.len()).unwrap_or(1);
    let idx = usize::try_from(started_at.elapsed().as_millis() / SPINNER_PERIOD_MS % frames_len)
        .unwrap_or(0);
    FRAMES[idx]
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn b() -> ModalBindings {
        ModalBindings::default()
    }

    /// Default lister for the vast majority of tests, none of which
    /// touch the remote-completion path. The remote variant is
    /// exercised by dedicated tests further down.
    fn local() -> DirLister<'static> {
        DirLister::Local
    }

    /// Build a `BootstrapError::Bootstrap` with `VersionProbe` stage
    /// and the supplied source text. The stage maps to a known head
    /// line ("ssh probe failed …") in `BootstrapError::user_message`,
    /// which the prepare-error tests assert against.
    fn probe_err(source: &'static str) -> BootstrapError {
        BootstrapError::Bootstrap {
            stage: Stage::VersionProbe,
            source: Box::new(std::io::Error::other(source)),
        }
    }

    /// Construct a minibuffer with controlled state, bypassing `open()` so
    /// tests are deterministic regardless of the real cwd / `~/.ssh/config`.
    fn mb(host: &str, path: &str, focused: Zone, ssh_hosts: &[&str]) -> SpawnMinibuffer {
        let mut m = SpawnMinibuffer {
            host: host.into(),
            path: path.into(),
            focused,
            ssh_hosts: ssh_hosts.iter().map(|s| (*s).to_string()).collect(),
            filtered: Vec::new(),
            selected: None,
            path_mode: PathMode::Local,
            bootstrap_view: None,
            prepare_error: None,
        };
        m.refresh(&mut local());
        m
    }

    #[test]
    fn ctrl_modified_keys_are_dropped() {
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        let outcome = m.handle(&ctrl(KeyCode::Char('b')), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.path, "/tmp");
    }

    #[test]
    fn esc_returns_cancel() {
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        assert_eq!(
            m.handle(&key(KeyCode::Esc), &b(), &mut local()),
            ModalOutcome::Cancel
        );
    }

    #[test]
    fn typing_a_char_appends_to_focused_zone() {
        let mut m = mb("", "/tm", Zone::Path, &[]);
        m.handle(&key(KeyCode::Char('p')), &b(), &mut local());
        assert_eq!(m.path, "/tmp");
        assert_eq!(m.host, "");
    }

    #[test]
    fn backspace_pops_from_focused_zone() {
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        m.handle(&key(KeyCode::Backspace), &b(), &mut local());
        assert_eq!(m.path, "/tm");
    }

    #[test]
    fn at_in_path_zone_jumps_to_host() {
        // No SSH hosts → no prefill; host stays empty.
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        m.handle(&key(KeyCode::Char('@')), &b(), &mut local());
        assert_eq!(m.focused, Zone::Host);
        // The `@` was consumed, not appended to either field.
        assert_eq!(m.path, "/tmp");
        assert_eq!(m.host, "");
    }

    #[test]
    fn at_in_path_zone_enters_empty_host_with_full_wildmenu() {
        // After dropping the prefill, `@` no longer commits to a specific
        // host. The field stays empty (placeholder shows "local"), the
        // wildmenu lists every SSH host so the user can browse, and
        // `selected = None` so a stray Enter spawns local rather than
        // silently picking the first host.
        let mut m = mb("", "/tmp", Zone::Path, &["alpha", "bravo"]);
        m.handle(&key(KeyCode::Char('@')), &b(), &mut local());
        assert_eq!(m.focused, Zone::Host);
        assert_eq!(m.host, "");
        assert_eq!(m.filtered, vec!["alpha".to_string(), "bravo".to_string()]);
        assert_eq!(m.selected, None);
    }

    #[test]
    fn at_does_not_overwrite_existing_host() {
        let mut m = mb("custom", "/tmp", Zone::Path, &["alpha"]);
        m.handle(&key(KeyCode::Char('@')), &b(), &mut local());
        assert_eq!(m.focused, Zone::Host);
        assert_eq!(m.host, "custom");
    }

    #[test]
    fn typing_in_host_zone_auto_selects_first_match() {
        // Once the user has expressed a prefix, the wildmenu's first match
        // is highlighted — that's normal autocomplete UX. The "no
        // selection" rule only applies when the field is empty.
        let mut m = mb("", "/tmp", Zone::Host, &["devpod-go", "devpod-web"]);
        assert_eq!(m.selected, None);
        m.handle(&key(KeyCode::Char('d')), &b(), &mut local());
        assert_eq!(m.host, "d");
        assert_eq!(
            m.filtered,
            vec!["devpod-go".to_string(), "devpod-web".to_string()]
        );
        assert_eq!(m.selected, Some(0));
    }

    #[test]
    fn backspace_to_empty_host_clears_selection() {
        // Going back to an empty field must drop the implicit selection;
        // otherwise an empty prompt + auto-highlighted wildmenu commits on
        // Enter.
        let mut m = mb("d", "/tmp", Zone::Host, &["devpod-go"]);
        assert_eq!(m.selected, Some(0));
        m.handle(&key(KeyCode::Backspace), &b(), &mut local());
        assert_eq!(m.host, "");
        assert_eq!(m.selected, None);
    }

    #[test]
    fn enter_with_empty_host_and_no_selection_spawns_local() {
        // `@` then `Enter` — the user opened the host picker but didn't
        // pick. Should spawn local, NOT the first SSH host.
        let mut m = mb("", "/work", Zone::Path, &["devpod-go", "devpod-web"]);
        m.handle(&key(KeyCode::Char('@')), &b(), &mut local());
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "local".into(),
                path: "/work".into(),
            },
        );
    }

    #[test]
    fn tab_from_host_with_empty_text_toggles_to_path() {
        // Tab from Host with empty text is *not* a commit — there's
        // nothing to commit. It's a plain zone toggle.
        let mut m = mb("", "/tmp", Zone::Host, &[]);
        let outcome = m.handle(&key(KeyCode::Tab), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.focused, Zone::Path);
    }

    #[test]
    fn tab_from_host_with_text_emits_prepare_host() {
        // The host field has a non-empty non-"local" value: Tab
        // commits the host so the runtime can start prepare. The
        // host text is *not* erased — it stays so the runtime can
        // re-render the spinner with the host name and the user can
        // see what they're waiting on.
        let mut m = mb("custom", "/tmp", Zone::Host, &[]);
        let outcome = m.handle(&key(KeyCode::Tab), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::PrepareHost {
                host: "custom".into(),
            },
        );
        assert_eq!(m.host, "custom");
    }

    /// User types a partial host prefix that resolves to exactly one
    /// SSH alias via the wildmenu, then Tab-commits. The modal must
    /// write the *resolved* alias into `self.host` so the rendered
    /// spinner (and the post-prepare path zone) shows the alias the
    /// runtime is actually targeting, not the partial prefix the user
    /// typed. Without this write, "@web → Tab" would render as
    /// `@web : <spinner>` while bootstrap actually runs against
    /// `devpod-web`, and the post-bootstrap path zone would still
    /// show `@web`.
    #[test]
    fn tab_with_partial_match_writes_resolved_host_back() {
        let mut m = mb("", "/tmp", Zone::Path, &["devpod-go", "devpod-web"]);
        // Walk the production path: enter host zone via @, type
        // a unique prefix, Tab to commit.
        m.handle(&key(KeyCode::Char('@')), &b(), &mut local());
        m.handle(&key(KeyCode::Char('w')), &b(), &mut local());
        m.handle(&key(KeyCode::Char('e')), &b(), &mut local());
        m.handle(&key(KeyCode::Char('b')), &b(), &mut local());
        assert_eq!(
            m.selected,
            Some(0),
            "wildmenu must auto-highlight the unique match"
        );
        let outcome = m.handle(&key(KeyCode::Tab), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::PrepareHost {
                host: "devpod-web".into(),
            },
        );
        assert_eq!(
            m.host, "devpod-web",
            "self.host must reflect the resolved candidate"
        );
    }

    /// Same write-back as the Tab path, exercised via Enter on the
    /// host zone with an empty path field (the "I want to pick a
    /// remote folder next" gesture).
    #[test]
    fn enter_with_partial_match_writes_resolved_host_back() {
        let mut m = mb("", "", Zone::Path, &["devpod-go", "devpod-web"]);
        m.handle(&key(KeyCode::Char('@')), &b(), &mut local());
        m.handle(&key(KeyCode::Char('w')), &b(), &mut local());
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::PrepareHost {
                host: "devpod-web".into(),
            },
        );
        assert_eq!(m.host, "devpod-web");
    }

    /// User in path zone types a path then Enter → Spawn carries the
    /// resolved host, NOT the partial prefix. Mirrors the failure
    /// mode where a typo in the host zone bled into the eventual
    /// attach call (and the failure-pane label).
    #[test]
    fn enter_in_path_after_partial_host_spawns_with_resolved_host() {
        // Set up a state matching post-prepare: user typed "web",
        // committed via Tab (which wrote "devpod-web" back to
        // self.host), now is in path zone with a path typed.
        let mut m = mb(
            "devpod-web",
            "/srv",
            Zone::Path,
            &["devpod-go", "devpod-web"],
        );
        m.filtered = vec![];
        m.selected = None;
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "devpod-web".into(),
                path: "/srv".into(),
            },
        );
        assert_eq!(m.host, "devpod-web");
    }

    #[test]
    fn tab_from_host_with_local_value_just_toggles() {
        // The literal "local" sentinel is the local-spawn marker;
        // there's no remote to prepare. Tab toggles zones as before.
        let mut m = mb("local", "/tmp", Zone::Host, &[]);
        let outcome = m.handle(&key(KeyCode::Tab), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.focused, Zone::Path);
    }

    #[test]
    fn at_in_host_zone_is_a_literal_char() {
        // Important for `user@hostname` SSH targets.
        let mut m = mb("", "", Zone::Host, &[]);
        m.handle(&key(KeyCode::Char('@')), &b(), &mut local());
        assert_eq!(m.focused, Zone::Host);
        assert_eq!(m.host, "@");
    }

    /// Tab in the path zone is autocomplete: it replaces the path
    /// field with the highlighted wildmenu candidate and stays in the
    /// path zone. This is the main UX change — Tab no longer crosses
    /// into the host zone (only `@` does).
    #[test]
    fn tab_in_path_zone_applies_highlighted_candidate() {
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        m.filtered = vec!["/tmp/alpha".into(), "/tmp/beta".into()];
        m.selected = Some(1);
        let outcome = m.handle(&key(KeyCode::Tab), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.focused, Zone::Path, "must stay in the path zone");
        assert_eq!(
            m.path, "/tmp/beta",
            "field must reflect the picked candidate"
        );
    }

    /// Tab in the path zone with no selected candidate is a no-op:
    /// path text and focus are unchanged. The user can hit Down to
    /// start cycling, or just type a prefix which auto-highlights the
    /// first match.
    #[test]
    fn tab_in_path_zone_with_no_selection_is_noop() {
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        m.filtered = vec!["/tmp/alpha".into()];
        m.selected = None;
        let outcome = m.handle(&key(KeyCode::Tab), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.focused, Zone::Path);
        assert_eq!(m.path, "/tmp", "field must be unchanged");
    }

    #[test]
    fn empty_host_becomes_local_on_spawn() {
        let mut m = mb("", "/x", Zone::Path, &[]);
        // No wildmenu match → resolved values are the literal fields.
        m.filtered = vec![];
        m.selected = None;
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "local".into(),
                path: "/x".into()
            },
        );
    }

    /// Empty path stays empty on spawn — the runtime maps empty → None so
    /// the daemon's `--cwd` flag is omitted on the remote (SSH branch
    /// inherits `$HOME`, local branch inherits the TUI's cwd). Pre-fix,
    /// `confirm` defaulted an empty path to `std::env::current_dir()`,
    /// which sent the local laptop path verbatim to the remote daemon and
    /// tripped its `cwd.exists()` validation — the user-visible "EOF
    /// before `HelloAck`" failure mode for SSH spawns.
    #[test]
    fn empty_path_stays_empty_on_spawn() {
        let mut m = mb("devpod-go", "", Zone::Path, &["devpod-go"]);
        m.filtered = vec![];
        m.selected = None;
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "devpod-go".into(),
                path: String::new(),
            },
        );
    }

    #[test]
    fn enter_uses_highlighted_path_candidate() {
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        m.filtered = vec!["/tmp/alpha".into(), "/tmp/beta".into()];
        m.selected = Some(1);
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "local".into(),
                path: "/tmp/beta".into()
            },
        );
    }

    #[test]
    fn enter_uses_highlighted_host_candidate() {
        let mut m = mb("dev", "/work", Zone::Host, &["devpod-1", "devpod-2"]);
        // refresh() filtered "dev" against the seed.
        m.selected = Some(1);
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "devpod-2".into(),
                path: "/work".into()
            },
        );
    }

    #[test]
    fn host_field_with_text_overrides_local_default() {
        let mut m = mb("custom-host", "/work", Zone::Path, &[]);
        m.filtered = vec![];
        m.selected = None;
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "custom-host".into(),
                path: "/work".into()
            },
        );
    }

    #[test]
    fn down_cycles_with_wrap() {
        let mut m = mb("", "", Zone::Host, &["a", "b", "c"]);
        // Empty field → no implicit selection. First Down advances from
        // None to Some(0), then it cycles normally with wrap-around.
        assert_eq!(m.selected, None);
        m.handle(&key(KeyCode::Down), &b(), &mut local());
        assert_eq!(m.selected, Some(0));
        m.handle(&key(KeyCode::Down), &b(), &mut local());
        assert_eq!(m.selected, Some(1));
        m.handle(&key(KeyCode::Down), &b(), &mut local());
        m.handle(&key(KeyCode::Down), &b(), &mut local());
        assert_eq!(m.selected, Some(0));
    }

    #[test]
    fn at_into_host_zone_refreshes_wildmenu_to_host_pool() {
        // The path zone has its own (empty here) wildmenu of path
        // candidates. Pressing `@` jumps focus to the host zone, which
        // re-runs the filter against the SSH pool. Note: `@` clears any
        // host text the user previously typed (per `enter_host_zone`'s
        // semantics — empty-by-default), so the wildmenu shows the full
        // pool.
        let mut m = mb("dev", "/tmp", Zone::Path, &["devpod-1", "devpod-2"]);
        m.handle(&key(KeyCode::Char('@')), &b(), &mut local());
        assert_eq!(m.focused, Zone::Host);
        // Existing host text is preserved by `@` (covered by
        // `at_does_not_overwrite_existing_host`); narrows the pool.
        assert_eq!(
            m.filtered,
            vec!["devpod-1".to_string(), "devpod-2".to_string()]
        );
    }

    #[test]
    fn typed_prefix_with_no_matches_keeps_selection_unset_and_spawns_literal() {
        // User types "unknown-host" — zero SSH matches. selected must stay
        // None so Enter resolves to the literal typed string, not to some
        // fallback host.
        let mut m = mb("", "/work", Zone::Host, &["devpod-go", "devpod-web"]);
        for c in "unknown-host".chars() {
            m.handle(&key(KeyCode::Char(c)), &b(), &mut local());
        }
        assert_eq!(m.host, "unknown-host");
        assert!(m.filtered.is_empty());
        assert_eq!(m.selected, None);
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "unknown-host".into(),
                path: "/work".into(),
            },
        );
    }

    #[test]
    fn at_back_to_host_preserves_prefix_and_re_highlights_first_match() {
        // `@` in the path zone preserves any host text the user typed
        // earlier and re-narrows the wildmenu against it, re-highlighting
        // the first match. (Tab from a non-empty host zone commits via
        // PrepareHost — covered by a separate test. This one drives the
        // Path→Host direction, which after the Tab-UX change is the only
        // job of `@`.)
        let mut m = mb("dev", "/tmp", Zone::Path, &["devpod-go", "devpod-web"]);
        m.handle(&key(KeyCode::Char('@')), &b(), &mut local());
        assert_eq!(m.focused, Zone::Host);
        assert_eq!(m.host, "dev");
        assert_eq!(
            m.filtered,
            vec!["devpod-go".to_string(), "devpod-web".to_string()],
        );
        assert_eq!(m.selected, Some(0));
    }

    #[test]
    fn host_completions_returns_full_pool_for_empty_input() {
        let pool = vec!["a".into(), "b".into()];
        assert_eq!(host_completions("", &pool), pool);
    }

    #[test]
    fn host_completions_filters_by_substring() {
        let pool = vec!["devpod-1".into(), "devpod-2".into(), "laptop".into()];
        assert_eq!(
            host_completions("dev", &pool),
            vec!["devpod-1".to_string(), "devpod-2".to_string()],
        );
    }

    #[test]
    fn host_completions_prefers_prefix_matches() {
        let pool = vec!["xxprodxx".into(), "prod-east".into()];
        let got = host_completions("prod", &pool);
        assert_eq!(got[0], "prod-east");
    }

    #[test]
    fn split_empty_path_uses_dot_and_empty_prefix() {
        let (dir, prefix) = split_path_for_completion("");
        assert_eq!(dir, PathBuf::from("."));
        assert_eq!(prefix, "");
    }

    #[test]
    fn split_trailing_slash_keeps_full_path() {
        let (dir, prefix) = split_path_for_completion("/foo/bar/");
        assert_eq!(dir, PathBuf::from("/foo/bar/"));
        assert_eq!(prefix, "");
    }

    #[test]
    fn split_path_separates_dir_and_basename() {
        let (dir, prefix) = split_path_for_completion("/foo/bar/baz");
        assert_eq!(dir, PathBuf::from("/foo/bar"));
        assert_eq!(prefix, "baz");
    }

    #[test]
    fn clip_middle_passes_short_strings_through() {
        assert_eq!(clip_middle("hello", 10), "hello");
    }

    #[test]
    fn clip_middle_keeps_tail_with_ellipsis_when_too_long() {
        let out = clip_middle("/very/long/path/foo", 10);
        assert!(out.starts_with('…'));
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with("foo"));
    }

    #[test]
    fn clip_middle_returns_empty_when_width_is_zero() {
        assert_eq!(clip_middle("anything", 0), "");
    }

    #[test]
    fn move_selection_clears_selection_when_filtered_is_empty() {
        let mut m = mb("", "/tmp", Zone::Host, &[]);
        m.filtered.clear();
        m.selected = Some(5);
        m.move_selection_forward();
        assert_eq!(m.selected, None);
    }

    #[test]
    fn move_selection_from_none_with_backward_jumps_to_last() {
        let mut m = mb("", "", Zone::Host, &["a", "b", "c"]);
        m.selected = None;
        m.move_selection_backward();
        assert_eq!(m.selected, Some(2));
    }

    #[test]
    fn centered_rect_with_size_centers_within_parent() {
        let parent = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 50,
        };
        let r = centered_rect_with_size(40, 20, parent);
        assert_eq!(r.x, 30);
        assert_eq!(r.y, 15);
        assert_eq!(r.width, 40);
        assert_eq!(r.height, 20);
    }

    #[test]
    fn centered_rect_with_size_clamps_to_parent_when_oversized() {
        let parent = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        };
        let r = centered_rect_with_size(50, 30, parent);
        assert_eq!(r.width, 20);
        assert_eq!(r.height, 10);
    }

    #[test]
    fn value_style_focused_is_cyan_bold() {
        let s = value_style(true);
        assert_eq!(s.fg, Some(Color::Cyan));
        assert!(s.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn value_style_unfocused_is_default() {
        assert_eq!(value_style(false), Style::default());
    }

    /// Empty + focused: cursor overlays the FIRST char of the placeholder
    /// (`local` → `█ocal`), and the remainder renders dim. This is the bug
    /// fix from the user report — previously the placeholder rendered in
    /// cyan/bold (focused style) with the cursor at the end, making
    /// "local" look like real typed input.
    #[test]
    fn zone_spans_empty_focused_overlays_cursor_on_first_placeholder_char() {
        let placeholder_style = Style::default().add_modifier(Modifier::DIM);
        let cursor_style = Style::default().fg(Color::Cyan);
        let spans = zone_spans(true, "", "local", placeholder_style, cursor_style, false);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "█");
        // First char of "local" is consumed by the cursor; "ocal" remains.
        assert_eq!(spans[1].content, "ocal");
        assert!(spans[1].style.add_modifier.contains(Modifier::DIM));
        // Critically, the remainder is NOT rendered in the focused
        // (cyan + bold) style.
        assert!(!spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_ne!(spans[1].style.fg, Some(Color::Cyan));
    }

    /// A 1-char placeholder is fully consumed by the cursor; no remainder
    /// span is emitted (otherwise we'd push an empty `Span` and ratatui
    /// would still allocate a row cell for it).
    #[test]
    fn zone_spans_empty_focused_with_single_char_placeholder_omits_remainder() {
        let spans = zone_spans(true, "", "x", Style::default(), Style::default(), false);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "█");
    }

    #[test]
    fn zone_spans_empty_unfocused_renders_full_placeholder_no_cursor() {
        let placeholder_style = Style::default().add_modifier(Modifier::DIM);
        let cursor_style = Style::default();
        let spans = zone_spans(false, "", "local", placeholder_style, cursor_style, false);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "local");
    }

    #[test]
    fn zone_spans_non_empty_focused_puts_cursor_after_value() {
        let placeholder_style = Style::default();
        let cursor_style = Style::default().fg(Color::Cyan);
        let spans = zone_spans(
            true,
            "alpha",
            "local",
            placeholder_style,
            cursor_style,
            false,
        );
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "alpha");
        assert_eq!(spans[1].content, "█");
    }

    #[test]
    fn zone_spans_non_empty_unfocused_omits_cursor() {
        let spans = zone_spans(
            false,
            "alpha",
            "local",
            Style::default(),
            Style::default(),
            false,
        );
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "alpha");
    }

    /// Focused path with multiple segments: the parent prefix
    /// (everything up to and including the last `/`) renders in
    /// default style, the trailing component renders in the focused
    /// style. This is the path-zone UX so the user can see the
    /// in-progress segment without losing the parent for context.
    #[test]
    fn zone_spans_focused_path_highlights_only_basename() {
        let spans = zone_spans(
            true,
            "/home/df/repos",
            "<cwd>",
            Style::default(),
            Style::default(),
            true,
        );
        // prefix + tail + cursor = 3 spans
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "/home/df/");
        // Prefix is plain default — no fg, no BOLD.
        assert_eq!(spans[0].style, Style::default());
        assert_eq!(spans[1].content, "repos");
        // Tail uses the focused value style (cyan + bold).
        assert_eq!(spans[1].style.fg, Some(Color::Cyan));
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[2].content, "█");
    }

    /// A path that ends in `/` (e.g. just-seeded remote $HOME) has no
    /// trailing component. Render the prefix in default style, no
    /// tail span, then the cursor.
    #[test]
    fn zone_spans_focused_path_with_trailing_slash_emits_no_tail_span() {
        let spans = zone_spans(
            true,
            "/home/df/",
            "<cwd>",
            Style::default(),
            Style::default(),
            true,
        );
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "/home/df/");
        assert_eq!(spans[0].style, Style::default());
        assert_eq!(spans[1].content, "█");
    }

    /// A path with no `/` (relative single segment) falls through to
    /// the legacy whole-value highlight — there's nothing to split on.
    #[test]
    fn zone_spans_focused_path_without_slash_highlights_whole_value() {
        let spans = zone_spans(
            true,
            "repos",
            "<cwd>",
            Style::default(),
            Style::default(),
            true,
        );
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "repos");
        assert_eq!(spans[0].style.fg, Some(Color::Cyan));
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[1].content, "█");
    }

    /// Unfocused path: no highlight regardless of `highlight_basename`.
    /// (This is the baseline we render when the host zone is focused.)
    #[test]
    fn zone_spans_unfocused_path_skips_basename_highlight() {
        let spans = zone_spans(
            false,
            "/home/df/repos",
            "<cwd>",
            Style::default(),
            Style::default(),
            true,
        );
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "/home/df/repos");
        assert_eq!(spans[0].style, Style::default());
    }

    // -- Step 4: lock_for_bootstrap / unlock_* setters and the
    // resulting outcomes. Runtime wiring lands in Step 6; these tests
    // pin the pure modal-side behavior so the runtime can drive it
    // without surprises.

    #[test]
    fn enter_on_host_with_empty_path_and_remote_host_emits_prepare_host() {
        // Per the new flow: confirming the host zone with an empty
        // path field is a "I want to pick a remote folder" gesture.
        // Spawn would have to pick a default cwd and the user can't
        // see what — so commit the host instead.
        let mut m = mb("devpod-go", "", Zone::Host, &["devpod-go"]);
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::PrepareHost {
                host: "devpod-go".into(),
            },
        );
    }

    #[test]
    fn enter_on_host_with_empty_path_and_local_host_still_spawns_local() {
        // Empty host + empty path is the local-spawn gesture from
        // the moment the modal opens. No prepare to run.
        let mut m = mb("", "", Zone::Host, &[]);
        m.filtered = vec![];
        m.selected = None;
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "local".into(),
                path: String::new(),
            },
        );
    }

    #[test]
    fn enter_on_host_with_path_typed_still_spawns_directly() {
        // Escape hatch for power users who already know the remote
        // path: Enter from the Host zone with a non-empty path
        // skips the remote folder picker and spawns straight away.
        let mut m = mb("devpod-go", "/srv", Zone::Host, &["devpod-go"]);
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "devpod-go".into(),
                path: "/srv".into(),
            },
        );
    }

    #[test]
    fn lock_for_bootstrap_drops_typing_keys() {
        // Once the runtime has locked the path zone, the user can't
        // type into it — only cancel.
        let mut m = mb("devpod-go", "", Zone::Host, &["devpod-go"]);
        m.lock_for_bootstrap("devpod-go".into(), Instant::now());

        let outcome = m.handle(&key(KeyCode::Char('x')), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.host, "devpod-go");
        assert_eq!(m.path, "");

        let outcome = m.handle(&key(KeyCode::Tab), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);

        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
    }

    #[test]
    fn lock_for_bootstrap_with_esc_emits_cancel_bootstrap() {
        let mut m = mb("devpod-go", "", Zone::Host, &["devpod-go"]);
        m.lock_for_bootstrap("devpod-go".into(), Instant::now());
        let outcome = m.handle(&key(KeyCode::Esc), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::CancelBootstrap);
    }

    #[test]
    fn lock_for_bootstrap_with_at_emits_cancel_bootstrap() {
        // `@` is the SwapToHost shortcut elsewhere; while locked,
        // it's repurposed to "cancel and let me re-edit the host".
        let mut m = mb("devpod-go", "", Zone::Host, &["devpod-go"]);
        m.lock_for_bootstrap("devpod-go".into(), Instant::now());
        let outcome = m.handle(&key(KeyCode::Char('@')), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::CancelBootstrap);
    }

    #[test]
    fn set_bootstrap_stage_updates_view_when_locked() {
        let mut m = mb("devpod-go", "", Zone::Host, &["devpod-go"]);
        m.lock_for_bootstrap("devpod-go".into(), Instant::now());
        m.set_bootstrap_stage(Stage::RemoteBuild);
        let view = m.bootstrap_view.as_ref().unwrap();
        assert_eq!(view.current_stage, Some(Stage::RemoteBuild));
    }

    #[test]
    fn set_bootstrap_stage_is_a_noop_when_not_locked() {
        // The runtime might race a Stage event against a cancel that
        // already cleared the view. Don't panic, just drop it.
        let mut m = mb("devpod-go", "", Zone::Host, &["devpod-go"]);
        m.set_bootstrap_stage(Stage::RemoteBuild);
        assert!(m.bootstrap_view.is_none());
    }

    #[test]
    fn unlock_back_to_host_preserves_host_text_and_clears_view() {
        let mut m = mb("devpod-go", "", Zone::Host, &["devpod-go"]);
        m.lock_for_bootstrap("devpod-go".into(), Instant::now());
        m.unlock_back_to_host(&mut local(), None);
        assert!(m.bootstrap_view.is_none());
        assert_eq!(m.host, "devpod-go");
        assert_eq!(m.focused, Zone::Host);
        assert!(matches!(m.path_mode, PathMode::Local));
        assert!(m.prepare_error.is_none());
    }

    /// `unlock_back_to_host` with an error stores the structured
    /// banner so the next render surfaces it in the wildmenu region.
    /// The banner is the only feedback the user gets on a prepare
    /// failure (the previous behavior was a silent re-flip back to
    /// the host zone, with the error trapped in `tracing::error!`).
    #[test]
    fn unlock_back_to_host_with_error_stores_banner() {
        let mut m = mb("foo", "", Zone::Host, &["foo"]);
        m.lock_for_bootstrap("foo".into(), Instant::now());
        m.unlock_back_to_host(&mut local(), Some(probe_err("auth refused")));
        let Some(stored) = m.prepare_error.as_ref() else {
            unreachable!("banner must be set after unlock with Some(err)")
        };
        let formatted = stored.user_message();
        assert!(
            formatted.starts_with("ssh probe failed"),
            "got {formatted:?}"
        );
        assert!(formatted.contains("auth refused"), "got {formatted:?}");
        assert_eq!(
            m.focused,
            Zone::Host,
            "user should land back on the host zone"
        );
        assert_eq!(m.host, "foo", "host text preserved so the user can edit");
    }

    /// Any actionable keystroke clears the banner — read once, gone.
    /// We use a printable Char so the keystroke is unambiguously
    /// "actionable"; Ctrl-* keys are dropped before the clear, but
    /// that's not a UX-relevant scenario (no one Ctrl-clicks an
    /// error message).
    #[test]
    fn next_keystroke_clears_prepare_error_banner() {
        let mut m = mb("foo", "", Zone::Host, &["foo"]);
        m.prepare_error = Some(probe_err("something broke"));
        m.handle(&key(KeyCode::Char('x')), &b(), &mut local());
        assert!(
            m.prepare_error.is_none(),
            "banner must clear on the first key after surfacing",
        );
    }

    /// Banner persists when the modal is in locked state and the
    /// user mashes keys that get swallowed (the typing-while-locked
    /// behavior). In practice the locked state and a banner are
    /// mutually exclusive, but if a future code path violates that
    /// the banner shouldn't get accidentally cleared by a no-op
    /// keystroke against the lock.
    #[test]
    fn keystroke_during_lock_does_not_clear_banner() {
        let mut m = mb("foo", "", Zone::Host, &["foo"]);
        m.prepare_error = Some(probe_err("stale banner"));
        m.lock_for_bootstrap("foo".into(), Instant::now());
        m.handle(&key(KeyCode::Char('x')), &b(), &mut local());
        let Some(stored) = m.prepare_error.as_ref() else {
            unreachable!("banner must persist while locked")
        };
        assert!(
            stored.user_message().contains("stale banner"),
            "got {stored:?}",
        );
    }

    #[test]
    fn unlock_for_remote_path_switches_path_mode_to_remote() {
        let mut m = mb("devpod-go", "", Zone::Host, &["devpod-go"]);
        m.lock_for_bootstrap("devpod-go".into(), Instant::now());
        m.unlock_for_remote_path("devpod-go".into(), PathBuf::from("/home/df"), &mut local());
        assert!(m.bootstrap_view.is_none());
        assert!(matches!(m.path_mode, PathMode::Remote { .. }));
        // The path zone is seeded with the remote $HOME (with a
        // trailing slash); the wildmenu is empty when the supplied
        // lister is Local — that's the runtime's "RemoteFs::open
        // failed" fallback path, exercised here for simplicity.
        assert_eq!(m.path, "/home/df/");
        assert_eq!(m.focused, Zone::Path);
    }

    #[test]
    fn unlock_for_remote_path_keeps_trailing_slash_when_already_present() {
        // Remote $HOME from the probe might already end in `/` (e.g.
        // root). Don't double-slash.
        let mut m = mb("devpod-go", "", Zone::Host, &["devpod-go"]);
        m.lock_for_bootstrap("devpod-go".into(), Instant::now());
        m.unlock_for_remote_path("devpod-go".into(), PathBuf::from("/"), &mut local());
        assert_eq!(m.path, "/");
    }

    /// Belt-and-suspenders for the partial-host write-back: even if a
    /// future code path leaves `self.host` as the typed prefix at
    /// unlock time, `unlock_for_remote_path` now overwrites with the
    /// runtime's canonical host. The runtime is the source of truth
    /// because it owns the prepare slot's host string (the modal can
    /// be re-edited mid-flight).
    #[test]
    fn unlock_for_remote_path_overwrites_host_with_runtime_value() {
        let mut m = mb("web", "", Zone::Host, &["devpod-web"]);
        m.lock_for_bootstrap("devpod-web".into(), Instant::now());
        m.unlock_for_remote_path("devpod-web".into(), PathBuf::from("/home/df"), &mut local());
        assert_eq!(m.host, "devpod-web");
    }

    // -- Step 7: remote completion + cache --
    //
    // These tests use a fake `RemoteFs` (constructed via the
    // doc-hidden `for_test` ctor) plus a scripted `CommandRunner`
    // that intercepts the `ssh -S {socket} -- ls` invocation. The
    // tests exercise the cache layer in `remote_path_completions`
    // directly; the integration with `refresh()` is implied by
    // the existing host/path-zone tests that use `local()`.

    use codemuxd_bootstrap::CommandOutput;
    use std::sync::Mutex;

    /// Records the args of every `run` call and returns scripted
    /// outputs in FIFO order. Test fails on under-script (more calls
    /// than scripted).
    struct ScriptedRunner {
        calls: Mutex<Vec<Vec<String>>>,
        responses: Mutex<Vec<std::io::Result<CommandOutput>>>,
    }

    impl ScriptedRunner {
        fn new(stdouts: Vec<&[u8]>) -> Self {
            let responses = stdouts
                .into_iter()
                .map(|s| {
                    Ok(CommandOutput {
                        status: 0,
                        stdout: s.to_vec(),
                        stderr: Vec::new(),
                    })
                })
                .collect();
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn run(&self, _program: &str, args: &[&str]) -> std::io::Result<CommandOutput> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|s| (*s).to_string()).collect());
            let mut responses = self.responses.lock().unwrap();
            assert!(
                !responses.is_empty(),
                "ScriptedRunner exhausted — more `run` calls than scripted",
            );
            responses.remove(0)
        }

        fn spawn_detached(&self, _: &str, _: &[&str]) -> std::io::Result<std::process::Child> {
            unreachable!("remote_path_completions never spawns detached subprocesses");
        }
    }

    fn fake_fs() -> RemoteFs {
        RemoteFs::for_test("devpod-go".into(), PathBuf::from("/tmp/codemux-test.sock"))
    }

    #[test]
    fn remote_completions_populates_wildmenu_from_list_dir() {
        let fs = fake_fs();
        let runner = ScriptedRunner::new(vec![b"bin/\nREADME.md\n.hidden\nsrc/\n"]);
        let mut cache = HashMap::new();
        let out = remote_path_completions("/home/df/", &fs, &runner, &mut cache);
        // Sorted; dotfile filtered (prefix is empty); is_dir gets `/`.
        assert_eq!(
            out,
            vec![
                "/home/df/README.md".to_string(),
                "/home/df/bin/".to_string(),
                "/home/df/src/".to_string(),
            ],
        );
    }

    #[test]
    fn remote_completions_filters_by_basename_prefix() {
        let fs = fake_fs();
        let runner = ScriptedRunner::new(vec![b"bin/\nREADME.md\n.hidden\nsrc/\n"]);
        let mut cache = HashMap::new();
        let out = remote_path_completions("/home/df/s", &fs, &runner, &mut cache);
        // Only the `s`-prefixed entry survives.
        assert_eq!(out, vec!["/home/df/src/".to_string()]);
    }

    #[test]
    fn remote_completions_show_dotfiles_when_prefix_starts_with_dot() {
        // The dot needs at least one trailing char; `/home/df/.` on
        // its own normalizes away (Path::file_name returns None on a
        // bare `.`), matching the local-completion behavior.
        let fs = fake_fs();
        let runner = ScriptedRunner::new(vec![b".bashrc\n.config/\nREADME.md\n"]);
        let mut cache = HashMap::new();
        let out = remote_path_completions("/home/df/.b", &fs, &runner, &mut cache);
        assert_eq!(out, vec!["/home/df/.bashrc".to_string()]);
    }

    #[test]
    fn remote_completions_caches_listing_and_filters_in_process_on_narrow() {
        // First call lists `/home/df/`, second narrows to `s` —
        // the second call must NOT reach the runner because the
        // entry list for `/home/df/` is already cached.
        let fs = fake_fs();
        let runner = ScriptedRunner::new(vec![b"bin/\nREADME.md\nsrc/\n"]);
        let mut cache = HashMap::new();

        let _ = remote_path_completions("/home/df/", &fs, &runner, &mut cache);
        assert_eq!(runner.call_count(), 1);

        let out2 = remote_path_completions("/home/df/s", &fs, &runner, &mut cache);
        assert_eq!(runner.call_count(), 1, "narrow should hit cache");
        assert_eq!(out2, vec!["/home/df/src/".to_string()]);
    }

    #[test]
    fn remote_completions_re_shells_when_user_crosses_a_slash() {
        // Listing `/home/df/`, then `/home/df/src/` should fire two
        // separate `list_dir` calls — different parent directories,
        // distinct cache keys.
        let fs = fake_fs();
        let runner = ScriptedRunner::new(vec![b"src/\n", b"main.rs\nlib.rs\n"]);
        let mut cache = HashMap::new();

        let _ = remote_path_completions("/home/df/", &fs, &runner, &mut cache);
        let out2 = remote_path_completions("/home/df/src/", &fs, &runner, &mut cache);
        assert_eq!(runner.call_count(), 2);
        assert_eq!(
            out2,
            vec![
                "/home/df/src/lib.rs".to_string(),
                "/home/df/src/main.rs".to_string(),
            ],
        );
    }

    #[test]
    fn remote_completions_returns_empty_on_list_dir_error() {
        // ScriptedRunner that errors immediately on first call.
        let fs = fake_fs();
        let runner = ScriptedRunner {
            calls: Mutex::new(Vec::new()),
            responses: Mutex::new(vec![Err(std::io::Error::other("network down"))]),
        };
        let mut cache = HashMap::new();
        let out = remote_path_completions("/home/df/", &fs, &runner, &mut cache);
        assert!(
            out.is_empty(),
            "list_dir failure must degrade to empty wildmenu"
        );
        // The cache must NOT have a stale empty entry — a future
        // retry should re-attempt the network.
        assert!(
            cache.is_empty(),
            "failed list_dir must not poison the cache"
        );
    }
}
