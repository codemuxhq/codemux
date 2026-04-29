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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use codemuxd_bootstrap::{CommandRunner, DirEntry, Error as BootstrapError, RemoteFs, Stage};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::config::{NamedProject, SearchMode};
use crate::index_worker::{IndexState, IndexedDir, ProjectKind};
use crate::keymap::{ModalAction, ModalBindings};
use crate::ssh_config::load_ssh_hosts;

const WILDMENU_ROWS: u16 = 4;
const STRIP_ROWS: u16 = WILDMENU_ROWS + 1;
const MAX_COMPLETIONS: usize = 8;
/// Cap on fuzzy-mode wildmenu candidates returned per query. The
/// rendered window only shows ~3 at once, but Down/Up cycles through
/// the full Vec — a larger cap means richer exploration without
/// re-querying. nucleo-matcher's score loop is microseconds per
/// candidate so capping at 50 has negligible cost on a 50k-entry index.
const MAX_FUZZY_RESULTS: usize = 50;
/// Score boost added to candidates that match a user-defined named
/// project (`[[spawn.projects]]`). Highest of the three because named
/// projects are explicit user intent — when the user typed `cm`
/// expecting `~/Workbench/repositories/codemux`, that should dominate
/// any auto-discovered repo.
const BOOST_NAMED: u32 = 1000;
/// Score boost added to candidates that are git repositories (have a
/// `.git` child). Strong enough to lift a fuzzy-matched repo above a
/// fuzzy-matched non-repo at similar nucleo scores, but not so high
/// that a clearly better non-repo match (e.g. exact prefix) loses.
const BOOST_GIT: u32 = 300;
/// Score boost added to candidates that contain a project marker
/// file (`Cargo.toml`, `package.json`, etc.) but not `.git`. Lower
/// than `BOOST_GIT` so a true repo always outranks a marker-only
/// directory at the same nucleo score.
const BOOST_MARKER: u32 = 150;
/// Cap for the synchronous `read_dir` scan that runs on every keystroke in
/// the path zone. Without this guard, landing the prompt in a huge directory
/// (`/usr/lib`, `node_modules`, mailbox) would block the render loop.
const MAX_SCAN_ENTRIES: usize = 1024;
/// The sentinel host string for "spawn locally" — both the modal's
/// placeholder and the runtime's routing branch reference this so the
/// UI/runtime contract has a single source of truth. See
/// `runtime.rs`'s spawn dispatch for the consumer.
pub const HOST_PLACEHOLDER: &str = "local";
const PATH_PLACEHOLDER: &str = "<cwd>";
const FUZZY_PLACEHOLDER: &str = "<find>";

/// What the spawn UI tells the event loop after handling a key.
#[derive(Debug, Eq, PartialEq)]
pub enum ModalOutcome {
    None,
    Cancel,
    Spawn {
        host: String,
        path: String,
    },
    /// User pressed Enter in the path zone without picking or typing
    /// anything (no wildmenu selection, empty fuzzy query, and the
    /// path field is either empty or still holds the auto-seeded
    /// cwd / remote `$HOME`). The runtime resolves the configured
    /// `[spawn].scratch_dir` against the local or remote `$HOME`,
    /// `mkdir -p`s it, and spawns there.
    ///
    /// Carried separately from `Spawn` so the runtime can distinguish
    /// "the user explicitly chose this path" from "the user wants the
    /// default scratch landing pad" without overloading the meaning
    /// of an empty `path` (which today maps to "use the platform
    /// default cwd").
    SpawnScratch {
        host: String,
    },
    /// User committed a non-local host while the path zone is empty
    /// (or while focused on the host zone with text). The runtime
    /// should kick off the prepare phase and call
    /// [`SpawnMinibuffer::lock_for_bootstrap`] to lock the path zone
    /// with the in-progress status row.
    PrepareHost {
        host: String,
    },
    /// User picked a named project bound to an SSH host (via the
    /// `host = "<alias>"` field on `[[spawn.projects]]`). The runtime
    /// kicks off the prepare phase for `host` exactly as it would for
    /// a plain `PrepareHost`, but stashes `path` on the prepare slot
    /// so the spawn fires automatically once prepare reports
    /// `Done(Ok)` — no second user step.
    ///
    /// Carried separately from `PrepareHost` (which always returns the
    /// modal to the user for path entry on success) so the runtime
    /// can distinguish "the user wants to pick a remote folder" from
    /// "the user already named the folder via a project alias."
    PrepareHostThenSpawn {
        host: String,
        path: String,
    },
    /// User pressed Cancel (Esc) or `SwapToHost` (`@`) while the path
    /// zone was locked for bootstrap. The runtime should drop the
    /// in-flight worker and call
    /// [`SpawnMinibuffer::unlock_back_to_host`] (focus returns to host
    /// zone with text preserved).
    CancelBootstrap,
    /// User pressed `RefreshIndex` (default `ctrl+r`) while in fuzzy
    /// mode. The runtime should cancel any in-flight index build and
    /// start a fresh one from the configured search roots. No-op for
    /// the modal beyond emitting this — the wildmenu reverts to the
    /// "indexing…" sentinel via the normal `notify_index_state` path.
    RefreshIndex,
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

/// Provenance of the `path` field's current value. See
/// [`SpawnMinibuffer::path_origin`] for the contract.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PathOrigin {
    /// Filled by the system (local cwd at modal open, or remote
    /// `$HOME` after a successful prepare). Safe for the SSH flow to
    /// clear without confirmation.
    AutoSeeded,
    /// User typed, backspaced, or applied a Tab completion. Must
    /// not be cleared without explicit user intent.
    #[default]
    UserTyped,
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
    /// Provenance of the current `path` value. `AutoSeeded` means the
    /// field was filled by the system ([`Self::open`] using the local
    /// TUI cwd, or [`Self::unlock_for_remote_path`] using the remote
    /// `$HOME`); `UserTyped` means the user touched it (typed,
    /// backspaced, or applied a Tab completion).
    ///
    /// The distinction matters because system seeds carry context
    /// that doesn't apply across host boundaries — sending the local
    /// laptop path verbatim to the remote daemon trips its
    /// `cwd.exists()` validation. The `@`-into-host transition uses
    /// this flag to clear an `AutoSeeded` value silently while
    /// preserving any `UserTyped` value (the existing power-user
    /// escape hatch where `/path` then `@host` Enter spawns directly
    /// at that path on the remote host).
    path_origin: PathOrigin,
    /// Local TUI cwd captured at [`Self::open`]. Used to (re-)seed
    /// the path zone when the user comes back from the host zone
    /// (Enter on `local`, Tab on empty/local, Esc from host) so the
    /// path zone always lands ready-to-use, mirroring the SSH flow's
    /// post-prepare reseed with the remote `$HOME`.
    cwd: PathBuf,
    /// Currently-active path-zone search engine. Diverges from
    /// [`Self::user_search_mode`] when the runtime forces `Precise` for a
    /// remote-SSH session (the indexer is local-only). Restored on the
    /// remote-to-local transition.
    search_mode: SearchMode,
    /// User's preferred search mode, sourced from `[spawn].default_mode`
    /// at `open()` time. Never overridden by remote-host logic — used as
    /// the restore target on `unlock_back_to_host` after a remote
    /// session ends.
    user_search_mode: SearchMode,
    /// Input buffer for fuzzy mode. Kept separate from `path` so
    /// toggling Fuzzy ↔ Precise preserves both engines' inputs (the
    /// path field stays as the user left it; the query field stays as
    /// the user left it). The runtime feeds this into `nucleo-matcher`
    /// via [`Self::refresh_fuzzy`] each time the runtime drains an
    /// index event or an input keystroke arrives.
    fuzzy_query: String,
    /// User-curated named projects from `[[spawn.projects]]`. Stashed
    /// at `open()` because they don't change mid-session. Scored by
    /// `nucleo-matcher` against `name` (not the full path) and
    /// boosted above any auto-discovered repository.
    named_projects: Vec<NamedProject>,
    /// Lookup table from `expand_named_project_path(np.path)` → host
    /// alias, populated at `open()` from any `named_projects` entry
    /// whose `host` is `Some` and non-empty. Used at commit time in
    /// [`Self::confirm`] to upgrade a plain `Spawn` to
    /// `PrepareHostThenSpawn` when the picked path matches an alias
    /// bound to a remote host.
    ///
    /// Keyed by *expanded* path so the lookup matches what
    /// `score_fuzzy` emits (the wildmenu shows the expanded path; the
    /// confirm path resolves the same string back).
    project_hosts: HashMap<String, String>,
}

impl SpawnMinibuffer {
    /// Open the spawn modal. In Precise mode the path zone is seeded
    /// with the local TUI's cwd (with a trailing `/` so the wildmenu
    /// lists the cwd's subfolders directly) — the seed gives the user
    /// immediate visual confirmation of where a local spawn would land.
    /// In Fuzzy mode the path zone starts empty and the wildmenu shows
    /// the indexer state; the user types a query against the
    /// session-built directory index instead.
    ///
    /// `named_projects` is the user's `[[spawn.projects]]` list (cloned
    /// at open time so the modal owns its copy for the modal's
    /// lifetime — the list is small).
    ///
    /// The seed is tracked via [`Self::path_origin`] so the SSH
    /// path (`@host` Enter / Tab) can clear it without disturbing a
    /// user-typed path.
    #[must_use]
    pub fn open(cwd: &Path, default_mode: SearchMode, named_projects: Vec<NamedProject>) -> Self {
        let project_hosts = build_project_hosts(&named_projects);
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
            path_origin: PathOrigin::UserTyped,
            cwd: cwd.to_path_buf(),
            search_mode: default_mode,
            user_search_mode: default_mode,
            fuzzy_query: String::new(),
            named_projects,
            project_hosts,
        };
        if default_mode == SearchMode::Precise {
            m.seed_path_with_cwd();
        }
        // Initial wildmenu lists the cwd's subfolders in Precise mode
        // (filtered to directories only — files are not valid spawn
        // targets). In Fuzzy mode `refresh` short-circuits via the
        // path-zone fuzzy guard at the top; the wildmenu shows the
        // index-state sentinel until the runtime calls
        // `notify_index_state` with the first batch of results.
        // Modal opens in `PathMode::Local` so the lister doesn't
        // matter; the local branch in `refresh` calls into the
        // synchronous `read_dir` path.
        m.refresh(&mut DirLister::Local);
        m
    }

    /// Set the path zone to the local cwd plus a trailing `/` so the
    /// wildmenu lists subfolders directly, and mark the field as
    /// auto-seeded (so a follow-up `@`-into-host clears it without
    /// disturbing a user-typed path).
    ///
    /// Idempotent — safe to call from any `Host → Path` transition
    /// (Enter on `local`, Tab on empty/local, Esc from host zone).
    fn seed_path_with_cwd(&mut self) {
        let cwd_str = self.cwd.to_string_lossy();
        self.path = if cwd_str.ends_with('/') {
            cwd_str.into_owned()
        } else {
            format!("{cwd_str}/")
        };
        self.path_origin = PathOrigin::AutoSeeded;
    }

    /// Switch focus to the path zone, reseeding the local cwd when the
    /// field is empty so the user always lands at a usable folder
    /// picker. Centralised so every Host→Path transition (Enter on
    /// `local`, Tab on empty/local, Esc from host) shares one
    /// reseed-and-refresh contract — the bug class this prevents is "I
    /// added a new transition and forgot to reseed."
    fn transition_to_path_zone(&mut self, lister: &mut DirLister<'_>) {
        self.focused = Zone::Path;
        // Reseed cwd only in Precise mode. In Fuzzy mode the path field
        // stays empty (the input lives in `fuzzy_query`); seeding it
        // would surprise the user with a literal-path-looking value
        // that the fuzzy engine would then ignore.
        if self.search_mode == SearchMode::Precise && self.path.is_empty() {
            self.seed_path_with_cwd();
        }
        self.refresh(lister);
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
        // The remote $HOME is a system-derived seed, not a user-typed
        // path. Mark it as auto-seeded so a follow-up `@`-into-host
        // (e.g. user changed their mind about the host) doesn't
        // preserve a stale local-irrelevant value.
        self.path_origin = PathOrigin::AutoSeeded;
        self.path_mode = PathMode::Remote {
            remote_home,
            cache: HashMap::new(),
        };
        self.focused = Zone::Path;
        // Restore the user's preferred mode rather than forcing
        // Precise. The remote fuzzy index now exists (built per-host
        // by the runtime via `start_index_remote`), so SSH spawns
        // can use the same fuzzy UX as local. If the index isn't
        // ready yet, the wildmenu falls into its "indexing… N dirs"
        // sentinel — same first-use UX as local.
        self.search_mode = self.user_search_mode;
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
        // Restore the user's preferred search mode now that we're
        // back on local. The `unlock_for_remote_path` forced Precise;
        // the user's choice was preserved in `user_search_mode`.
        self.search_mode = self.user_search_mode;
        self.refresh(lister);
    }

    pub fn handle(
        &mut self,
        key: &KeyEvent,
        bindings: &ModalBindings,
        lister: &mut DirLister<'_>,
    ) -> ModalOutcome {
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

        // Named Ctrl-keyed actions (default ToggleSearchMode = ctrl+t,
        // RefreshIndex = ctrl+r) need to escape the generic Ctrl
        // early-exit just below — that exit drops every Ctrl key the
        // generic shortcut handler doesn't recognise, including these.
        // Resolve them here and dispatch directly. Bootstrap-locked
        // state already returned above, so toggle/refresh are correctly
        // no-op'd while the path zone is locked.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match bindings.lookup(key) {
                Some(ModalAction::ToggleSearchMode) => {
                    self.toggle_search_mode(lister);
                    return ModalOutcome::None;
                }
                Some(ModalAction::RefreshIndex) => return ModalOutcome::RefreshIndex,
                _ => {}
            }
        }

        // Ctrl-modified typing shortcuts. Handled before the action
        // lookup so Ctrl-Backspace / Ctrl-W (delete word backward) and
        // Ctrl-U (delete to start) work in either zone. Every other
        // Ctrl-key is dropped to avoid clashing with terminal /
        // wrapping-shell shortcuts (Ctrl-C, Ctrl-Z, the global prefix
        // key, etc.) — those should fall through to the host shell, not
        // be silently consumed by the modal.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.handle_ctrl_shortcut(key.code, lister);
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
                ModalAction::Cancel => self.cancel(lister),
                ModalAction::Confirm => self.confirm(lister),
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
                // Ctrl-default chords are intercepted by the pre-Ctrl
                // block above. These arms cover the case where a user
                // remaps either action to a non-Ctrl chord — the
                // semantics stay the same.
                ModalAction::ToggleSearchMode => {
                    self.toggle_search_mode(lister);
                    ModalOutcome::None
                }
                ModalAction::RefreshIndex => ModalOutcome::RefreshIndex,
            };
        }

        match key.code {
            KeyCode::Char(c) => {
                // Auto-enter navigation: typing `/` or `~` from an
                // empty fuzzy query is a "I want to navigate by path"
                // gesture. Switch to Precise mode and seed the path
                // field at root (`/`) or the (local or remote) `$HOME`
                // (`~`). The user's preferred mode is preserved
                // (`user_search_mode` is not touched), so the next
                // modal open returns to Fuzzy.
                if self.focused == Zone::Path
                    && self.search_mode == SearchMode::Fuzzy
                    && self.fuzzy_query.is_empty()
                    && (c == '/' || c == '~')
                {
                    self.enter_navigation_mode_with_seed(c, lister);
                    return ModalOutcome::None;
                }
                if self.focused == Zone::Path {
                    self.path_origin = PathOrigin::UserTyped;
                }
                self.current_field_mut().push(c);
                self.refresh(lister);
                ModalOutcome::None
            }
            KeyCode::Backspace => {
                if self.focused == Zone::Path {
                    self.path_origin = PathOrigin::UserTyped;
                }
                self.current_field_mut().pop();
                self.refresh(lister);
                ModalOutcome::None
            }
            _ => ModalOutcome::None,
        }
    }

    /// Switch from Fuzzy to Precise mode with the path field seeded
    /// at root (`/`) or the user's home (`~`). Called from
    /// [`Self::handle`] when the user types `/` or `~` against an
    /// empty fuzzy query — the gesture is "drop into navigation,
    /// starting here." The user's `user_search_mode` is intentionally
    /// NOT updated: this is a one-shot escape into Precise, not a
    /// preference change.
    ///
    /// `~` expands to the local `$HOME` in `PathMode::Local` and to
    /// the remote `$HOME` (captured during prepare) in
    /// `PathMode::Remote`. If `$HOME` is unset on the local side,
    /// the field is seeded with the literal `~/` and the user can
    /// either edit forward or backspace.
    fn enter_navigation_mode_with_seed(&mut self, c: char, lister: &mut DirLister<'_>) {
        self.search_mode = SearchMode::Precise;
        self.path = match c {
            '/' => "/".to_string(),
            '~' => {
                let home: Option<PathBuf> = match &self.path_mode {
                    PathMode::Local => std::env::var_os("HOME").map(PathBuf::from),
                    PathMode::Remote { remote_home, .. } => Some(remote_home.clone()),
                };
                match home {
                    Some(h) => {
                        let s = h.to_string_lossy();
                        if s.ends_with('/') {
                            s.into_owned()
                        } else {
                            format!("{s}/")
                        }
                    }
                    None => "~/".to_string(),
                }
            }
            // Caller gates on `c == '/' || c == '~'`.
            _ => unreachable!(),
        };
        self.path_origin = PathOrigin::UserTyped;
        self.fuzzy_query.clear();
        self.filtered.clear();
        self.selected = None;
        self.refresh(lister);
    }

    /// Handle Ctrl-modified keys. Currently supports Ctrl-Backspace
    /// and Ctrl-W (delete word backward, where "word" = back to and
    /// including the preceding `/`) and Ctrl-U (clear the field).
    /// All other Ctrl-keys are dropped — the modal is text-input
    /// only, and consuming Ctrl-C / Ctrl-Z / the global prefix key
    /// would be surprising.
    ///
    /// Word-delete is most useful in the path zone (back through one
    /// path segment) but applies to whichever zone is focused so the
    /// host zone benefits from Ctrl-U too.
    fn handle_ctrl_shortcut(&mut self, code: KeyCode, lister: &mut DirLister<'_>) -> ModalOutcome {
        match code {
            KeyCode::Backspace | KeyCode::Char('w') => {
                if self.focused == Zone::Path {
                    self.path_origin = PathOrigin::UserTyped;
                }
                delete_word_backward(self.current_field_mut());
                self.refresh(lister);
                ModalOutcome::None
            }
            KeyCode::Char('u') => {
                if self.focused == Zone::Path {
                    self.path_origin = PathOrigin::UserTyped;
                }
                self.current_field_mut().clear();
                self.refresh(lister);
                ModalOutcome::None
            }
            _ => ModalOutcome::None,
        }
    }

    /// Cancel handler. Esc in the host zone is "back" — switch to the
    /// path zone and re-seed the cwd if the path field was previously
    /// cleared (the `@`-into-host clear, or the user backspaced it
    /// to nothing). Esc in the path zone closes the modal entirely.
    fn cancel(&mut self, lister: &mut DirLister<'_>) -> ModalOutcome {
        if self.focused == Zone::Host {
            self.transition_to_path_zone(lister);
            ModalOutcome::None
        } else {
            ModalOutcome::Cancel
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
                // Empty / `local` host commit: just a zone toggle.
                // Re-seed the cwd so the user has a folder picker to
                // work with — the `@`-into-host clear left the path
                // empty and an empty wildmenu would be useless here.
                self.transition_to_path_zone(lister);
                ModalOutcome::None
            }
            // In Fuzzy mode Tab is a no-op for v1 — Enter is the only
            // way to commit a fuzzy-matched directory. The zsh-style
            // "complete then descend" semantics don't carry over to
            // ranked free-text matching; a future iteration may add
            // "Tab to drill into the highlighted dir via Precise mode".
            Zone::Path if self.search_mode == SearchMode::Fuzzy => ModalOutcome::None,
            Zone::Path => {
                self.apply_path_completion(lister);
                ModalOutcome::None
            }
        }
    }

    /// Tab in the path zone applies the highlighted wildmenu
    /// candidate. The first Tab applies the basename WITHOUT the
    /// trailing `/` so the wildmenu doesn't immediately descend into
    /// the folder — matching zsh / fish autocomplete: the first Tab
    /// confirms what you picked, then you descend either by typing
    /// `/` yourself or by hitting Tab again. The second Tab is
    /// detected by `path == candidate-without-slash` and applies the
    /// candidate WITH the slash, which makes `refresh` list the
    /// folder's contents.
    ///
    /// No-op when nothing is selected — the user can hit Down to
    /// start cycling, or just type a prefix which auto-highlights
    /// the first match.
    fn apply_path_completion(&mut self, lister: &mut DirLister<'_>) {
        if let Some(idx) = self.selected
            && let Some(candidate) = self.filtered.get(idx).cloned()
        {
            let trimmed = candidate.strip_suffix('/').unwrap_or(&candidate);
            self.path = if self.path == trimmed {
                // Second Tab on the same folder → descend.
                candidate
            } else {
                // First Tab → apply without trailing slash so the
                // wildmenu stays at the same level.
                trimmed.to_string()
            };
            // Tab-applying a completion is an explicit user choice;
            // the seeded cwd is no longer the literal value in the
            // field, so the auto-seeded marker no longer applies.
            self.path_origin = PathOrigin::UserTyped;
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

    fn confirm(&mut self, lister: &mut DirLister<'_>) -> ModalOutcome {
        // Enter on the host zone with an empty path field is a "I want
        // to pick a folder next" gesture, not a "spawn now" gesture.
        // Two cases by host:
        //   - remote   → emit `PrepareHost` so the runtime can start
        //                the SSH bootstrap; after prepare succeeds the
        //                modal unlocks at the remote `$HOME` for the
        //                folder picker.
        //   - local /  → switch to the path zone with the local cwd
        //     empty       reseeded; mirrors the SSH flow visually so
        //                 the user always lands at a folder picker.
        // Enter on the host zone with a NON-empty path falls through
        // to Spawn (today's escape hatch for power users who already
        // know the remote / local path they want).
        if self.focused == Zone::Host && self.path.trim().is_empty() {
            if self.is_remote_host_committed() {
                return ModalOutcome::PrepareHost {
                    host: self.commit_resolved_host(),
                };
            }
            // Local / empty host: switch to the path picker. Clear
            // the host field so the dim `local` placeholder shows
            // (consistent with the empty-host = local convention
            // used everywhere else).
            self.host.clear();
            self.transition_to_path_zone(lister);
            return ModalOutcome::None;
        }

        // Path zone with no real choice: no wildmenu pick, no typed
        // fuzzy query, and the path field is either empty or still
        // the system-seeded default (local cwd, or remote $HOME after
        // a successful prepare). Treat that as "user just wants a
        // sandbox" and emit `SpawnScratch` so the runtime can route
        // to the configured scratch dir instead of silently spawning
        // at the platform default cwd.
        //
        // Local: `host` is empty / "local" — the runtime resolves
        // scratch against the local $HOME.
        // Remote: `host` is the SSH host the user already committed
        // via PrepareHost — the runtime resolves scratch against the
        // remote $HOME captured during prepare.
        if self.focused == Zone::Path && self.is_passive_path_enter() {
            let trimmed = self.host.trim();
            let host = if trimmed.is_empty() {
                HOST_PLACEHOLDER.to_string()
            } else {
                trimmed.to_string()
            };
            return ModalOutcome::SpawnScratch { host };
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
        // Project-host override: if the picked path matches a named
        // project bound to an SSH alias, route through the bootstrap
        // state machine instead of emitting a plain `Spawn` (which the
        // runtime would reject for any non-local host without an
        // active prepare slot — see the `runtime.rs` "modal state
        // machine bug" guard). The project alias is the more specific
        // signal, so it overrides whatever the user typed in the host
        // zone.
        if let Some(project_host) = self.project_hosts.get(&path) {
            return ModalOutcome::PrepareHostThenSpawn {
                host: project_host.clone(),
                path,
            };
        }
        ModalOutcome::Spawn { host, path }
    }

    /// True when the path zone is in its "user opened the modal and
    /// hit Enter without doing anything" state: no wildmenu pick, no
    /// typed fuzzy query, and the path field is either empty or still
    /// the system-seeded default. Used by [`Self::confirm`] to route
    /// to the scratch-dir spawn instead of the platform default cwd.
    fn is_passive_path_enter(&self) -> bool {
        self.selected.is_none()
            && self.fuzzy_query.is_empty()
            && (self.path.trim().is_empty() || self.path_origin == PathOrigin::AutoSeeded)
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
    ///
    /// Also clears the auto-seeded local cwd in the path field. The
    /// local cwd doesn't apply to remote targets, and leaving it in
    /// place would defeat the `Enter on host with empty path →
    /// PrepareHost` gesture (the user would silently spawn directly
    /// at the local laptop path on the remote host, which fails the
    /// daemon's `cwd.exists()` check). User-typed paths are preserved
    /// — the existing power-user escape hatch (`/path` then `@host`
    /// Enter spawns directly) still works.
    fn enter_host_zone(&mut self, lister: &mut DirLister<'_>) {
        self.focused = Zone::Host;
        if self.path_origin == PathOrigin::AutoSeeded {
            self.path.clear();
            self.path_origin = PathOrigin::UserTyped;
        }
        self.refresh(lister);
    }

    fn current_field(&self) -> &str {
        match self.focused {
            Zone::Host => &self.host,
            Zone::Path if self.search_mode == SearchMode::Fuzzy => &self.fuzzy_query,
            Zone::Path => &self.path,
        }
    }

    fn current_field_mut(&mut self) -> &mut String {
        match self.focused {
            Zone::Host => &mut self.host,
            Zone::Path if self.search_mode == SearchMode::Fuzzy => &mut self.fuzzy_query,
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
        // Fuzzy mode owns its own wildmenu lifecycle: results are
        // populated by `refresh_fuzzy` (driven by the runtime's index
        // drain), not by the per-keystroke read_dir below. Bail out
        // here so a Char/Backspace in fuzzy mode doesn't clobber the
        // matcher's results with an empty Vec.
        if self.search_mode == SearchMode::Fuzzy && self.focused == Zone::Path {
            return;
        }
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

    /// Toggle the path zone between Fuzzy and Precise. Updates the
    /// user's preferred mode (so a remote→local restore later picks
    /// the new choice). In Precise mode this re-seeds the cwd if the
    /// path is empty, mirroring the [`Self::open`] behavior so the
    /// wildmenu shows immediately-useful candidates instead of a
    /// blank list.
    pub fn toggle_search_mode(&mut self, lister: &mut DirLister<'_>) {
        let next = match self.search_mode {
            SearchMode::Fuzzy => SearchMode::Precise,
            SearchMode::Precise => SearchMode::Fuzzy,
        };
        self.search_mode = next;
        self.user_search_mode = next;
        if next == SearchMode::Precise && self.focused == Zone::Path && self.path.is_empty() {
            self.seed_path_with_cwd();
        }
        // Reset wildmenu state — different engine, different candidates.
        // Precise mode is repopulated by `refresh`; Fuzzy mode waits
        // for the runtime's `notify_index_state` callback.
        self.filtered.clear();
        self.selected = None;
        self.refresh(lister);
        tracing::trace!(mode = ?next, "spawn modal: search mode toggled");
    }

    /// Runtime entry point. Called once per frame after the index
    /// drain so the modal can repopulate its fuzzy wildmenu when a new
    /// `IndexEvent::Done` lands or when the user just typed into the
    /// query buffer. No-op outside Fuzzy + Path mode.
    pub fn notify_index_state(&mut self, index: Option<&IndexState>) {
        if self.search_mode != SearchMode::Fuzzy || self.focused != Zone::Path {
            return;
        }
        self.refresh_fuzzy(index);
    }

    /// Catalog key the runtime should use when looking up the index
    /// for the modal's *current* path-zone target. Returns
    /// [`HOST_PLACEHOLDER`] (`"local"`) for local mode and the SSH
    /// host name for remote mode. The runtime calls this each frame
    /// to pick the right per-host index out of [`IndexCatalog`] so a
    /// remote modal queries the SSH index, not the stale local one.
    #[must_use]
    pub fn active_host_key(&self) -> &str {
        match self.path_mode {
            PathMode::Local => HOST_PLACEHOLDER,
            PathMode::Remote { .. } => &self.host,
        }
    }

    /// Score the current `fuzzy_query` against the index and populate
    /// `filtered` with the top results. Called by `notify_index_state`;
    /// public so tests can drive it without a runtime drain loop.
    pub fn refresh_fuzzy(&mut self, index: Option<&IndexState>) {
        // Empty query: clear the wildmenu so Enter doesn't commit a
        // stale highlighted candidate from a previous query.
        if self.fuzzy_query.is_empty() {
            self.filtered.clear();
            self.selected = None;
            return;
        }
        // Both `Ready` and `Refreshing` carry usable cached results
        // — the SWR refresh just keeps a worker walking in the
        // background. Reading via `cached_dirs` keeps this branch
        // exhaustive over the four-variant enum without enumerating
        // both arms inline.
        let Some(dirs) = index.and_then(IndexState::cached_dirs) else {
            // Index not ready (Building / Failed / None). Leave
            // `filtered` empty so `wildmenu_view` falls into the
            // index-state sentinel branch.
            self.filtered.clear();
            self.selected = None;
            return;
        };
        self.filtered = score_fuzzy(&self.fuzzy_query, dirs, &self.named_projects);
        self.selected = (!self.filtered.is_empty()).then_some(0);
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
    pub fn render(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        bindings: &ModalBindings,
        index: Option<&IndexState>,
    ) {
        if area.height < STRIP_ROWS + 4 {
            self.render_fallback_popup(frame, area, bindings, index);
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
            self.wildmenu_view(wildmenu_area.width as usize, index),
            wildmenu_area,
        );
        frame.render_widget(self.prompt_view(bindings), prompt_area);
    }

    fn wildmenu_view(&self, width: usize, index: Option<&IndexState>) -> Paragraph<'_> {
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

        // Fuzzy + Path: render the indexer-state sentinel when there
        // are no current matches. Either the index is still building,
        // the user hasn't typed a query yet, the query had no matches,
        // or the build failed.
        if self.search_mode == SearchMode::Fuzzy
            && self.focused == Zone::Path
            && self.filtered.is_empty()
        {
            return self.fuzzy_state_view(width, index, block);
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
        let zone = self.focused;
        let fuzzy = self.search_mode == SearchMode::Fuzzy;
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
                // In fuzzy mode the full path *is* the signal — basename
                // alone strips the directory context that the user is
                // searching for. Precise / Host modes keep the existing
                // basename-or-literal display.
                let display_str = if fuzzy && zone == Zone::Path {
                    c.clone()
                } else {
                    wildmenu_display_text(zone, c)
                };
                let display = clip_middle(&display_str, width.saturating_sub(3));
                Line::styled(format!("{marker}{display}"), style)
            })
            .collect();
        Paragraph::new(lines).block(block)
    }

    /// Build the fuzzy-state sentinel paragraph for the wildmenu strip.
    /// Extracted from [`Self::wildmenu_view`] purely to keep that
    /// function under clippy's `too_many_lines` threshold; the only
    /// caller is the fuzzy-empty branch.
    ///
    /// Building → always show the live progress sentinel, even if the
    /// user has typed a query that didn't match the partial index.
    /// The walker is still discovering dirs; "no matches" would lie
    /// because the next batch may add the match. Once the walk
    /// completes (Ready / Refreshing) the "(no matches)" sentinel
    /// applies normally.
    fn fuzzy_state_view<'a>(
        &self,
        width: usize,
        index: Option<&IndexState>,
        block: Block<'a>,
    ) -> Paragraph<'a> {
        let dim = Style::default().add_modifier(Modifier::DIM);
        match index {
            None | Some(IndexState::Building { count: 0, .. }) => {
                Paragraph::new(Line::styled("  ⠋ indexing…".to_string(), dim)).block(block)
            }
            Some(IndexState::Building { count, .. }) => {
                Paragraph::new(Line::styled(format!("  ⠋ indexing… {count} dirs"), dim))
                    .block(block)
            }
            Some(IndexState::Ready { .. } | IndexState::Refreshing { .. })
                if self.fuzzy_query.is_empty() =>
            {
                Paragraph::new(Line::styled(
                    "  (type to search the directory index)".to_string(),
                    dim,
                ))
                .block(block)
            }
            Some(IndexState::Ready { .. } | IndexState::Refreshing { .. }) => {
                Paragraph::new(Line::styled("  (no matches)".to_string(), dim)).block(block)
            }
            Some(IndexState::Failed { message }) => {
                let display = clip_middle(message, width.saturating_sub(4));
                Paragraph::new(Line::styled(
                    format!("  ✗ {display}"),
                    Style::default().fg(Color::Red),
                ))
                .block(block)
            }
        }
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

        // In Fuzzy + Path mode the prompt label flips from "spawn:" to
        // "find:" so the user has an obvious visual cue about which
        // engine is active. Other states keep "spawn:" (the modal still
        // ultimately spawns, but the path-zone semantics differ enough
        // that a label change is justified).
        let label = if self.search_mode == SearchMode::Fuzzy && self.focused == Zone::Path {
            "find:  "
        } else {
            "spawn: "
        };
        let mut spans = vec![
            Span::styled(label, label_style),
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
                host_unfocused_value_style(),
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
            host_unfocused_value_style(),
        ));
        spans.push(Span::styled(" : ", separator_style));
        // Path zone shows whichever input buffer is active for the
        // current mode: `fuzzy_query` in Fuzzy, `path` in Precise. The
        // placeholder flips with the mode so the empty state reads
        // either `<find>` or `<cwd>` depending on what Enter would
        // commit.
        let (path_text, path_placeholder) =
            if self.search_mode == SearchMode::Fuzzy && self.focused == Zone::Path {
                (self.fuzzy_query.as_str(), FUZZY_PLACEHOLDER)
            } else {
                (self.path.as_str(), PATH_PLACEHOLDER)
            };
        spans.extend(zone_spans(
            self.focused == Zone::Path,
            path_text,
            path_placeholder,
            placeholder_style,
            cursor_style,
            true,
            path_unfocused_value_style(),
        ));

        // Hint reflects what's actionable in the focused zone. Fuzzy +
        // Path drops the Tab-complete hint (Tab is a no-op in Fuzzy)
        // and adds the toggle / refresh chords so the user can find
        // the escape hatches via the help line.
        let tab = bindings.binding_for(ModalAction::SwapField);
        let pick = bindings.binding_for(ModalAction::NextCompletion);
        let spawn = bindings.binding_for(ModalAction::Confirm);
        let cancel = bindings.binding_for(ModalAction::Cancel);
        let toggle = bindings.binding_for(ModalAction::ToggleSearchMode);
        let refresh = bindings.binding_for(ModalAction::RefreshIndex);
        let at = bindings.binding_for(ModalAction::SwapToHost);
        let hint = match (self.focused, self.search_mode) {
            (Zone::Path, SearchMode::Fuzzy) => format!(
                "  [{toggle} navigate · {refresh} rebuild · {at} host · {pick} pick · {spawn} spawn · {cancel} cancel]",
            ),
            (Zone::Path, SearchMode::Precise) => format!(
                "  [{tab} complete · {toggle} fuzzy · {at} host · {pick} pick · {spawn} spawn · {cancel} cancel]",
            ),
            (Zone::Host, _) => {
                format!("  [{tab} next · {pick} pick · {spawn} spawn · {cancel} cancel]")
            }
        };
        spans.push(Span::styled(hint, placeholder_style));
        Paragraph::new(Line::from(spans))
    }

    /// Tiny terminal escape hatch: when the screen is too short for the
    /// minibuffer + wildmenu, fall back to a centered popup so the variant
    /// remains usable.
    fn render_fallback_popup(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        bindings: &ModalBindings,
        index: Option<&IndexState>,
    ) {
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
        frame.render_widget(self.wildmenu_view(wm.width as usize, index), wm);
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
/// `unfocused_value_style` is what gets applied to a non-empty value
/// when the zone isn't focused. The path zone passes `Style::default()`
/// (the value just sits there in the terminal's default color); the
/// host zone passes a cyan style so a committed SSH host stays
/// visibly "selected" after focus moves to the path zone — without
/// this, a cyan-when-focused / default-when-not flip made the
/// unfocused host look identical to the local placeholder.
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
    unfocused_value_style: Style,
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
                out.push(Span::styled(tail, focused_value_style()));
            }
        } else {
            let style = if focused {
                focused_value_style()
            } else {
                unfocused_value_style
            };
            out.push(Span::styled(value, style));
        }
        if focused {
            out.push(Span::styled(CURSOR, cursor_style));
        }
    }
    out
}

/// Style applied to the focused zone's value text. The cyan + bold
/// combo is the modal's "this is what you're typing" cue, used by
/// both zones (host and path) and by the path-zone basename
/// highlight.
fn focused_value_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// Default unfocused value style for the path zone — terminal
/// default color, no modifiers. The host zone uses
/// [`host_unfocused_value_style`] instead so a committed SSH host
/// keeps its blue accent after focus moves on.
fn path_unfocused_value_style() -> Style {
    Style::default()
}

/// Unfocused value style for the host zone. Cyan but NOT bold so a
/// committed SSH host (e.g. `devpod-go`) stays visually distinct
/// from typed text in the path zone while not competing with the
/// focused zone's bolded cursor.
fn host_unfocused_value_style() -> Style {
    Style::default().fg(Color::Cyan)
}

fn host_marker_style(focused: bool) -> Style {
    let s = Style::default().fg(Color::Cyan);
    if focused {
        s.add_modifier(Modifier::BOLD)
    } else {
        s.add_modifier(Modifier::DIM)
    }
}

/// Rank `dirs` and `named` against `query` with `nucleo-matcher`,
/// returning up to [`MAX_FUZZY_RESULTS`] full-path strings ordered by
/// `(score + boost) desc, path asc`. The lex tiebreaker matters:
/// without it, candidates that hash to the same nucleo score swap
/// positions on every keystroke and the wildmenu visibly flickers.
///
/// Boost layering (additive on top of nucleo's score):
///   * Named project (matched against `name`): [`BOOST_NAMED`] (+1000)
///   * Indexed dir with `.git`:                [`BOOST_GIT`]   (+300)
///   * Indexed dir with project marker:        [`BOOST_MARKER`] (+150)
///   * Plain indexed dir:                      0
///
/// Named projects are matched against `NamedProject::name`, NOT path
/// — that's the alias semantic. If a named project's path also lives
/// in the indexed search roots, the named entry wins (the indexed dup
/// is filtered out by path equality).
#[must_use]
fn score_fuzzy(query: &str, dirs: &[IndexedDir], named: &[NamedProject]) -> Vec<String> {
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config, Matcher, Utf32Str};

    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();

    let mut scored: Vec<(u32, String)> = Vec::new();
    let mut named_paths: HashSet<String> = HashSet::new();

    // Named projects first — score the user-friendly `name`, but emit
    // the (tilde-expanded) `path` as the spawn target.
    for np in named {
        let haystack = Utf32Str::new(&np.name, &mut buf);
        if let Some(score) = pattern.score(haystack, &mut matcher) {
            let expanded = expand_named_project_path(&np.path);
            named_paths.insert(expanded.clone());
            scored.push((score.saturating_add(BOOST_NAMED), expanded));
        }
    }

    // Indexed dirs — score the full path. Skip any path already
    // emitted by a named project so the same dir doesn't appear twice.
    for d in dirs {
        let s = d.path.to_string_lossy();
        if named_paths.contains(s.as_ref()) {
            continue;
        }
        let haystack = Utf32Str::new(&s, &mut buf);
        if let Some(score) = pattern.score(haystack, &mut matcher) {
            let boost = match d.kind {
                ProjectKind::Git => BOOST_GIT,
                ProjectKind::Marker => BOOST_MARKER,
                ProjectKind::Plain => 0,
            };
            scored.push((score.saturating_add(boost), s.into_owned()));
        }
    }

    // Sort by (score desc, path asc). The path tiebreaker is what
    // keeps the wildmenu from flickering when two candidates tie.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(MAX_FUZZY_RESULTS)
        .map(|(_, s)| s)
        .collect()
}

/// Tilde-expand a named project's `path`. Mirrors the indexer's
/// expansion so a `path = "~/foo"` produces the same string the
/// indexer would have emitted for that dir — keeps the dedup path
/// equality stable.
#[must_use]
fn expand_named_project_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(rest).to_string_lossy().into_owned();
    }
    if path == "~"
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).to_string_lossy().into_owned();
    }
    path.to_string()
}

/// Build the path → host lookup the spawn modal uses at commit time
/// to upgrade a plain `Spawn` into `PrepareHostThenSpawn` for projects
/// bound to an SSH alias. Empty/missing `host` is filtered out (treated
/// as local). Keys are tilde-expanded so they match the strings
/// `score_fuzzy` emits into the wildmenu.
///
/// On a duplicate path with conflicting hosts the *first* entry wins —
/// the user's config order is the tiebreaker. We don't bother warning;
/// duplicate paths in `[[spawn.projects]]` are already a config smell
/// that the existing dedup in `score_fuzzy` papers over.
#[must_use]
fn build_project_hosts(projects: &[NamedProject]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for np in projects {
        let Some(host) = np.host.as_ref() else {
            continue;
        };
        if host.is_empty() {
            continue;
        }
        map.entry(expand_named_project_path(&np.path))
            .or_insert_with(|| host.clone());
    }
    map
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
        // Spawn target = working directory; files are filtered out so
        // the wildmenu only shows pickable candidates. Mirrors the
        // local `scan_dir` policy.
        .filter(|e| e.is_dir)
        .map(|e| {
            let name = format!("{}/", e.name);
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

/// Scan `dir` for *directory* entries matching `prefix`, returning at most
/// `cap` names with a trailing slash. Files are excluded — the spawn modal
/// picks a working directory for the agent, so a regular file is never a
/// valid candidate. Hidden entries are kept only if the prefix itself
/// starts with a dot.
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
            // Spawn target = working directory; files are filtered
            // out so the wildmenu only shows pickable candidates.
            // `file_type()` failure (e.g. dangling symlink) is treated
            // as "not a dir" and skipped — same as the previous
            // is_dir-or-bust behavior, just without the file branch.
            if !e.file_type().ok()?.is_dir() {
                return None;
            }
            Some(format!("{name}/"))
        })
        .collect();
    out.sort();
    out.truncate(cap);
    out
}

/// Host-zone completions: filter the cached SSH `Host` list against the
/// typed prefix. Always pins the [`HOST_PLACEHOLDER`] (`"local"`) entry to
/// the top so the local-spawn target is one Enter away — the runtime
/// already routes `host == "local"` through the local-PTY branch (see
/// `runtime.rs`). When the user has typed a prefix, `local` is included
/// only if it matches (so typing `dev` doesn't surface `local`).
///
/// Returns the full pool when the input is empty (so the user can browse).
fn host_completions(input: &str, hosts: &[String]) -> Vec<String> {
    let needle = input.trim().to_lowercase();
    let local = HOST_PLACEHOLDER.to_string();

    if needle.is_empty() {
        let mut out = Vec::with_capacity(hosts.len() + 1);
        out.push(local);
        out.extend(hosts.iter().cloned());
        return out;
    }

    let mut out = Vec::new();
    // `local` is always first when it matches the typed prefix /
    // substring — bypass the score-then-sort pipeline so it can't
    // be outranked by a hostname that happens to score lower.
    if score(HOST_PLACEHOLDER, &needle).is_some() {
        out.push(local);
    }
    let mut scored: Vec<(usize, &String)> = hosts
        .iter()
        .filter_map(|c| score(&c.to_lowercase(), &needle).map(|s| (s, c)))
        .collect();
    scored.sort_by_key(|(s, _)| *s);
    out.extend(scored.into_iter().map(|(_, c)| c.clone()));
    out
}

fn score(haystack: &str, needle: &str) -> Option<usize> {
    haystack.find(needle).map(|pos| {
        let prefix_bonus = if pos == 0 { 0 } else { 100 };
        prefix_bonus + pos + haystack.len() / 8
    })
}

/// Strip the parent directory from a path-zone candidate so the
/// wildmenu shows just the leaf — `/Users/x/codemux/apps/` renders as
/// `apps/`, `/etc/hosts` as `hosts`. The full path lives in the
/// `filtered` Vec so applying the candidate (Tab, Enter) still uses
/// the correct value; this is purely a render-time transform.
///
/// Edge cases: empty input → empty output; single-segment path with
/// no separator → returned unchanged; `"/"` → `"/"` (root directory).
fn basename_for_display(path: &str) -> String {
    let (stem, suffix) = match path.strip_suffix('/') {
        Some(stem) => (stem, "/"),
        None => (path, ""),
    };
    let base = stem.rsplit('/').next().unwrap_or(stem);
    format!("{base}{suffix}")
}

/// Delete the last "word" from `s`, where "word" follows path-segment
/// semantics: drop the trailing `/` if any, then drop everything back
/// to and including the previous `/`. If no `/` remains, clear the
/// string. Mirrors Ctrl-W in zsh / bash with `WORDCHARS` configured
/// for path editing.
///
/// Examples:
/// - `"/Users/x/codemux/"` → `"/Users/x/"`
/// - `"/Users/x/codemux"`  → `"/Users/x/"`
/// - `"abc"`               → `""`
/// - `""`                  → `""`
fn delete_word_backward(s: &mut String) {
    if s.ends_with('/') {
        s.pop();
    }
    if let Some(idx) = s.rfind('/') {
        s.truncate(idx + 1);
    } else {
        s.clear();
    }
}

/// Pick the wildmenu display text for a candidate. Path-zone
/// candidates collapse to the leaf folder (`apps/`) because the path
/// field already shows the parent context; host-zone candidates show
/// the full host name. The `filtered` Vec keeps full paths so
/// applying a candidate (Tab / Enter) still uses the correct value —
/// this is purely a render-time transform.
fn wildmenu_display_text(zone: Zone, candidate: &str) -> String {
    match zone {
        Zone::Path => basename_for_display(candidate),
        Zone::Host => candidate.to_string(),
    }
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
            // Tests that exercise the auto-seeded behavior set this
            // explicitly; the default mirrors the user-typed semantics
            // the bulk of the tests (which set `path` to a literal
            // value) were written against.
            path_origin: PathOrigin::UserTyped,
            // A stable test cwd. The Esc/Confirm tests that exercise
            // the cwd-reseed behavior assert against this value.
            cwd: PathBuf::from("/test/cwd"),
            // Default to Precise so the existing test suite (which
            // pre-dates fuzzy mode) keeps its original semantics. The
            // fuzzy-mode tests further down opt in explicitly.
            search_mode: SearchMode::Precise,
            user_search_mode: SearchMode::Precise,
            fuzzy_query: String::new(),
            named_projects: Vec::new(),
            project_hosts: HashMap::new(),
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

    /// Esc in the host zone is "back to path", not "close modal" —
    /// the user opened the host picker and changed their mind. The
    /// path field is reseeded with the cwd if it was previously
    /// cleared (the `@`-into-host clear) so the user lands at a
    /// usable folder picker.
    #[test]
    fn esc_in_host_zone_returns_to_path_with_cwd_reseeded() {
        let mut m = mb("", "", Zone::Host, &["alpha"]);
        // Simulate the post-`@` state: path empty, focus = Host.
        let outcome = m.handle(&key(KeyCode::Esc), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.focused, Zone::Path);
        assert_eq!(m.path, "/test/cwd/");
        assert_eq!(m.path_origin, PathOrigin::AutoSeeded);
    }

    /// Esc in host zone preserves a non-empty path (the user's
    /// already-typed path; we only reseed if the field is empty).
    #[test]
    fn esc_in_host_zone_preserves_user_typed_path() {
        let mut m = mb("", "/work/proj", Zone::Host, &["alpha"]);
        let outcome = m.handle(&key(KeyCode::Esc), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.focused, Zone::Path);
        assert_eq!(m.path, "/work/proj");
        assert_eq!(m.path_origin, PathOrigin::UserTyped);
    }

    /// Ctrl-Backspace in the path zone deletes the last path segment
    /// (back through the previous `/`). Chosen over plain Backspace
    /// because typing a long path and shaving it one char at a time
    /// is tedious — power users expect Ctrl-Backspace to behave like
    /// the shell's word delete.
    #[test]
    fn ctrl_backspace_deletes_word_in_path_zone() {
        let mut m = mb("", "/Users/x/codemux/", Zone::Path, &[]);
        let outcome = m.handle(&ctrl(KeyCode::Backspace), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.path, "/Users/x/");
    }

    /// Ctrl-W is the vim/shell convention for "delete word backward".
    /// Same semantics as Ctrl-Backspace — the modal accepts both.
    #[test]
    fn ctrl_w_deletes_word_in_path_zone() {
        let mut m = mb("", "/Users/x/codemux", Zone::Path, &[]);
        let outcome = m.handle(&ctrl(KeyCode::Char('w')), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.path, "/Users/x/");
    }

    /// Ctrl-U clears the focused field. Useful to wipe the seeded
    /// cwd in one keystroke and start typing a different path from
    /// scratch (or to clear a typed-but-wrong host name).
    #[test]
    fn ctrl_u_clears_field_in_path_zone() {
        let mut m = mb("", "/Users/x/codemux", Zone::Path, &[]);
        let outcome = m.handle(&ctrl(KeyCode::Char('u')), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.path, "");
    }

    /// Ctrl-Backspace also marks the path as user-touched — same as
    /// any other path-zone edit. Without this, a follow-up
    /// `@`-into-host wouldn't clear the field (auto-seed defense).
    #[test]
    fn ctrl_backspace_clears_auto_seeded_marker() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = SpawnMinibuffer::open(dir.path(), SearchMode::Precise, Vec::new());
        assert_eq!(m.path_origin, PathOrigin::AutoSeeded);
        m.handle(&ctrl(KeyCode::Backspace), &b(), &mut local());
        assert_eq!(m.path_origin, PathOrigin::UserTyped);
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
        // wildmenu lists every SSH host (with `local` pinned first so
        // the local-spawn target is always one Enter away), and
        // `selected = None` so a stray Enter spawns local rather than
        // silently picking the first host.
        let mut m = mb("", "/tmp", Zone::Path, &["alpha", "bravo"]);
        m.handle(&key(KeyCode::Char('@')), &b(), &mut local());
        assert_eq!(m.focused, Zone::Host);
        assert_eq!(m.host, "");
        assert_eq!(
            m.filtered,
            vec![
                "local".to_string(),
                "alpha".to_string(),
                "bravo".to_string()
            ]
        );
        assert_eq!(m.selected, None);
    }

    #[test]
    fn at_does_not_overwrite_existing_host() {
        let mut m = mb("custom", "/tmp", Zone::Path, &["alpha"]);
        m.handle(&key(KeyCode::Char('@')), &b(), &mut local());
        assert_eq!(m.focused, Zone::Host);
        assert_eq!(m.host, "custom");
    }

    /// `open(cwd)` seeds the path zone with the cwd as a string,
    /// appending a trailing `/` so the wildmenu lists subfolders. The
    /// `path_origin` flag is set to `AutoSeeded` so a follow-up `@`-into-host
    /// can clear the local-only path before the SSH flow runs.
    #[test]
    fn open_seeds_path_with_cwd_and_marks_auto_seeded() {
        let dir = tempfile::tempdir().unwrap();
        let m = SpawnMinibuffer::open(dir.path(), SearchMode::Precise, Vec::new());
        let expected = format!("{}/", dir.path().display());
        assert_eq!(m.path, expected);
        assert_eq!(m.path_origin, PathOrigin::AutoSeeded);
        assert_eq!(m.focused, Zone::Path);
    }

    /// `open(cwd)` is idempotent on a cwd that already ends in `/`
    /// (root, or any path the caller normalized). Don't double-slash.
    #[test]
    fn open_does_not_double_slash_when_cwd_already_ends_in_slash() {
        let m = SpawnMinibuffer::open(Path::new("/"), SearchMode::Precise, Vec::new());
        assert_eq!(m.path, "/");
    }

    /// User typing in the path zone flips `path_origin` to `UserTyped` — from
    /// that point forward the field is "user-typed" and the SSH-flow
    /// clearing on `@` no longer applies.
    #[test]
    fn typing_in_path_zone_clears_auto_seeded() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = SpawnMinibuffer::open(dir.path(), SearchMode::Precise, Vec::new());
        assert_eq!(m.path_origin, PathOrigin::AutoSeeded);
        m.handle(&key(KeyCode::Char('x')), &b(), &mut local());
        assert_eq!(m.path_origin, PathOrigin::UserTyped);
    }

    /// Backspacing in the path zone also marks the field as
    /// user-touched. (Trimming the trailing slash is a common
    /// gesture to back out of the seeded cwd into the parent.)
    #[test]
    fn backspace_in_path_zone_clears_auto_seeded() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = SpawnMinibuffer::open(dir.path(), SearchMode::Precise, Vec::new());
        m.handle(&key(KeyCode::Backspace), &b(), &mut local());
        assert_eq!(m.path_origin, PathOrigin::UserTyped);
    }

    /// Tab-applying a wildmenu candidate is also a user choice; the
    /// resulting path is the candidate's value (without the trailing
    /// slash on first Tab, see [`SpawnMinibuffer::apply_path_completion`]),
    /// not the auto-seeded cwd, so the marker is cleared.
    #[test]
    fn tab_completion_in_path_zone_clears_auto_seeded() {
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        m.path_origin = PathOrigin::AutoSeeded;
        m.filtered = vec!["/tmp/alpha/".into()];
        m.selected = Some(0);
        m.handle(&key(KeyCode::Tab), &b(), &mut local());
        // First Tab applies the basename without the trailing slash.
        assert_eq!(m.path, "/tmp/alpha");
        assert_eq!(m.path_origin, PathOrigin::UserTyped);
    }

    /// `@` into the host zone clears the path field if it was
    /// auto-seeded (the local cwd doesn't apply to remote targets,
    /// and the SSH flow's `Enter on host with empty path →
    /// PrepareHost` gesture needs the field empty to fire).
    #[test]
    fn at_into_host_clears_auto_seeded_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = SpawnMinibuffer::open(dir.path(), SearchMode::Precise, Vec::new());
        assert!(!m.path.is_empty());
        m.handle(&key(KeyCode::Char('@')), &b(), &mut local());
        assert_eq!(m.focused, Zone::Host);
        assert_eq!(m.path, "");
        assert_eq!(m.path_origin, PathOrigin::UserTyped);
    }

    /// `@` into the host zone PRESERVES a user-typed path. The
    /// existing power-user escape hatch (`/path` then `@host` Enter
    /// spawns directly at that path on the remote) must still work.
    #[test]
    fn at_into_host_preserves_user_typed_path() {
        let mut m = mb("", "/work/project", Zone::Path, &["alpha"]);
        // mb() defaults path_origin to PathOrigin::UserTyped.
        m.handle(&key(KeyCode::Char('@')), &b(), &mut local());
        assert_eq!(m.focused, Zone::Host);
        assert_eq!(m.path, "/work/project");
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

    /// First Tab on a folder candidate applies the basename WITHOUT
    /// the trailing slash, so the wildmenu stays at the same level
    /// (the user is "selecting" the folder, not descending into it).
    /// Mirrors zsh / fish autocomplete: confirm the pick, then choose
    /// to descend.
    #[test]
    fn first_tab_on_folder_applies_without_trailing_slash() {
        let mut m = mb("", "/tmp/", Zone::Path, &[]);
        m.filtered = vec!["/tmp/alpha/".into(), "/tmp/beta/".into()];
        m.selected = Some(0);
        let outcome = m.handle(&key(KeyCode::Tab), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.path, "/tmp/alpha");
    }

    /// Second Tab on the same folder (path already matches the
    /// trimmed candidate) applies WITH the trailing slash, which
    /// causes `refresh` to list the folder's contents — i.e.
    /// descend.
    #[test]
    fn second_tab_on_same_folder_descends() {
        let mut m = mb("", "/tmp/alpha", Zone::Path, &[]);
        m.filtered = vec!["/tmp/alpha/".into()];
        m.selected = Some(0);
        let outcome = m.handle(&key(KeyCode::Tab), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.path, "/tmp/alpha/");
    }

    /// Typing `/` after a first Tab also descends (refresh re-runs
    /// against the new path with trailing slash). This test pins the
    /// keystroke-equivalence so the user has two ways to descend.
    #[test]
    fn typing_slash_after_first_tab_also_descends() {
        let mut m = mb("", "/tmp/alpha", Zone::Path, &[]);
        m.handle(&key(KeyCode::Char('/')), &b(), &mut local());
        assert_eq!(m.path, "/tmp/alpha/");
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

    /// Path-zone Enter with no selection, no typed query, and an
    /// empty path field is the "user picked nothing" gesture — the
    /// modal emits `SpawnScratch` so the runtime can route the
    /// agent into the configured scratch dir. The host is preserved
    /// (so the remote-with-no-folder-pick case lands at the remote
    /// scratch dir, not local).
    ///
    /// Replaces the older "empty path passes through to Spawn" test:
    /// after the scratch fallback landed, this synthetic state is no
    /// longer reachable as Spawn{path:""}. The runtime still has the
    /// machinery to translate empty → None for safety, but the modal
    /// no longer emits it.
    #[test]
    fn empty_path_with_no_selection_emits_spawn_scratch() {
        let mut m = mb("devpod-go", "", Zone::Path, &["devpod-go"]);
        m.filtered = vec![];
        m.selected = None;
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::SpawnScratch {
                host: "devpod-go".into(),
            },
        );
    }

    /// Local variant of the same gesture: no host typed, empty path,
    /// no wildmenu pick. Resolves the placeholder host so the runtime
    /// knows to mkdir + spawn locally.
    #[test]
    fn empty_local_path_with_no_selection_emits_spawn_scratch() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.filtered = vec![];
        m.selected = None;
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::SpawnScratch {
                host: HOST_PLACEHOLDER.into(),
            },
        );
    }

    /// Auto-seeded path field still counts as "passive" — covers the
    /// real-world Fuzzy-mode flow where the modal opens with cwd /
    /// remote $HOME pre-filled but the user neither typed nor picked.
    /// Without this, the post-prepare SSH path would silently spawn at
    /// remote $HOME instead of the configured scratch dir.
    #[test]
    fn auto_seeded_path_with_no_selection_emits_spawn_scratch() {
        let mut m = mb("devpod-go", "/root/", Zone::Path, &["devpod-go"]);
        m.path_origin = PathOrigin::AutoSeeded;
        m.filtered = vec![];
        m.selected = None;
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::SpawnScratch {
                host: "devpod-go".into(),
            },
        );
    }

    /// User typed a path (or applied a Tab completion) → `path_origin`
    /// is `UserTyped`, even if the field happens to be non-empty. With
    /// no wildmenu pick that maps to the literal typed path via the
    /// existing Spawn route — scratch only fires when the user really
    /// did nothing.
    #[test]
    fn user_typed_path_with_no_selection_falls_through_to_spawn() {
        let mut m = mb("", "/work", Zone::Path, &[]);
        m.path_origin = PathOrigin::UserTyped;
        m.filtered = vec![];
        m.selected = None;
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "local".into(),
                path: "/work".into(),
            },
        );
    }

    /// A wildmenu pick beats the scratch fallback. The user explicitly
    /// chose something — the modal must commit that choice, not shunt
    /// them into the sandbox dir.
    #[test]
    fn highlighted_candidate_overrides_scratch_fallback() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.path_origin = PathOrigin::AutoSeeded;
        m.filtered = vec!["/tmp/alpha".into()];
        m.selected = Some(0);
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "local".into(),
                path: "/tmp/alpha".into(),
            },
        );
    }

    /// A non-empty fuzzy query also overrides the scratch fallback —
    /// the user is actively searching, even if nothing matches yet.
    /// Falling through to `Spawn { path: "" }` is the right thing
    /// here: the runtime treats an empty path as "use platform default
    /// cwd," matching the long-standing "I typed gibberish, give me
    /// the default" semantic.
    #[test]
    fn typed_fuzzy_query_overrides_scratch_fallback() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.user_search_mode = SearchMode::Fuzzy;
        m.fuzzy_query = "no-match".into();
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
        // Filtered list is `local` + the three SSH hosts (`local` is
        // pinned first per `host_completions`), giving four entries
        // total. Empty field → no implicit selection; first Down
        // advances from None to Some(0), then it cycles 0→1→2→3→0.
        let mut m = mb("", "", Zone::Host, &["a", "b", "c"]);
        assert_eq!(m.selected, None);
        m.handle(&key(KeyCode::Down), &b(), &mut local());
        assert_eq!(m.selected, Some(0));
        m.handle(&key(KeyCode::Down), &b(), &mut local());
        assert_eq!(m.selected, Some(1));
        m.handle(&key(KeyCode::Down), &b(), &mut local());
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
        // Empty input returns `local` first followed by every SSH host;
        // the local-spawn target is always one Enter away.
        let pool = vec!["a".into(), "b".into()];
        assert_eq!(
            host_completions("", &pool),
            vec!["local".to_string(), "a".to_string(), "b".to_string()],
        );
    }

    #[test]
    fn host_completions_with_typed_prefix_pins_local_first_when_it_matches() {
        let pool = vec!["alpine-1".into(), "loki".into()];
        // `lo` matches both `local` and `loki`; `local` must come first
        // even though `loki` would have a comparable score on its own.
        assert_eq!(
            host_completions("lo", &pool),
            vec!["local".to_string(), "loki".to_string()],
        );
    }

    #[test]
    fn host_completions_omits_local_when_input_does_not_match_it() {
        let pool = vec!["devpod-1".into(), "devpod-2".into()];
        // `dev` does not appear in `local`, so `local` is dropped from
        // the completions — surfacing it would be a distraction.
        assert_eq!(
            host_completions("dev", &pool),
            vec!["devpod-1".to_string(), "devpod-2".to_string()],
        );
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

    /// Local `scan_dir` only returns directory entries; regular files
    /// in the same parent are silently dropped. Spawn target = working
    /// directory, so a file is never a valid candidate. Mirrors the
    /// remote-side `remote_completions_filters_out_files` test.
    #[test]
    fn scan_dir_filters_out_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("alpha")).unwrap();
        std::fs::write(dir.path().join("beta.txt"), b"").unwrap();
        std::fs::create_dir(dir.path().join("gamma")).unwrap();
        std::fs::write(dir.path().join("delta.md"), b"").unwrap();

        let out = scan_dir(dir.path(), "", 8);
        assert_eq!(out, vec!["alpha/".to_string(), "gamma/".to_string()]);
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

    /// `basename_for_display` strips the parent dir from a path
    /// candidate so the wildmenu shows the leaf only. Trailing slash
    /// indicates a folder and must be preserved on output.
    #[test]
    fn basename_for_display_extracts_leaf_with_trailing_slash() {
        assert_eq!(basename_for_display("/Users/x/codemux/apps/"), "apps/");
    }

    #[test]
    fn basename_for_display_extracts_leaf_without_trailing_slash() {
        assert_eq!(basename_for_display("/etc/hosts"), "hosts");
    }

    #[test]
    fn basename_for_display_handles_root() {
        assert_eq!(basename_for_display("/"), "/");
    }

    #[test]
    fn basename_for_display_handles_single_segment() {
        assert_eq!(basename_for_display("apps/"), "apps/");
        assert_eq!(basename_for_display("apps"), "apps");
    }

    #[test]
    fn basename_for_display_handles_empty() {
        assert_eq!(basename_for_display(""), "");
    }

    /// Wildmenu picks the right display text per zone: leaf folder
    /// in path zone, full host name in host zone. Pinned here as a
    /// guard against future "swap the arms" refactors.
    #[test]
    fn wildmenu_display_text_path_zone_returns_basename() {
        assert_eq!(
            wildmenu_display_text(Zone::Path, "/Users/x/codemux/apps/"),
            "apps/",
        );
    }

    #[test]
    fn wildmenu_display_text_host_zone_returns_full_value() {
        assert_eq!(wildmenu_display_text(Zone::Host, "devpod-go"), "devpod-go");
    }

    /// `delete_word_backward` operates on path-segment boundaries:
    /// drop the trailing `/` if any, then drop everything back to
    /// (and including) the previous `/`. If there's nothing left to
    /// scan, the string is cleared.
    #[test]
    fn delete_word_backward_removes_trailing_segment_with_slash() {
        let mut s = String::from("/Users/x/codemux/");
        delete_word_backward(&mut s);
        assert_eq!(s, "/Users/x/");
    }

    #[test]
    fn delete_word_backward_removes_trailing_segment_without_slash() {
        let mut s = String::from("/Users/x/codemux");
        delete_word_backward(&mut s);
        assert_eq!(s, "/Users/x/");
    }

    #[test]
    fn delete_word_backward_clears_when_no_slash() {
        let mut s = String::from("abc");
        delete_word_backward(&mut s);
        assert_eq!(s, "");
    }

    #[test]
    fn delete_word_backward_on_empty_is_noop() {
        let mut s = String::new();
        delete_word_backward(&mut s);
        assert_eq!(s, "");
    }

    #[test]
    fn delete_word_backward_on_root_clears() {
        let mut s = String::from("/");
        delete_word_backward(&mut s);
        // After stripping the trailing `/`, the string is empty and
        // there's no `/` to find — clear it.
        assert_eq!(s, "");
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
        // Filtered list is `local` + the three SSH hosts (`local` is
        // pinned first per `host_completions`), so backward-from-None
        // wraps to the last index 3, not 2.
        let mut m = mb("", "", Zone::Host, &["a", "b", "c"]);
        m.selected = None;
        m.move_selection_backward();
        assert_eq!(m.selected, Some(3));
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
    fn focused_value_style_is_cyan_bold() {
        let s = focused_value_style();
        assert_eq!(s.fg, Some(Color::Cyan));
        assert!(s.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn path_unfocused_value_style_is_default() {
        // Path zone uses the terminal default for unfocused values
        // — the chrome surrounding it (cyan label, separator) carries
        // the visual cues; the path text itself just sits there.
        assert_eq!(path_unfocused_value_style(), Style::default());
    }

    #[test]
    fn host_unfocused_value_style_is_cyan_not_bold() {
        // Host zone keeps a cyan fg even when unfocused so a
        // committed SSH host (e.g. devpod-go) stays visibly
        // "selected" after focus moves to the path zone. NOT bold so
        // the focused zone's bolded text still wins for emphasis.
        let s = host_unfocused_value_style();
        assert_eq!(s.fg, Some(Color::Cyan));
        assert!(
            !s.add_modifier.contains(Modifier::BOLD),
            "unfocused host must not be bold (would compete with the focused zone)",
        );
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
        let spans = zone_spans(
            true,
            "",
            "local",
            placeholder_style,
            cursor_style,
            false,
            path_unfocused_value_style(),
        );
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
        let spans = zone_spans(
            true,
            "",
            "x",
            Style::default(),
            Style::default(),
            false,
            path_unfocused_value_style(),
        );
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "█");
    }

    #[test]
    fn zone_spans_empty_unfocused_renders_full_placeholder_no_cursor() {
        let placeholder_style = Style::default().add_modifier(Modifier::DIM);
        let cursor_style = Style::default();
        let spans = zone_spans(
            false,
            "",
            "local",
            placeholder_style,
            cursor_style,
            false,
            path_unfocused_value_style(),
        );
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
            path_unfocused_value_style(),
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
            path_unfocused_value_style(),
        );
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "alpha");
    }

    /// Host zone, unfocused, with a committed SSH host: the value
    /// must render in the host-zone unfocused style (cyan, not bold)
    /// rather than the path-zone default. Without this, the host
    /// disappears into the terminal's default color the moment focus
    /// moves to the path zone — the user's reported bug.
    #[test]
    fn zone_spans_host_unfocused_with_value_renders_in_cyan() {
        let spans = zone_spans(
            false,
            "devpod-go",
            "local",
            Style::default(),
            Style::default(),
            false,
            host_unfocused_value_style(),
        );
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "devpod-go");
        assert_eq!(spans[0].style.fg, Some(Color::Cyan));
        assert!(
            !spans[0].style.add_modifier.contains(Modifier::BOLD),
            "unfocused host must not be bold",
        );
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
            path_unfocused_value_style(),
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
            path_unfocused_value_style(),
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
            path_unfocused_value_style(),
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
            path_unfocused_value_style(),
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
    fn enter_on_host_with_empty_path_and_local_host_routes_to_path_zone() {
        // Empty host + empty path on the host zone is "I want to pick
        // a folder next" for local — switch to the path zone with the
        // cwd reseeded, mirroring the SSH flow's post-prepare reseed
        // with the remote `$HOME`. No spawn yet.
        let mut m = mb("", "", Zone::Host, &[]);
        m.filtered = vec![];
        m.selected = None;
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.focused, Zone::Path);
        // Path was reseeded from the test cwd in `mb()` (`/test/cwd`).
        assert_eq!(m.path, "/test/cwd/");
        assert_eq!(m.path_origin, PathOrigin::AutoSeeded);
        // Host cleared so the dim `local` placeholder shows.
        assert_eq!(m.host, "");
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
        // The remote $HOME is system-derived, not user-typed. Mark
        // it auto-seeded so a follow-up `@`-into-host clears it
        // before the SSH flow's `Enter on empty path → PrepareHost`
        // gesture would otherwise smuggle a stale path through.
        assert_eq!(m.path_origin, PathOrigin::AutoSeeded);
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
        // Sorted; dotfile filtered (prefix is empty); files (`README.md`)
        // are filtered too — spawn target is a working directory, not a
        // file. Mirrors the local `scan_dir` behavior.
        assert_eq!(
            out,
            vec!["/home/df/bin/".to_string(), "/home/df/src/".to_string(),],
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
    fn remote_completions_show_dot_dirs_when_prefix_starts_with_dot() {
        // The dot needs at least one trailing char; `/home/df/.` on
        // its own normalizes away (Path::file_name returns None on a
        // bare `.`), matching the local-completion behavior. Dot
        // *files* (`.bashrc`) are still filtered because they're not
        // directories — only dot *dirs* (`.config/`) survive.
        let fs = fake_fs();
        let runner = ScriptedRunner::new(vec![b".bashrc\n.config/\nREADME.md\n"]);
        let mut cache = HashMap::new();
        let out = remote_path_completions("/home/df/.c", &fs, &runner, &mut cache);
        assert_eq!(out, vec!["/home/df/.config/".to_string()]);
    }

    /// Files in the listing must be filtered out regardless of how
    /// well their basename matches the prefix. Pinning this here
    /// because the dotfile / sort tests don't exercise the file
    /// filter on its own.
    #[test]
    fn remote_completions_filters_out_files() {
        let fs = fake_fs();
        let runner = ScriptedRunner::new(vec![b"alpha.txt\nbeta/\ngamma.md\ndelta/\n"]);
        let mut cache = HashMap::new();
        let out = remote_path_completions("/home/df/", &fs, &runner, &mut cache);
        assert_eq!(
            out,
            vec!["/home/df/beta/".to_string(), "/home/df/delta/".to_string()],
        );
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
        // distinct cache keys. Both responses are dirs so the
        // file-filter doesn't drop them.
        let fs = fake_fs();
        let runner = ScriptedRunner::new(vec![b"src/\n", b"build/\nlib/\n"]);
        let mut cache = HashMap::new();

        let _ = remote_path_completions("/home/df/", &fs, &runner, &mut cache);
        let out2 = remote_path_completions("/home/df/src/", &fs, &runner, &mut cache);
        assert_eq!(runner.call_count(), 2);
        assert_eq!(
            out2,
            vec![
                "/home/df/src/build/".to_string(),
                "/home/df/src/lib/".to_string(),
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

    /// Build a `Ready` `IndexState` from a slice of plain path strings.
    /// Tests that need typed kinds construct `IndexedDir` literals
    /// directly.
    fn ready_index_plain(paths: &[&str]) -> IndexState {
        IndexState::Ready {
            dirs: paths
                .iter()
                .map(|p| IndexedDir {
                    path: PathBuf::from(p),
                    kind: ProjectKind::Plain,
                })
                .collect(),
        }
    }

    /// Build a `Refreshing` index state with the given paths as the
    /// stale-but-usable cache. The handle is inert (no live worker)
    /// so the test owns the lifecycle entirely.
    fn refreshing_index_plain(paths: &[&str]) -> IndexState {
        IndexState::Refreshing {
            dirs: paths
                .iter()
                .map(|p| IndexedDir {
                    path: PathBuf::from(p),
                    kind: ProjectKind::Plain,
                })
                .collect(),
            handle: crate::index_worker::inert_handle_for_test(),
            count: 0,
        }
    }

    #[test]
    fn open_fuzzy_starts_with_empty_path() {
        let dir = tempfile::tempdir().unwrap();
        let m = SpawnMinibuffer::open(dir.path(), SearchMode::Fuzzy, Vec::new());
        assert!(
            m.path.is_empty(),
            "fuzzy open should leave path empty (got {:?})",
            m.path,
        );
        assert!(m.fuzzy_query.is_empty());
        assert_eq!(m.search_mode, SearchMode::Fuzzy);
        assert_eq!(m.user_search_mode, SearchMode::Fuzzy);
    }

    #[test]
    fn open_precise_seeds_path_with_cwd() {
        // Pin: existing behavior. Precise mode should still seed.
        let dir = tempfile::tempdir().unwrap();
        let m = SpawnMinibuffer::open(dir.path(), SearchMode::Precise, Vec::new());
        assert!(m.path.starts_with(&*dir.path().to_string_lossy()));
        assert!(m.path.ends_with('/'));
        assert_eq!(m.search_mode, SearchMode::Precise);
    }

    #[test]
    fn ctrl_t_toggles_fuzzy_to_precise_and_seeds_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = SpawnMinibuffer::open(dir.path(), SearchMode::Fuzzy, Vec::new());
        let outcome = m.handle(&ctrl(KeyCode::Char('t')), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.search_mode, SearchMode::Precise);
        assert_eq!(m.user_search_mode, SearchMode::Precise);
        // Toggling to Precise with empty path re-seeds cwd so the
        // wildmenu has something to show.
        assert!(m.path.starts_with(&*dir.path().to_string_lossy()));
    }

    #[test]
    fn ctrl_t_toggles_precise_to_fuzzy() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = SpawnMinibuffer::open(dir.path(), SearchMode::Precise, Vec::new());
        m.handle(&ctrl(KeyCode::Char('t')), &b(), &mut local());
        assert_eq!(m.search_mode, SearchMode::Fuzzy);
        assert_eq!(m.user_search_mode, SearchMode::Fuzzy);
        // Fuzzy mode owns its own wildmenu lifecycle — toggling should
        // clear filtered/selected so a stale Precise highlight isn't
        // committed by an Enter under the new mode.
        assert!(m.filtered.is_empty());
        assert_eq!(m.selected, None);
    }

    #[test]
    fn tab_is_no_op_in_fuzzy_path_zone() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.user_search_mode = SearchMode::Fuzzy;
        // Wipe any precise-mode wildmenu the `mb()` constructor's
        // refresh left behind so we can assert Tab doesn't repopulate.
        m.filtered.clear();
        m.selected = None;
        let outcome = m.handle(&key(KeyCode::Tab), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        // Tab in fuzzy must not trigger any wildmenu mutation.
        assert!(m.filtered.is_empty());
        assert_eq!(m.focused, Zone::Path);
    }

    #[test]
    fn ctrl_r_emits_refresh_index_outcome_in_fuzzy_mode() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.user_search_mode = SearchMode::Fuzzy;
        let outcome = m.handle(&ctrl(KeyCode::Char('r')), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::RefreshIndex);
    }

    #[test]
    fn remote_unlock_preserves_user_search_mode() {
        // SSH spawn no longer forces Precise — the runtime now builds
        // a per-host fuzzy index over ssh+find, so remote modals can
        // use the same fuzzy UX as local. The user's preferred mode
        // wins on remote just like on local.
        let mut m = mb("devpod-web", "", Zone::Host, &["devpod-web"]);
        m.search_mode = SearchMode::Fuzzy;
        m.user_search_mode = SearchMode::Fuzzy;
        m.unlock_for_remote_path(
            "devpod-web".to_string(),
            PathBuf::from("/home/df"),
            &mut local(),
        );
        assert_eq!(
            m.search_mode,
            SearchMode::Fuzzy,
            "remote preserves user-preferred fuzzy mode",
        );
        assert_eq!(
            m.user_search_mode,
            SearchMode::Fuzzy,
            "user preference still tracked",
        );
    }

    #[test]
    fn remote_unlock_preserves_user_precise_mode() {
        // The other half of the same contract: a user who toggled to
        // Precise locally keeps Precise after the remote unlock.
        let mut m = mb("devpod-web", "", Zone::Host, &["devpod-web"]);
        m.search_mode = SearchMode::Precise;
        m.user_search_mode = SearchMode::Precise;
        m.unlock_for_remote_path(
            "devpod-web".to_string(),
            PathBuf::from("/home/df"),
            &mut local(),
        );
        assert_eq!(m.search_mode, SearchMode::Precise);
    }

    #[test]
    fn unlock_back_to_host_restores_user_search_mode() {
        let mut m = mb("devpod-web", "/home/df/", Zone::Path, &["devpod-web"]);
        m.search_mode = SearchMode::Precise; // forced by the prepare path
        m.user_search_mode = SearchMode::Fuzzy; // user's preference
        m.unlock_back_to_host(&mut local(), None);
        assert_eq!(m.search_mode, SearchMode::Fuzzy);
        assert_eq!(m.focused, Zone::Host);
    }

    #[test]
    fn refresh_fuzzy_with_ready_index_populates_filtered_with_full_paths() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.fuzzy_query = "code".to_string();
        let idx = ready_index_plain(&[
            "/home/df/Workbench/repositories/codemux",
            "/home/df/Library/something",
            "/home/df/code-utils",
        ]);
        m.refresh_fuzzy(Some(&idx));
        assert!(!m.filtered.is_empty(), "should have matches for 'code'");
        assert!(
            m.filtered
                .iter()
                .any(|p| p == "/home/df/Workbench/repositories/codemux"),
            "codemux should be in matches: {:?}",
            m.filtered,
        );
        assert_eq!(m.selected, Some(0));
        // Full paths, not basenames.
        assert!(m.filtered.iter().all(|p| p.contains('/')));
    }

    #[test]
    fn refresh_fuzzy_empty_query_clears_wildmenu() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.fuzzy_query.clear();
        m.filtered = vec!["stale".to_string()];
        m.selected = Some(0);
        let idx = ready_index_plain(&["/anything"]);
        m.refresh_fuzzy(Some(&idx));
        assert!(m.filtered.is_empty());
        assert_eq!(m.selected, None);
    }

    #[test]
    fn refresh_fuzzy_with_building_index_leaves_filtered_empty() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.fuzzy_query = "code".to_string();
        // None simulates the index not started yet. Both `None` and
        // the `Building` variant should keep `filtered` empty so the
        // wildmenu shows the indexer-state sentinel.
        m.refresh_fuzzy(None);
        assert!(m.filtered.is_empty());
    }

    #[test]
    fn score_fuzzy_sort_is_stable_on_score_ties() {
        // Two paths with the same query should sort lexicographically
        // when nucleo gives them the same score, so the wildmenu
        // doesn't flicker as the user types.
        let dirs = vec![
            IndexedDir {
                path: PathBuf::from("/home/df/zeta-codemux"),
                kind: ProjectKind::Plain,
            },
            IndexedDir {
                path: PathBuf::from("/home/df/alpha-codemux"),
                kind: ProjectKind::Plain,
            },
        ];
        let result_a = score_fuzzy("codemux", &dirs, &[]);
        let result_b = score_fuzzy("codemux", &dirs, &[]);
        assert_eq!(result_a, result_b, "sort must be deterministic");
        // Lex order on tie: alpha before zeta.
        if result_a.len() == 2 {
            assert!(
                result_a[0].contains("alpha"),
                "alpha should sort before zeta on score tie: {result_a:?}",
            );
        }
    }

    #[test]
    fn score_fuzzy_git_repo_outranks_plain_at_same_match_quality() {
        // Same fuzzy match quality on both → the .git'd repo wins.
        let dirs = [
            ("/a/code", ProjectKind::Plain),
            ("/b/code", ProjectKind::Git),
        ];
        let result = score_fuzzy(
            "code",
            &dirs
                .iter()
                .map(|(p, k)| IndexedDir {
                    path: PathBuf::from(p),
                    kind: *k,
                })
                .collect::<Vec<_>>(),
            &[],
        );
        assert_eq!(result.first().map(String::as_str), Some("/b/code"));
    }

    #[test]
    fn score_fuzzy_marker_outranks_plain_but_not_git() {
        let dirs = [
            ("/x/proj", ProjectKind::Plain),
            ("/y/proj", ProjectKind::Marker),
            ("/z/proj", ProjectKind::Git),
        ];
        let result = score_fuzzy(
            "proj",
            &dirs
                .iter()
                .map(|(p, k)| IndexedDir {
                    path: PathBuf::from(p),
                    kind: *k,
                })
                .collect::<Vec<_>>(),
            &[],
        );
        assert_eq!(result[0], "/z/proj", "git wins: {result:?}");
        assert_eq!(result[1], "/y/proj", "marker second: {result:?}");
        assert_eq!(result[2], "/x/proj", "plain last: {result:?}");
    }

    #[test]
    fn score_fuzzy_named_project_outranks_git_repo() {
        let dirs = vec![IndexedDir {
            path: PathBuf::from("/auto/discovered/codemux"),
            kind: ProjectKind::Git,
        }];
        let named = vec![NamedProject {
            name: "codemux".to_string(),
            // Use absolute path so we don't depend on $HOME in test.
            path: "/explicit/alias/codemux".to_string(),
            host: None,
        }];
        let result = score_fuzzy("codemux", &dirs, &named);
        assert_eq!(
            result.first().map(String::as_str),
            Some("/explicit/alias/codemux"),
            "named project should outrank a git repo: {result:?}",
        );
    }

    #[test]
    fn score_fuzzy_dedupes_named_project_present_in_index() {
        // If a named project's path also lives in the index, only the
        // named entry should appear (deduped by path equality).
        let dirs = vec![IndexedDir {
            path: PathBuf::from("/work/codemux"),
            kind: ProjectKind::Git,
        }];
        let named = vec![NamedProject {
            name: "cm".to_string(),
            path: "/work/codemux".to_string(),
            host: None,
        }];
        let result = score_fuzzy("cm", &dirs, &named);
        let cm_count = result.iter().filter(|s| s == &"/work/codemux").count();
        assert_eq!(cm_count, 1, "dup not removed: {result:?}");
    }

    #[test]
    fn score_fuzzy_named_project_matches_against_name_not_path() {
        // The name `xy` shouldn't match `codemux` (path) — but it
        // should match the alias `xy`.
        let dirs = vec![];
        let named = vec![NamedProject {
            name: "xy".to_string(),
            path: "/some/where/codemux".to_string(),
            host: None,
        }];
        let result = score_fuzzy("xy", &dirs, &named);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "/some/where/codemux");

        // Querying for `mux` (a fragment of the path, not the name)
        // should NOT match the named project — the matcher only
        // looks at `name`.
        let result = score_fuzzy("mux", &dirs, &named);
        assert!(result.is_empty(), "should not match path: {result:?}");
    }

    // ── Project → host binding ──────────────────────────────────
    //
    // `build_project_hosts` is the lookup `confirm` consults at commit
    // time to upgrade `Spawn` → `PrepareHostThenSpawn`. The unit tests
    // below cover the construction (only entries with a non-empty
    // `host` make it in, and tilde-expansion matches the wildmenu's
    // emitted strings) and the emission path through `confirm` itself.

    #[test]
    fn build_project_hosts_skips_local_only_entries() {
        let projects = vec![
            NamedProject {
                name: "local-only".to_string(),
                path: "/local/p".to_string(),
                host: None,
            },
            NamedProject {
                name: "explicit-empty".to_string(),
                path: "/local/q".to_string(),
                host: Some(String::new()),
            },
        ];
        let map = build_project_hosts(&projects);
        assert!(
            map.is_empty(),
            "neither None nor empty-string host should bind: {map:?}",
        );
    }

    #[test]
    fn build_project_hosts_keys_by_expanded_path() {
        // Use an absolute path so the test doesn't depend on $HOME, but
        // also cover a `~/...` entry to lock the tilde-expansion in.
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let projects = vec![
            NamedProject {
                name: "abs".to_string(),
                path: "/work/code".to_string(),
                host: Some("devpod-1".to_string()),
            },
            NamedProject {
                name: "tilde".to_string(),
                path: "~/dotfiles".to_string(),
                host: Some("devpod-2".to_string()),
            },
        ];
        let map = build_project_hosts(&projects);
        assert_eq!(map.get("/work/code").map(String::as_str), Some("devpod-1"));
        if let Some(home) = home {
            let expanded = home.join("dotfiles").to_string_lossy().into_owned();
            assert_eq!(map.get(&expanded).map(String::as_str), Some("devpod-2"));
        }
    }

    #[test]
    fn confirm_local_project_emits_plain_spawn() {
        // A project without `host` keeps today's behavior: confirming
        // its path emits `Spawn { host: "local", path }`. Simulates
        // the real flow: project shows in the wildmenu, user picks
        // it, the resolved path is the project's expanded path.
        let mut m = SpawnMinibuffer::open(
            Path::new("/tmp"),
            SearchMode::Precise,
            vec![NamedProject {
                name: "p".to_string(),
                path: "/tmp/p".to_string(),
                host: None,
            }],
        );
        m.host.clear();
        m.path_origin = PathOrigin::UserTyped;
        m.filtered = vec!["/tmp/p".to_string()];
        m.selected = Some(0);
        m.focused = Zone::Path;
        let outcome = m.confirm(&mut DirLister::Local);
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "local".to_string(),
                path: "/tmp/p".to_string(),
            },
        );
    }

    #[test]
    fn confirm_host_bound_project_emits_prepare_host_then_spawn() {
        // A project with `host = "devpod-1"` is upgraded to
        // `PrepareHostThenSpawn`, regardless of what's in the host
        // zone. The project alias is the more specific signal. The
        // wildmenu pick is the project's expanded path (just like the
        // local case above).
        let mut m = SpawnMinibuffer::open(
            Path::new("/tmp"),
            SearchMode::Precise,
            vec![NamedProject {
                name: "p".to_string(),
                path: "/work/p".to_string(),
                host: Some("devpod-1".to_string()),
            }],
        );
        m.host = "ignored-typed-host".to_string();
        m.path_origin = PathOrigin::UserTyped;
        m.filtered = vec!["/work/p".to_string()];
        m.selected = Some(0);
        m.focused = Zone::Path;
        let outcome = m.confirm(&mut DirLister::Local);
        assert_eq!(
            outcome,
            ModalOutcome::PrepareHostThenSpawn {
                host: "devpod-1".to_string(),
                path: "/work/p".to_string(),
            },
        );
    }

    #[test]
    fn bootstrap_locked_drops_ctrl_t_and_ctrl_r() {
        let mut m = mb("devpod-web", "", Zone::Path, &["devpod-web"]);
        m.search_mode = SearchMode::Fuzzy;
        m.lock_for_bootstrap("devpod-web".to_string(), Instant::now());
        // Both chords should be silently dropped while locked.
        let outcome_t = m.handle(&ctrl(KeyCode::Char('t')), &b(), &mut local());
        let outcome_r = m.handle(&ctrl(KeyCode::Char('r')), &b(), &mut local());
        assert_eq!(outcome_t, ModalOutcome::None);
        assert_eq!(outcome_r, ModalOutcome::None);
        // Mode unchanged.
        assert_eq!(m.search_mode, SearchMode::Fuzzy);
    }

    #[test]
    fn fuzzy_path_typing_writes_to_query_not_path() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.handle(&key(KeyCode::Char('c')), &b(), &mut local());
        m.handle(&key(KeyCode::Char('m')), &b(), &mut local());
        assert_eq!(m.fuzzy_query, "cm");
        assert!(
            m.path.is_empty(),
            "Precise path field must stay clean in Fuzzy mode (got {:?})",
            m.path,
        );
    }

    /// Typing `/` against an empty fuzzy query is a "drop into
    /// navigation at root" gesture: switch to Precise and seed `/`
    /// in the path field. Preserves `user_search_mode` so the next
    /// modal open returns to Fuzzy.
    #[test]
    fn slash_in_fuzzy_with_empty_query_enters_navigation_at_root() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.user_search_mode = SearchMode::Fuzzy;
        let outcome = m.handle(&key(KeyCode::Char('/')), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.search_mode, SearchMode::Precise);
        // user_search_mode is the persisted preference — auto-switch
        // is one-shot, so it must NOT change.
        assert_eq!(m.user_search_mode, SearchMode::Fuzzy);
        assert_eq!(m.path, "/");
        assert!(m.fuzzy_query.is_empty());
    }

    /// Typing `~` against an empty fuzzy query enters navigation at
    /// the user's home. The seed expands `$HOME` (with a trailing
    /// `/` so the wildmenu lists the home dir's children).
    #[test]
    fn tilde_in_fuzzy_with_empty_query_enters_navigation_at_home() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.user_search_mode = SearchMode::Fuzzy;
        let outcome = m.handle(&key(KeyCode::Char('~')), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.search_mode, SearchMode::Precise);
        assert_eq!(m.user_search_mode, SearchMode::Fuzzy);
        // The seed should be either `$HOME/` (when HOME is set,
        // which is the common case in CI / dev) or the literal
        // `~/` fallback. Either way, it's a directory-style path
        // ending in `/`. Test environments almost always have HOME
        // set; if not, the fallback is also acceptable.
        assert!(
            m.path.ends_with('/'),
            "tilde seed must end in '/': {}",
            m.path,
        );
        let expected_home = std::env::var_os("HOME").map_or_else(
            || "~/".to_string(),
            |h| format!("{}/", h.to_string_lossy().trim_end_matches('/')),
        );
        assert_eq!(m.path, expected_home);
        assert!(m.fuzzy_query.is_empty());
    }

    /// `/` typed mid-query should NOT trigger the auto-switch — the
    /// user already has a fuzzy search in progress and a stray `/`
    /// shouldn't blow it away. Falls through to the normal Char
    /// handler (appended to `fuzzy_query`).
    #[test]
    fn slash_after_fuzzy_query_stays_in_fuzzy() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.handle(&key(KeyCode::Char('a')), &b(), &mut local());
        let outcome = m.handle(&key(KeyCode::Char('/')), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.search_mode, SearchMode::Fuzzy);
        assert_eq!(m.fuzzy_query, "a/");
        assert!(m.path.is_empty());
    }

    /// In Precise mode, `/` and `~` are normal characters that go
    /// into the path field as-is. The auto-switch is a Fuzzy-only
    /// behavior because Precise users are already navigating by path.
    #[test]
    fn slash_in_precise_is_a_literal_char() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Precise;
        m.path.clear();
        m.handle(&key(KeyCode::Char('/')), &b(), &mut local());
        assert_eq!(m.search_mode, SearchMode::Precise);
        assert_eq!(m.path, "/");
    }

    /// `/` typed in the host zone is a literal char (host names
    /// don't contain `/`, but the modal shouldn't silently swap
    /// modes from under the host picker either). The Fuzzy gate
    /// already guards on `Zone::Path`, but pinning the host case
    /// makes the contract explicit.
    #[test]
    fn slash_in_host_zone_is_a_literal_char_not_an_auto_switch() {
        let mut m = mb("", "", Zone::Host, &["alpha"]);
        m.search_mode = SearchMode::Fuzzy;
        m.handle(&key(KeyCode::Char('/')), &b(), &mut local());
        assert_eq!(m.host, "/");
        assert_eq!(m.focused, Zone::Host);
        assert_eq!(m.search_mode, SearchMode::Fuzzy);
    }

    /// In `PathMode::Remote`, `~` should expand to the remote
    /// `$HOME` captured during prepare, not the local `$HOME`. The
    /// local laptop's home is irrelevant on the remote box.
    #[test]
    fn tilde_in_fuzzy_remote_mode_expands_to_remote_home() {
        let mut m = mb("devpod-go", "", Zone::Path, &["devpod-go"]);
        m.search_mode = SearchMode::Fuzzy;
        m.user_search_mode = SearchMode::Fuzzy;
        m.path_mode = PathMode::Remote {
            remote_home: PathBuf::from("/users/df"),
            cache: HashMap::new(),
        };
        m.handle(&key(KeyCode::Char('~')), &b(), &mut local());
        assert_eq!(m.search_mode, SearchMode::Precise);
        assert_eq!(m.path, "/users/df/");
    }

    #[test]
    fn notify_index_state_is_no_op_outside_fuzzy_path() {
        // In Host zone, even if mode is Fuzzy, notify should not touch
        // filtered (which holds host candidates from `refresh`).
        let mut m = mb("dev", "", Zone::Host, &["devpod-web"]);
        m.search_mode = SearchMode::Fuzzy;
        let before = m.filtered.clone();
        let idx = ready_index_plain(&["/some/where"]);
        m.notify_index_state(Some(&idx));
        assert_eq!(m.filtered, before, "host wildmenu must not be clobbered");
    }

    #[test]
    fn refresh_fuzzy_reads_from_refreshing_dirs() {
        // The whole point of SWR: while the background rebuild is in
        // flight, the modal must keep serving results from the cached
        // `dirs` carried by the `Refreshing` variant. Without the
        // `cached_dirs` extension, the matcher would see "no usable
        // index" and the wildmenu would go blank between rebuilds.
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.fuzzy_query = "code".to_string();
        let idx = refreshing_index_plain(&["/home/df/code-utils", "/home/df/Workbench/codemux"]);
        m.refresh_fuzzy(Some(&idx));
        assert!(
            !m.filtered.is_empty(),
            "Refreshing dirs must be queryable: {:?}",
            m.filtered,
        );
    }

    // ── active_host_key ──────────────────────────────────────────
    //
    // The runtime calls this every frame to look up the right
    // per-host index out of IndexCatalog. Wrong answer = the modal
    // shows local results when the user is targeting an SSH host (or
    // vice versa).

    #[test]
    fn active_host_key_returns_local_for_local_mode() {
        let m = mb("", "", Zone::Path, &[]);
        assert_eq!(m.active_host_key(), HOST_PLACEHOLDER);
    }

    #[test]
    fn active_host_key_returns_host_for_remote_mode() {
        let mut m = mb("devpod-web", "", Zone::Host, &["devpod-web"]);
        m.unlock_for_remote_path(
            "devpod-web".to_string(),
            PathBuf::from("/home/df"),
            &mut local(),
        );
        assert_eq!(m.active_host_key(), "devpod-web");
    }
}
