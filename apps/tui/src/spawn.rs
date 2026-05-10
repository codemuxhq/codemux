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
//!   with glob and `~/` expansion, so modular layouts like
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
use unicode_width::UnicodeWidthStr;

use crate::config::{NamedProject, SearchMode};
use crate::fuzzy_worker::FuzzyResult;
use crate::index_worker::{IndexState, IndexedDir, ProjectKind};
use crate::keymap::{ModalAction, ModalBindings};
use crate::ssh_config::load_ssh_hosts;

/// Total rows reserved at the bottom of the screen for the wildmenu
/// strip. The top row of those is the border, so the actual visible
/// candidate rows are `WILDMENU_ROWS - 1` (see [`wildmenu_view`] for
/// the use of the `usable` window). Tuned to comfortably show ~6
/// candidates without dominating the screen on a typical terminal —
/// scroll-into-view kicks in past that.
const WILDMENU_ROWS: u16 = 7;
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
    /// "indexing…" sentinel via the wildmenu render path once
    /// `cached_dirs()` returns empty for the rebuilding host.
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

/// What [`SpawnMinibuffer::fuzzy_dispatch_request`] hands back to the
/// runtime: a borrowed `(host, query)` pair the runtime turns into a
/// [`crate::fuzzy_worker::FuzzyControl::Query`] dispatch. Named struct
/// (vs a tuple) so the call site can't accidentally swap the two
/// `&str` fields.
#[derive(Debug, Eq, PartialEq)]
pub struct FuzzyDispatchRequest<'a> {
    pub host: &'a str,
    pub query: &'a str,
}

/// Which zone of the structured prompt is currently accepting input.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Zone {
    #[default]
    Path,
    Host,
}

/// Per-frame render context the wildmenu hands to each row builder.
/// Bundled to keep [`SpawnMinibuffer::wildmenu_row`] under clippy's
/// `too_many_arguments` cap; every field is constant for the lifetime
/// of one [`SpawnMinibuffer::wildmenu_view`] call.
#[derive(Clone, Copy, Debug)]
struct WildmenuRowContext {
    width: usize,
    zone: Zone,
    fuzzy: bool,
    precise_search: bool,
    stale: bool,
}

/// Saved-project metadata indexed off the candidate path string.
/// Carries the display name (always present) plus the SSH host alias
/// (`Some` only when the user explicitly bound the project to a
/// remote via `[[spawn.projects]] host = "alias"`). Single source of
/// truth for what [`SpawnMinibuffer::project_meta`] knows about a
/// candidate — `confirm` reads `host`, `wildmenu_row` reads both.
#[derive(Clone, Debug)]
struct NamedProjectMeta {
    name: String,
    host: Option<String>,
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
    /// the user left it). The runtime ships this to `nucleo-matcher`
    /// via the [`crate::fuzzy_worker`] background thread on every
    /// distinct value (see [`Self::fuzzy_dispatch_request`]).
    fuzzy_query: String,
    /// User-curated named projects from `[[spawn.projects]]`. Stashed
    /// at `open()` because they don't change mid-session. Scored by
    /// `nucleo-matcher` against `name` (not the full path) and
    /// boosted above any auto-discovered repository.
    named_projects: Vec<NamedProject>,
    /// Lookup table from `resolve_named_project_path(np)` → host alias,
    /// populated at `open()` from any `named_projects` entry whose
    /// `host` is `Some` and non-empty. Used at commit time in
    /// [`Self::confirm`] to upgrade a plain `Spawn` to
    /// `PrepareHostThenSpawn` when the picked path matches an alias
    /// bound to a remote host.
    ///
    /// Per-project metadata indexed by the candidate path string
    /// `score_fuzzy` emits — `resolve_named_project_path(np)` for both
    /// local and host-bound entries (see [`build_project_meta`]). One
    /// map covers every saved-project lookup the modal needs:
    ///
    /// * commit-time host upgrade (`Self::confirm` consults `host` to
    ///   route a path to `PrepareHostThenSpawn` when bound to an SSH
    ///   alias); `host: None` is the local default and emits a plain
    ///   `Spawn`.
    /// * render-time row swap (`Self::wildmenu_row` reads `name` to
    ///   build the `★ name … ~/path` row, and `host` to render the
    ///   `@host` badge).
    ///
    /// Host-bound entries are keyed by the *literal* `np.path` (no
    /// local tilde expansion) — `~/` resolves on the remote against
    /// `prepared.remote_home` during attach, not on the laptop. Local
    /// expansion would send a `/Users/...` path to a Linux remote and
    /// fail the daemon's `cwd.exists()` check. The wildmenu candidate
    /// emitted by `score_fuzzy` is the same literal so the lookup
    /// hits.
    project_meta: HashMap<String, NamedProjectMeta>,
    /// `true` when `filtered` may not reflect the current
    /// `fuzzy_query` because a fresh fuzzy search has been kicked off
    /// (background worker in `crate::fuzzy_worker`) but its result
    /// hasn't landed yet. Set on every keystroke that mutates
    /// `fuzzy_query`; cleared by [`Self::set_fuzzy_results`] when a
    /// matching result arrives. Drives the dim modifier in the
    /// wildmenu so the user can see results are "in flight" without
    /// the menu blanking. No effect outside Fuzzy + Path mode.
    filtered_stale: bool,
    /// Set to `true` for the duration of one frame after Tab/Enter
    /// descends into a folder via [`Self::apply_path_completion`].
    /// Drives a momentary visual: the freshly-picked leaf folder
    /// renders in cyan/bold WITHOUT its trailing `/`, confirming
    /// the pick. Cleared at the top of the next [`Self::handle`]
    /// call so the very next arrow / keystroke flips back to the
    /// usual nav-mode rendering (default style, slash visible).
    just_descended: bool,
    /// Set when an `enter_navigation_mode_with_seed('~', _)` was
    /// triggered by a tilde *variant* (combining tilde U+0303 or
    /// modifier-letter small tilde U+02DC) — i.e. the OS / terminal
    /// did not compose a literal `~` from the user's intl-layout
    /// dead key. The very next Char event is almost always the
    /// composing space; we swallow it so the user doesn't end up
    /// with `~/ ` (a literal space appended to the seeded home).
    /// Cleared at the top of the next [`Self::handle`] call so a
    /// later space has no special meaning.
    tilde_compose_armed: bool,
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
        let project_meta = build_project_meta(&named_projects);
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
            project_meta,
            filtered_stale: false,
            just_descended: false,
            tilde_compose_armed: false,
        };
        if default_mode == SearchMode::Precise {
            m.seed_path_with_cwd();
        }
        // Initial wildmenu lists the cwd's subfolders in Precise mode
        // (filtered to directories only — files are not valid spawn
        // targets). In Fuzzy mode `refresh` short-circuits via the
        // path-zone fuzzy guard at the top; the wildmenu shows the
        // index-state sentinel until the runtime drains the first
        // [`crate::fuzzy_worker::FuzzyResult`] and the modal applies
        // it via [`Self::set_fuzzy_results`].
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

        // One-frame visual flags. `just_descended` clears here so any
        // keystroke flips the prompt back to default rendering;
        // `apply_path_completion` re-sets it on the way out for the
        // descend gesture itself. `tilde_compose_armed` is captured
        // and cleared so the Char arm below can read it once.
        self.just_descended = false;
        let tilde_armed_before = self.tilde_compose_armed;
        self.tilde_compose_armed = false;

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
                // Intl dead-key compose: when the previous Char was
                // a tilde *variant* (combining or modifier-letter
                // tilde — produced by uncomposed dead-key input),
                // the OS / terminal commonly delivers the composing
                // space as the next event. Swallow it so the user
                // who just typed `~ + space` to mean a literal `~`
                // doesn't end up with a stray space appended to the
                // seeded `~/`. Cleared above; one-shot.
                if tilde_armed_before && c == ' ' {
                    return ModalOutcome::None;
                }
                // Auto-enter navigation: typing `/` or `~` against a
                // fresh path field is the "I want to navigate by
                // path" gesture. Switch to Precise mode and seed the
                // path at root (`/`) or the (local or remote) `$HOME`
                // (`~`). The user's preferred mode is preserved
                // (`user_search_mode` is not touched).
                //
                // "Fresh" depends on the active engine:
                //   - Fuzzy: the typed query is empty.
                //   - Precise: the path is empty OR still the auto-
                //     seeded cwd (the modal just opened, the user
                //     hasn't edited). User-typed paths preserve `~`
                //     as a literal char so power users can build a
                //     path containing `~` if they really want to.
                //
                // Tilde recognition is broadened to the unicode
                // variants intl layouts emit when dead-key composition
                // doesn't fire (combining tilde U+0303, modifier-letter
                // small tilde U+02DC). On hit, we also arm
                // `tilde_compose_armed` so the next-event space (the
                // composing key) gets swallowed instead of polluting
                // the seeded path.
                let is_tilde_variant = matches!(c, '~' | '\u{02DC}' | '\u{0303}');
                let path_is_initial = match self.search_mode {
                    SearchMode::Fuzzy => self.fuzzy_query.is_empty(),
                    SearchMode::Precise => {
                        self.path.is_empty() || self.path_origin == PathOrigin::AutoSeeded
                    }
                };
                if self.focused == Zone::Path && path_is_initial && (c == '/' || is_tilde_variant) {
                    let trigger = if c == '/' { '/' } else { '~' };
                    self.enter_navigation_mode_with_seed(trigger, lister);
                    if c != '~' && is_tilde_variant {
                        self.tilde_compose_armed = true;
                    }
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
    /// to nothing).
    ///
    /// Esc in the path zone (Precise mode) is "back out of
    /// search/selection": if a wildmenu row is highlighted OR the
    /// path has characters typed after the last `/` (search mode),
    /// one Esc clears both — selection drops, filter chars truncate
    /// to the last `/` so the wildmenu falls back to nav mode at the
    /// current dir. A second Esc (with nothing left to clear) closes
    /// the modal. In Fuzzy mode and the no-selection-no-filter case,
    /// Esc closes immediately.
    fn cancel(&mut self, lister: &mut DirLister<'_>) -> ModalOutcome {
        if self.focused == Zone::Host {
            self.transition_to_path_zone(lister);
            return ModalOutcome::None;
        }
        if self.search_mode == SearchMode::Precise {
            let has_filter = !self.path.is_empty() && !self.path.ends_with('/');
            if self.selected.is_some() || has_filter {
                if has_filter {
                    let truncate_to = self.path.rfind('/').map_or(0, |i| i + 1);
                    self.path.truncate(truncate_to);
                    self.path_origin = PathOrigin::UserTyped;
                }
                self.selected = None;
                self.refresh(lister);
                return ModalOutcome::None;
            }
        }
        ModalOutcome::Cancel
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

    /// Tab/Enter in the path zone applies the highlighted wildmenu
    /// candidate as a one-step descend: the candidate (which already
    /// carries its trailing `/` from [`path_completions`]) becomes
    /// the new path, the selection is cleared, and the next refresh
    /// lists the new directory's children with no preselected row —
    /// the user is now navigating inside the picked folder. From
    /// here, typing chars enters search mode (auto-selects the first
    /// match); pressing Enter again with no selection spawns at the
    /// current path.
    ///
    /// No-op when nothing is selected — the user can hit Down to
    /// start cycling, or just type a prefix which auto-highlights
    /// the first match.
    fn apply_path_completion(&mut self, lister: &mut DirLister<'_>) {
        if let Some(idx) = self.selected
            && let Some(candidate) = self.filtered.get(idx).cloned()
        {
            self.path = candidate;
            self.selected = None;
            // Tab/Enter-applying a completion is an explicit user
            // choice; the seeded cwd is no longer the literal value
            // in the field, so the auto-seeded marker no longer
            // applies.
            self.path_origin = PathOrigin::UserTyped;
            // Momentary "you just landed at this folder" cue. The
            // top of `handle` clears this flag, so the next arrow
            // / keystroke flips the prompt back to the usual nav
            // rendering (default style, slash visible).
            self.just_descended = true;
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

        // Path zone + Precise mode + a highlighted wildmenu pick:
        // descend into the picked folder, don't spawn. The user must
        // press Enter again (with no selection) to commit at the
        // current path. Decoupling navigate-from-spawn this way means
        // a confident Tab-then-Enter rhythm walks the tree instead
        // of silently committing at whatever level the user happened
        // to be when the autocomplete fired.
        //
        // Fuzzy mode keeps the historical "Enter applies the
        // highlight then spawns" semantics — there's no concept of
        // "descend into" a fuzzy hit.
        if self.focused == Zone::Path
            && self.search_mode == SearchMode::Precise
            && self.selected.is_some()
        {
            self.apply_path_completion(lister);
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
        // zone. Local-only entries (host: None) skip this branch and
        // fall through to the plain `Spawn` below.
        if let Some(project_host) = self.project_meta.get(&path).and_then(|m| m.host.as_deref()) {
            return ModalOutcome::PrepareHostThenSpawn {
                host: project_host.to_string(),
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
        // Fuzzy mode owns its own wildmenu lifecycle: results come
        // from the background [`crate::fuzzy_worker`] (kicked off by
        // the runtime when it sees `fuzzy_dispatch_request` change),
        // not the per-keystroke `read_dir` below. Hand off to
        // `mark_fuzzy_stale` so the empty-query branch clears
        // synchronously and non-empty queries are marked stale until
        // the worker's matching result lands.
        if self.search_mode == SearchMode::Fuzzy && self.focused == Zone::Path {
            self.mark_fuzzy_stale();
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
        //
        // Precise + Path also treats "input ends with `/`" as nav mode:
        // the wildmenu shows the children of that directory but no row
        // is preselected, because the user hasn't started narrowing yet.
        // Selection only auto-arms when chars are typed AFTER the last
        // `/` (search mode). Down/Up still arms a selection manually.
        let nav_mode_no_select = self.focused == Zone::Path
            && self.search_mode == SearchMode::Precise
            && self.path.ends_with('/');
        self.selected =
            if self.filtered.is_empty() || self.current_field().is_empty() || nav_mode_no_select {
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
        // Precise mode is repopulated by `refresh`; Fuzzy mode marks
        // the wildmenu stale and waits for the worker dispatch on the
        // next runtime tick.
        self.filtered.clear();
        self.selected = None;
        self.filtered_stale = false;
        self.refresh(lister);
        tracing::trace!(mode = ?next, "spawn modal: search mode toggled");
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

    /// Reset the fuzzy wildmenu after the user changed the query.
    /// Empty query → clear synchronously (no worker round-trip
    /// makes sense). Non-empty → mark `filtered` stale so the
    /// renderer dims it and the runtime knows to dispatch a fresh
    /// scoring request to [`crate::fuzzy_worker`].
    ///
    /// Public so tests can drive the empty-query / stale paths
    /// without standing up a worker. No-op outside Fuzzy + Path mode.
    pub fn mark_fuzzy_stale(&mut self) {
        if self.search_mode != SearchMode::Fuzzy || self.focused != Zone::Path {
            return;
        }
        // Empty query: clear synchronously so the wildmenu doesn't
        // hold ranked hits from a previous query (and so Enter doesn't
        // commit a stale highlighted candidate). No worker dispatch
        // needed — the runtime short-circuits before sending.
        if self.fuzzy_query.is_empty() {
            self.filtered.clear();
            self.selected = None;
            self.filtered_stale = false;
            return;
        }
        // Non-empty: the previously rendered hits stay visible (the
        // wildmenu will dim them via `filtered_stale`) until the
        // worker's matching result lands and `set_fuzzy_results`
        // replaces them.
        self.filtered_stale = true;
    }

    /// Apply a [`FuzzyResult`] to the modal's wildmenu. Drops results
    /// whose `(host, query)` tag doesn't match the modal's current
    /// `(active_host_key, fuzzy_query)` — the natural race when the
    /// user types fast: an in-flight `c` result lands after the
    /// modal has already moved on to `co`.
    ///
    /// On match, replaces `filtered` and clears the stale flag.
    /// Selection is preserved by candidate identity (the path string)
    /// across re-scores: if the previously-selected path is still in
    /// the new hits, selection follows it to its new index. This
    /// matters during background indexing, where the runtime triggers
    /// a re-score on every batch of newly-indexed dirs (each batch
    /// bumps the per-host index generation, which clears the
    /// last-pushed-query memo in `tick_fuzzy_dispatch`). Without
    /// identity preservation, every batch would clobber the user's
    /// Down/Up pick — the symptom is "I press Down and selection
    /// snaps back to the top while the indexer is still working."
    ///
    /// Falls back to `Some(0)` when there was no prior selection or
    /// the previously-selected path dropped out of the new hits, so
    /// the auto-arm-first-hit autocomplete UX still applies on the
    /// initial result for a fresh query. Empty hits clear `selected`
    /// so Enter doesn't commit a non-candidate.
    pub fn set_fuzzy_results(&mut self, result: FuzzyResult) {
        if result.host != self.active_host_key() {
            tracing::trace!(
                got = ?result.host,
                want = ?self.active_host_key(),
                "fuzzy result: host mismatch, dropping"
            );
            return;
        }
        if result.query != self.fuzzy_query {
            tracing::trace!(
                got = ?result.query,
                want = ?self.fuzzy_query,
                "fuzzy result: query superseded, dropping"
            );
            return;
        }
        let prior_selected_path = self.selected.and_then(|i| self.filtered.get(i)).cloned();
        self.filtered = result.hits;
        self.selected = if self.filtered.is_empty() {
            None
        } else if let Some(path) = prior_selected_path
            && let Some(new_idx) = self.filtered.iter().position(|p| p == &path)
        {
            Some(new_idx)
        } else {
            Some(0)
        };
        self.filtered_stale = false;
    }

    /// What the runtime needs to dispatch to [`crate::fuzzy_worker`].
    /// Returns `Some(FuzzyDispatchRequest)` when the modal is in
    /// Fuzzy + Path mode AND the query is non-empty. The runtime
    /// memoizes on the returned `query` (last-pushed-per-host map)
    /// so the worker only sees one dispatch per distinct query string.
    #[must_use]
    pub fn fuzzy_dispatch_request(&self) -> Option<FuzzyDispatchRequest<'_>> {
        if self.search_mode != SearchMode::Fuzzy || self.focused != Zone::Path {
            return None;
        }
        if self.fuzzy_query.is_empty() {
            return None;
        }
        Some(FuzzyDispatchRequest {
            host: self.active_host_key(),
            query: self.fuzzy_query.as_str(),
        })
    }

    /// Borrow the user's named-project list. The fuzzy worker scores
    /// against this alongside the indexed dirs, and the runtime
    /// hands it through inside [`crate::fuzzy_worker::FuzzyControl::SetIndex`].
    /// Set at [`Self::open`] and never mutated mid-modal.
    #[must_use]
    pub fn named_projects(&self) -> &[NamedProject] {
        &self.named_projects
    }

    /// Test-only seam: set the fuzzy query string without driving a
    /// real keystroke through `handle`. The runtime's
    /// [`crate::runtime::tick_fuzzy_dispatch`] tests use this to
    /// arrange a known modal state before exercising the dispatch.
    #[cfg(test)]
    pub(crate) fn set_fuzzy_query_for_test(&mut self, query: &str) {
        self.fuzzy_query = query.to_string();
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

        // Position the real terminal cursor at the prompt's input
        // position so OS dead-key previews (e.g. the small `~` an
        // intl layout draws while waiting for the composing space)
        // land where the user is typing — not at the rightmost edge
        // of the prompt line, where ratatui would otherwise leave
        // the cursor after the hint span. Skipped while the path
        // zone is locked for bootstrap (no input is accepted there).
        if self.bootstrap_view.is_none() {
            let offset = self.prompt_cursor_offset();
            let cursor_x = prompt_area.x.saturating_add(offset).min(
                prompt_area
                    .x
                    .saturating_add(prompt_area.width)
                    .saturating_sub(1),
            );
            frame.set_cursor_position((cursor_x, prompt_area.y));
        }
    }

    /// Column offset of the input cursor within the prompt row,
    /// measured from the row's start. Mirrors the span ordering in
    /// [`Self::prompt_view`]: `label` + `@` + `host_zone` + ` : ` +
    /// `path_zone`, with the cursor sitting at the end of whichever
    /// zone is focused (or at the first placeholder char when that
    /// zone is empty — matching `zone_spans`'s overlay behavior).
    ///
    /// Used by [`Self::render`] to drive `frame.set_cursor_position`
    /// so dead-key previews land at the input position.
    fn prompt_cursor_offset(&self) -> u16 {
        // Fixed prefix: "spawn: " / "find:  " (both 7 ASCII bytes /
        // chars) followed by `@` (1).
        let mut x: u16 = 7 + 1;

        // Host zone width: cursor on first placeholder char when
        // focused-and-empty; cursor at end of value otherwise.
        if self.focused == Zone::Host && self.host.is_empty() {
            return x;
        }
        let host_width = if self.host.is_empty() {
            u16::try_from(HOST_PLACEHOLDER.chars().count()).unwrap_or(u16::MAX)
        } else {
            u16::try_from(self.host.chars().count()).unwrap_or(u16::MAX)
        };
        x = x.saturating_add(host_width);
        if self.focused == Zone::Host {
            return x;
        }

        // Separator ` : ` (3 chars).
        x = x.saturating_add(3);

        // Path zone. Honors the same `just_descended` trim as
        // `prompt_view` so the cursor lands at the visible
        // (slash-stripped) end of the path that frame.
        let path_text = if self.search_mode == SearchMode::Fuzzy && self.focused == Zone::Path {
            self.fuzzy_query.as_str()
        } else if self.just_descended && self.focused == Zone::Path && self.path.ends_with('/') {
            &self.path[..self.path.len() - 1]
        } else {
            self.path.as_str()
        };
        if path_text.is_empty() && self.focused == Zone::Path {
            return x;
        }
        let path_width = u16::try_from(path_text.chars().count()).unwrap_or(u16::MAX);
        x.saturating_add(path_width)
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
        // While the fuzzy worker is computing fresh hits for a
        // just-changed query, the previous query's hits stay visible
        // but dimmed so the user has a visual cue that results are
        // in flight. The selected row keeps its highlight (cyan bg)
        // so arrow/Enter stays unambiguous. Only relevant in Fuzzy
        // + Path mode — `filtered_stale` is never set otherwise.
        let stale = fuzzy && zone == Zone::Path && self.filtered_stale;
        // Precise + Path uses search-mode rendering (full candidate
        // path, parent dim + leaf cyan/bold) when the user has
        // typed chars after the last `/`. Nav mode (path ends `/`
        // or empty) keeps the basename-only rendering — the parent
        // context already lives in the prompt above.
        let precise_search =
            !fuzzy && zone == Zone::Path && !self.path.is_empty() && !self.path.ends_with('/');
        // Scroll-into-view: when the selection is past the bottom of
        // the visible window, slide the start so the selected row sits
        // at the last visible position. `enumerate` runs BEFORE `skip`
        // so each row's `i` is its absolute index in `self.filtered`,
        // which keeps the `is_selected` check correct.
        let scroll = wildmenu_scroll_offset(self.selected, usable);
        let ctx = WildmenuRowContext {
            width,
            zone,
            fuzzy,
            precise_search,
            stale,
        };
        let lines: Vec<Line> = self
            .filtered
            .iter()
            .enumerate()
            .skip(scroll)
            .take(usable)
            .map(|(i, c)| self.wildmenu_row(c, i, &ctx))
            .collect();
        Paragraph::new(lines).block(block)
    }

    /// Build a single wildmenu row. Extracted from [`Self::wildmenu_view`]
    /// purely to keep that function under clippy's `too_many_lines`
    /// threshold; the per-row branching (precise-search / saved-project /
    /// generic) lives here so the caller stays focused on the framing
    /// (block, scroll math, sentinel branches).
    fn wildmenu_row(&self, candidate: &str, i: usize, ctx: &WildmenuRowContext) -> Line<'static> {
        let is_selected = Some(i) == self.selected;
        if ctx.precise_search {
            return precise_search_row(candidate, is_selected, ctx.width);
        }
        // Saved-project row: only meaningful in fuzzy + path mode
        // (named projects are matched via `score_fuzzy`, which only
        // runs in that mode). Nav-mode candidates are always
        // indexed-dir paths, never named-project candidates, so the
        // lookup would miss anyway.
        if ctx.fuzzy
            && ctx.zone == Zone::Path
            && let Some(meta) = self.project_meta.get(candidate)
        {
            let mut row = named_project_row(
                &meta.name,
                candidate,
                meta.host.as_deref(),
                is_selected,
                ctx.width,
            );
            if ctx.stale && !is_selected {
                row = row.patch_style(Style::default().add_modifier(Modifier::DIM));
            }
            return row;
        }
        let marker = if is_selected { " ▸ " } else { "   " };
        let mut line_style = if is_selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        if ctx.stale && !is_selected {
            line_style = line_style.add_modifier(Modifier::DIM);
        }
        // In fuzzy mode the full path *is* the signal — basename alone
        // strips the directory context that the user is searching for.
        // Precise nav and Host modes keep the basename-or-literal
        // display.
        let display_str = if ctx.fuzzy && ctx.zone == Zone::Path {
            candidate.to_string()
        } else {
            wildmenu_display_text(ctx.zone, candidate)
        };
        let display = clip_middle(&display_str, ctx.width.saturating_sub(3));
        Line::styled(format!("{marker}{display}"), line_style)
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
        //
        // Just-descended override: for the one frame after Tab/Enter
        // descended into a folder, render the path WITHOUT its
        // trailing `/` by slicing one byte off the end (the trailing
        // `/` is always single-byte ASCII). `zone_spans` then splits
        // at the next-to-last `/` and shows the freshly-picked leaf
        // folder in cyan/bold, confirming the pick. `self.path`
        // keeps the slash so refresh continues to list children —
        // this is purely a render-time transform.
        let path_after_trim =
            if self.just_descended && self.focused == Zone::Path && self.path.ends_with('/') {
                &self.path[..self.path.len() - 1]
            } else {
                self.path.as_str()
            };
        let (path_text, path_placeholder) =
            if self.search_mode == SearchMode::Fuzzy && self.focused == Zone::Path {
                (self.fuzzy_query.as_str(), FUZZY_PLACEHOLDER)
            } else {
                (path_after_trim, PATH_PLACEHOLDER)
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

#[cfg(test)]
impl SpawnMinibuffer {
    /// Build a minibuffer with the given cwd and otherwise-default
    /// state, without invoking `open()`/`refresh()` (which would shell
    /// out to `read_dir` and be non-deterministic in tests).
    /// Encapsulates the field set so that adding a field doesn't break
    /// every test fixture.
    pub(crate) fn new_for_test(cwd: PathBuf) -> Self {
        Self {
            host: String::new(),
            path: String::new(),
            focused: Zone::Path,
            ssh_hosts: Vec::new(),
            filtered: Vec::new(),
            selected: None,
            path_mode: PathMode::Local,
            bootstrap_view: None,
            prepare_error: None,
            path_origin: PathOrigin::UserTyped,
            cwd,
            search_mode: SearchMode::Precise,
            user_search_mode: SearchMode::Precise,
            fuzzy_query: String::new(),
            named_projects: Vec::new(),
            project_meta: HashMap::new(),
            filtered_stale: false,
            just_descended: false,
            tilde_compose_armed: false,
        }
    }
}

/// Build the spans for one zone of the prompt.
///
/// Placeholder semantics: when `value` is empty, render the full
/// `placeholder` in dim style. When focused, the real terminal
/// cursor — positioned by the caller via `frame.set_cursor_position`
/// — sits over the first placeholder character so OS dead-key
/// previews land at the right spot. We deliberately do NOT render a
/// `█` glyph here: doing so duplicated the cursor visually and (on
/// some terminals) the block drew over the placeholder's first
/// character, leaving e.g. `find>` instead of `<find>`.
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
///
/// `cursor_style` is currently unused (kept in the signature so call
/// sites don't churn while the cursor strategy is in flux); the
/// terminal cursor inherits whatever attrs the host emulator applies.
fn zone_spans<'a>(
    focused: bool,
    value: &'a str,
    placeholder: &'a str,
    placeholder_style: Style,
    _cursor_style: Style,
    highlight_basename: bool,
    unfocused_value_style: Style,
) -> Vec<Span<'a>> {
    let mut out = Vec::with_capacity(2);
    if value.is_empty() {
        out.push(Span::styled(placeholder, placeholder_style));
    } else if focused
        && highlight_basename
        && let Some(slash) = value.rfind('/')
    {
        // Path basename split: when focused on the path zone and the
        // value has at least one `/`, render the parent prefix
        // (including the trailing slash) in the default style and
        // only the trailing component in the focused style. `rfind`
        // returns a byte index; `/` is a single byte in UTF-8 so
        // `slash + 1` is always a valid char boundary regardless of
        // any multi-byte chars elsewhere in the path.
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
pub(crate) fn score_fuzzy(query: &str, dirs: &[IndexedDir], named: &[NamedProject]) -> Vec<String> {
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config, Matcher, Utf32Str};

    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();

    let mut scored: Vec<(u32, String)> = Vec::new();
    let mut named_paths: HashSet<String> = HashSet::new();

    // Named projects first — score the user-friendly `name`, but emit
    // the resolved `path` as the spawn target. For local projects that's
    // the tilde-expanded absolute path; for host-bound projects it's the
    // literal path so the bootstrap library can expand `~/` against the
    // remote `$HOME` (see `resolve_named_project_path`).
    for np in named {
        let haystack = Utf32Str::new(&np.name, &mut buf);
        if let Some(score) = pattern.score(haystack, &mut matcher) {
            let expanded = expand_named_project_path(&np.path);
            let candidate = if np.host.as_deref().is_some_and(|h| !h.is_empty()) {
                np.path.clone()
            } else {
                expanded.clone()
            };
            // Dedup against indexed dirs always uses the local-expanded
            // path: indexed dirs are absolute local paths, and a shared
            // dir like `~/.dotfiles` (present on both laptop and devpod)
            // should still drop its local entry from the wildmenu when a
            // host-bound named project covers it.
            named_paths.insert(expanded);
            scored.push((score.saturating_add(BOOST_NAMED), candidate));
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

/// The string the spawn modal emits for a named project — both as the
/// wildmenu candidate and as the spawn target the runtime hands to the
/// bootstrap worker.
///
/// Local projects (no `host`): tilde-expanded absolute path, matching
/// the indexer's emission so dedup with auto-discovered dirs works.
///
/// Host-bound projects (`host: Some(non_empty)`): the literal `np.path`
/// with `~/` preserved. The bootstrap library
/// (`codemuxd_bootstrap::expand_remote_tilde`) expands against the
/// captured remote `$HOME` during attach. Local-expanding here would
/// send a `/Users/...` path to a Linux remote, where the daemon's
/// `cwd.exists()` check fails before the supervisor binds — surfacing
/// to the user as `bootstrap of <host> failed: cwd /Users/... does not
/// exist`.
#[must_use]
fn resolve_named_project_path(np: &NamedProject) -> String {
    if np.host.as_deref().is_some_and(|h| !h.is_empty()) {
        return np.path.clone();
    }
    expand_named_project_path(&np.path)
}

/// Build the path → metadata lookup the spawn modal consults at
/// commit time (`Self::confirm` reads `host` to upgrade `Spawn` →
/// `PrepareHostThenSpawn`) and at render time (`Self::wildmenu_row`
/// reads `name` to render the `★ alias` row and `host` to render the
/// `@host` badge). Keys come from [`resolve_named_project_path`] so
/// they match the strings `score_fuzzy` emits into the wildmenu —
/// i.e. the locally-expanded path for local entries and the literal
/// `np.path` (with `~/` preserved) for host-bound entries.
///
/// Every named project gets an entry. `host` is `Some(non_empty)`
/// only when the user explicitly bound the project to an SSH alias;
/// `None` and `Some("")` both collapse to "local" so the commit-time
/// branch in `confirm` skips the host upgrade and emits a plain
/// `Spawn`.
///
/// On a duplicate path the *first* entry wins — the user's config
/// order is the tiebreaker. We don't bother warning; duplicate paths
/// in `[[spawn.projects]]` are already a config smell that the
/// existing dedup in `score_fuzzy` papers over.
#[must_use]
fn build_project_meta(projects: &[NamedProject]) -> HashMap<String, NamedProjectMeta> {
    let mut map = HashMap::new();
    for np in projects {
        let host = np.host.clone().filter(|h| !h.is_empty());
        map.entry(resolve_named_project_path(np))
            .or_insert_with(|| NamedProjectMeta {
                name: np.name.clone(),
                host,
            });
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

/// Compute how many leading rows to skip in the wildmenu so the
/// selected candidate stays visible inside a window of `usable`
/// rows. Returns 0 when no row is selected or when the selection is
/// already inside the first `usable` rows; otherwise slides the
/// start forward so the selected row lands at the last visible
/// position.
///
/// Works in tandem with `enumerate().skip(scroll).take(usable)` in
/// the wildmenu render — `enumerate` runs first so each row keeps
/// its absolute index for the `is_selected` check.
#[must_use]
fn wildmenu_scroll_offset(selected: Option<usize>, usable: usize) -> usize {
    let Some(sel) = selected else {
        return 0;
    };
    sel.saturating_sub(usable.saturating_sub(1))
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

/// Build a wildmenu row for a Precise + Path candidate while the
/// user is in *search mode* (typing chars after the last `/`). The
/// full candidate path is shown so the user can confirm which
/// directory the autocomplete is offering, with the parent prefix
/// dimmed and the leaf folder rendered in cyan/bold so the eye lands
/// on the part being filtered.
///
/// Selected rows skip the parent/leaf split and render as a single
/// span with the modal's cyan-bg highlight — uniform background
/// reads as "this is the active pick" without competing colors.
fn precise_search_row(candidate: &str, is_selected: bool, width: usize) -> Line<'static> {
    let marker = if is_selected { " ▸ " } else { "   " };
    let clipped = clip_middle(candidate, width.saturating_sub(3));
    if is_selected {
        let style = Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        return Line::styled(format!("{marker}{clipped}"), style);
    }
    // Strip the candidate's own trailing `/` (if any) before locating
    // the parent slash so the rsplit hits the leaf's parent — not the
    // dir's terminator — then re-attach the slash to the leaf.
    let stem = clipped.strip_suffix('/').unwrap_or(&clipped);
    let suffix = if clipped.ends_with('/') { "/" } else { "" };
    if let Some(slash) = stem.rfind('/') {
        let split = slash + 1;
        let (parent, leaf) = stem.split_at(split);
        Line::from(vec![
            Span::raw(marker.to_string()),
            Span::styled(
                parent.to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::styled(
                format!("{leaf}{suffix}"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    } else {
        Line::from(vec![
            Span::raw(marker.to_string()),
            Span::styled(
                clipped,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    }
}

/// Collapse a `$HOME`-prefixed absolute path to `~/...` for display.
/// Pure render-time transform — never feed the result back into the
/// spawn pipeline (the bootstrap library and daemon want absolute
/// paths for local hosts). When `home` is `None` or empty, or when
/// `path` doesn't share the prefix, returns the input unchanged.
///
/// Splitting the env read ([`tilde_collapse`]) from the pure
/// substitution ([`tilde_collapse_with_home`]) keeps the defensive
/// branches (HOME unset / empty) reachable from a unit test without
/// needing to mutate process env across test threads — which `std`
/// flags as unsafe in newer rustc.
#[must_use]
fn tilde_collapse_with_home(path: &str, home: Option<&str>) -> String {
    let Some(home_str) = home else {
        return path.to_string();
    };
    if home_str.is_empty() {
        return path.to_string();
    }
    if path == home_str {
        return "~".to_string();
    }
    if let Some(rest) = path
        .strip_prefix(home_str)
        .and_then(|r| r.strip_prefix('/'))
    {
        return format!("~/{rest}");
    }
    path.to_string()
}

/// Convenience wrapper that reads `$HOME` from the process env and
/// delegates to [`tilde_collapse_with_home`]. The wildmenu render
/// path uses this; tests for the env-aware logic call the underlying
/// helper directly with a synthetic home string.
#[must_use]
fn tilde_collapse(path: &str) -> String {
    let home = std::env::var_os("HOME");
    let home_str = home.as_ref().map(|h| h.to_string_lossy());
    tilde_collapse_with_home(path, home_str.as_deref())
}

/// Build a wildmenu row for a saved project (`[[spawn.projects]]`
/// entry). The leading `★` marks the row as user-curated (versus an
/// auto-discovered indexed dir); the project's `name` lands on the
/// left followed by an optional ` @host` badge for remote-bound
/// entries (greyed out so it reads as secondary metadata next to the
/// name). The resolved path is right-aligned and dimmed so the eye
/// lands on the alias while the path stays available as confirmation.
/// Local saved projects omit the badge entirely — its presence alone
/// signals "this spawns on a remote".
///
/// Both selected and unselected rows share the same span layout; the
/// selection highlight is applied at the line level via `patch_style`
/// so the cyan-bg/black-fg/bold treatment paints the entire row
/// uniformly. Per-span DIM modifiers on the badge and path stay set,
/// which combines with the line-level BOLD on selection — terminals
/// render BOLD as the dominant signal so the highlight still reads.
///
/// Width budget: when the row doesn't fit, the path is the first
/// thing to lose space (middle-clipped via [`clip_middle`]); the
/// star, name, and host badge stay intact since they're the load-
/// bearing identifying signal. Variable-width components measure via
/// [`UnicodeWidthStr::width`] so a project name with a wide-glyph
/// character (CJK, emoji, combining marks) doesn't overflow the row;
/// constant components use literal column counts because their
/// glyphs are known to be single-cell (`★` U+2605, `▸` U+25B8).
#[must_use]
fn named_project_row(
    name: &str,
    path: &str,
    host: Option<&str>,
    is_selected: bool,
    width: usize,
) -> Line<'static> {
    let marker: &'static str = if is_selected { " ▸ " } else { "   " };
    let star: &'static str = "★ ";
    let badge = host.map(|h| format!(" @{h}")).unwrap_or_default();
    // Display path: tilde-collapse only when the candidate is an
    // absolute local path. Host-bound candidates already arrive as
    // literal `~/...` (per `resolve_named_project_path`) so a second
    // pass is a no-op there.
    let display_path = tilde_collapse(path);

    // Marker (3 cols) + star+space (2 cols) + variable-width name +
    // variable-width badge. The literals are fixed-width by
    // construction; everything user-supplied measures in terminal
    // columns.
    let used = 3 + 2 + UnicodeWidthStr::width(name) + UnicodeWidthStr::width(badge.as_str());
    // Reserve at least 2 spaces between name+badge and path; if we
    // can't afford that plus a single path character, drop the path
    // entirely so the name + badge stay legible.
    let path_budget = width.saturating_sub(used).saturating_sub(2);
    let (gap, path_text) = if path_budget == 0 {
        (String::new(), String::new())
    } else {
        let clipped = clip_middle(&display_path, path_budget);
        let gap_len = width
            .saturating_sub(used)
            .saturating_sub(UnicodeWidthStr::width(clipped.as_str()));
        (" ".repeat(gap_len), clipped)
    };

    let star_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().add_modifier(Modifier::DIM);

    let mut spans = vec![
        Span::raw(marker),
        Span::styled(star, star_style),
        Span::raw(name.to_string()),
    ];
    if !badge.is_empty() {
        spans.push(Span::styled(badge, dim));
    }
    spans.push(Span::raw(gap));
    spans.push(Span::styled(path_text, dim));

    let line = Line::from(spans);
    if is_selected {
        line.patch_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        line
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
            project_meta: HashMap::new(),
            filtered_stale: false,
            just_descended: false,
            tilde_compose_armed: false,
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

    /// Esc in the path zone closes the modal when there's nothing
    /// left to "back out of": no wildmenu selection, and the path is
    /// in nav mode (ends with `/` or is empty). The two-step
    /// "clear-search-then-close" behavior is covered by the
    /// `esc_clears_*` tests further down.
    #[test]
    fn esc_in_nav_mode_with_no_selection_closes_modal() {
        let mut m = mb("", "/tmp/", Zone::Path, &[]);
        m.selected = None;
        assert_eq!(
            m.handle(&key(KeyCode::Esc), &b(), &mut local()),
            ModalOutcome::Cancel
        );
    }

    /// Esc in the path zone with filter chars typed after the last
    /// `/` (search mode) clears them — truncating the path back to
    /// the parent dir — instead of closing the modal. A second Esc
    /// (now in nav mode with no selection) closes it.
    #[test]
    fn esc_in_search_mode_clears_filter_chars() {
        let mut m = mb("", "/tmp/al", Zone::Path, &["alpha", "beta"]);
        // mb() ran refresh; selection is auto-armed in search mode.
        m.selected = Some(0);
        let outcome = m.handle(&key(KeyCode::Esc), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.path, "/tmp/", "filter chars must be truncated");
        assert_eq!(m.selected, None, "selection must drop");
    }

    /// Esc in nav mode with only a selection (no filter chars)
    /// clears the selection and stays open. Useful when the user
    /// arrowed into the wildmenu to look around then changed their
    /// mind — Enter after this Esc spawns at the current dir.
    #[test]
    fn esc_in_nav_mode_with_selection_clears_selection() {
        let mut m = mb("", "/tmp/", Zone::Path, &[]);
        m.filtered = vec!["/tmp/alpha/".into(), "/tmp/beta/".into()];
        m.selected = Some(0);
        let outcome = m.handle(&key(KeyCode::Esc), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.path, "/tmp/", "path must be unchanged");
        assert_eq!(m.selected, None, "selection must drop");
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

    /// In Precise + Path nav mode (input ends `/`), refresh leaves
    /// selection unset even when the wildmenu has candidates — the
    /// user hasn't typed a filter, so we don't auto-arm a row.
    /// They still get one with the first Down/Up.
    #[test]
    fn refresh_in_precise_nav_mode_leaves_selection_unset() {
        let dir = tempfile::tempdir().unwrap();
        // `open()` runs an initial refresh against the cwd's children.
        let m = SpawnMinibuffer::open(dir.path(), SearchMode::Precise, Vec::new());
        assert!(m.path.ends_with('/'), "path must end with `/` (nav mode)");
        assert_eq!(m.selected, None, "no auto-armed selection in nav mode");
    }

    /// In Precise + Path search mode (chars after the last `/`),
    /// refresh auto-arms the first match so Enter immediately
    /// descends into the highlighted candidate. Uses a tempdir with
    /// a known child so `path_completions` returns a deterministic
    /// match.
    #[test]
    fn refresh_in_precise_search_mode_auto_arms_first_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("alpha")).unwrap();
        let mut m = SpawnMinibuffer::open(dir.path(), SearchMode::Precise, Vec::new());
        // Type one char to enter search mode (`/.../alp` → only `alpha/`).
        m.handle(&key(KeyCode::Char('a')), &b(), &mut local());
        assert!(
            !m.path.ends_with('/'),
            "path must have filter chars after `/` (search mode)"
        );
        assert_eq!(m.selected, Some(0), "first match must auto-arm");
    }

    /// Cursor offset when the path zone is focused and empty: the
    /// cursor sits on the first placeholder char, just after
    /// `label + @ + host_zone + " : "`. Verifies the offset accounts
    /// for the host zone's placeholder width even when host is empty.
    #[test]
    fn prompt_cursor_offset_path_focused_empty() {
        let m = mb("", "", Zone::Path, &[]);
        // "spawn: " (7) + "@" (1) + "local" (5 placeholder) + " : " (3) = 16
        assert_eq!(m.prompt_cursor_offset(), 16);
    }

    /// Path focused with typed value: cursor lands at the cell AFTER
    /// the value (no trailing-slash trim because `just_descended` is
    /// false on this fixture).
    #[test]
    fn prompt_cursor_offset_path_focused_with_value() {
        let m = mb("", "/tmp/al", Zone::Path, &[]);
        // 16 (prefix) + len("/tmp/al") = 23
        assert_eq!(m.prompt_cursor_offset(), 23);
    }

    /// Path focused with `just_descended` set and a trailing slash:
    /// cursor lands at the slash position (the rendered path is
    /// trimmed by one byte for visual confirmation).
    #[test]
    fn prompt_cursor_offset_just_descended_trims_trailing_slash() {
        let mut m = mb("", "/tmp/alpha/", Zone::Path, &[]);
        m.just_descended = true;
        // 16 (prefix) + len("/tmp/alpha") (no trailing /) = 26
        assert_eq!(m.prompt_cursor_offset(), 26);
    }

    /// Host zone focused, empty host: cursor sits on the first char
    /// of the `local` placeholder (offset 8 = "spawn: " + "@").
    #[test]
    fn prompt_cursor_offset_host_focused_empty() {
        let m = mb("", "", Zone::Host, &[]);
        assert_eq!(m.prompt_cursor_offset(), 8);
    }

    /// Host zone focused with typed value: cursor at end of host text.
    #[test]
    fn prompt_cursor_offset_host_focused_with_value() {
        let m = mb("alpha", "/tmp", Zone::Host, &[]);
        // 8 (label + @) + len("alpha") = 13
        assert_eq!(m.prompt_cursor_offset(), 13);
    }

    /// Tab-applying a wildmenu candidate is also a user choice; the
    /// resulting path is the candidate's value (full path with
    /// trailing slash, since Tab now descends in one step), not the
    /// auto-seeded cwd, so the marker is cleared.
    #[test]
    fn tab_completion_in_path_zone_clears_auto_seeded() {
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        m.path_origin = PathOrigin::AutoSeeded;
        m.filtered = vec!["/tmp/alpha/".into()];
        m.selected = Some(0);
        m.handle(&key(KeyCode::Tab), &b(), &mut local());
        assert_eq!(m.path, "/tmp/alpha/");
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
    /// field with the highlighted wildmenu candidate (full path
    /// including the trailing `/`) and stays in the path zone. Tab
    /// no longer crosses into the host zone — only `@` does.
    #[test]
    fn tab_in_path_zone_applies_highlighted_candidate() {
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        m.filtered = vec!["/tmp/alpha/".into(), "/tmp/beta/".into()];
        m.selected = Some(1);
        let outcome = m.handle(&key(KeyCode::Tab), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.focused, Zone::Path, "must stay in the path zone");
        assert_eq!(
            m.path, "/tmp/beta/",
            "field must reflect the picked candidate verbatim"
        );
    }

    /// Tab in the path zone with no selected candidate is a no-op:
    /// path text and focus are unchanged. The user can hit Down to
    /// start cycling, or just type a prefix which auto-highlights the
    /// first match.
    #[test]
    fn tab_in_path_zone_with_no_selection_is_noop() {
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        m.filtered = vec!["/tmp/alpha/".into()];
        m.selected = None;
        let outcome = m.handle(&key(KeyCode::Tab), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.focused, Zone::Path);
        assert_eq!(m.path, "/tmp", "field must be unchanged");
    }

    /// Tab on a folder candidate descends in one step: the
    /// candidate's full path (including trailing slash) becomes the
    /// new path, refresh lists the folder's children, and the
    /// selection drops so a follow-up Enter spawns at the new dir
    /// rather than walking deeper.
    #[test]
    fn tab_descends_into_folder_in_one_step() {
        let mut m = mb("", "/tmp/", Zone::Path, &[]);
        m.filtered = vec!["/tmp/alpha/".into(), "/tmp/beta/".into()];
        m.selected = Some(0);
        let outcome = m.handle(&key(KeyCode::Tab), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.path, "/tmp/alpha/");
        assert_eq!(m.selected, None, "selection must clear after descend");
    }

    /// Descending sets `just_descended` so the next render highlights
    /// the freshly-picked leaf folder without its trailing slash —
    /// momentary visual confirmation of the pick.
    #[test]
    fn descend_sets_just_descended_flag() {
        let mut m = mb("", "/tmp/", Zone::Path, &[]);
        m.filtered = vec!["/tmp/alpha/".into()];
        m.selected = Some(0);
        m.handle(&key(KeyCode::Tab), &b(), &mut local());
        assert!(
            m.just_descended,
            "descend must arm the post-descend visual flag",
        );
    }

    /// The `just_descended` flag is one-frame: any subsequent
    /// keystroke (arrow, type, backspace) clears it so the prompt
    /// reverts to the standard nav-mode rendering. Confirmed via
    /// `move_selection_forward` here; the same clear-at-top-of-handle
    /// applies to every other key path.
    #[test]
    fn next_keystroke_clears_just_descended() {
        let mut m = mb("", "/tmp/", Zone::Path, &[]);
        m.filtered = vec!["/tmp/alpha/".into(), "/tmp/beta/".into()];
        m.selected = Some(0);
        m.handle(&key(KeyCode::Tab), &b(), &mut local());
        assert!(m.just_descended);
        m.handle(&key(KeyCode::Down), &b(), &mut local());
        assert!(
            !m.just_descended,
            "any subsequent key must clear the visual flag",
        );
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

    /// A wildmenu pick in Precise mode beats the scratch fallback,
    /// but the meaning of "beats" changed: instead of spawning at
    /// the candidate, Enter descends into it. The user must press
    /// Enter again (with no selection) to commit. Either way, the
    /// scratch dir doesn't fire.
    #[test]
    fn highlighted_candidate_overrides_scratch_fallback() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.path_origin = PathOrigin::AutoSeeded;
        m.filtered = vec!["/tmp/alpha/".into()];
        m.selected = Some(0);
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None, "Enter descends, not spawn");
        assert_eq!(m.path, "/tmp/alpha/");
        assert_eq!(m.selected, None, "selection cleared after descend");
    }

    /// A wildmenu pick in Fuzzy mode keeps the historical
    /// "Enter applies the highlight then spawns" semantics — there's
    /// no concept of "descend into" a fuzzy hit.
    #[test]
    fn fuzzy_highlighted_candidate_spawns_on_enter() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.user_search_mode = SearchMode::Fuzzy;
        m.fuzzy_query = "alp".into();
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

    /// Enter in Precise + Path with a highlighted candidate descends
    /// into the picked folder rather than spawning there. A second
    /// Enter (with no selection) is what commits. This is the core
    /// of the navigate-vs-spawn split: a wildmenu pick is "I want to
    /// look inside this," not "I want to land here."
    #[test]
    fn enter_with_selection_in_precise_descends() {
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        m.filtered = vec!["/tmp/alpha/".into(), "/tmp/beta/".into()];
        m.selected = Some(1);
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.path, "/tmp/beta/");
        assert_eq!(m.selected, None);
    }

    /// Enter in Precise + Path with NO selection commits at the
    /// current path. Pairs with the descend-on-selection test above
    /// to define the two-step "Enter to descend, Enter again to
    /// spawn" rhythm.
    #[test]
    fn enter_without_selection_in_precise_spawns() {
        let mut m = mb("", "/tmp/alpha/", Zone::Path, &[]);
        m.path_origin = PathOrigin::UserTyped;
        m.filtered = vec![];
        m.selected = None;
        let outcome = m.handle(&key(KeyCode::Enter), &b(), &mut local());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "local".into(),
                path: "/tmp/alpha/".into(),
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

    /// Non-selected search-mode rows split into marker + parent
    /// (DIM) + leaf (cyan/bold), so the user's eye lands on the
    /// segment they're filtering for.
    #[test]
    fn precise_search_row_splits_parent_and_leaf_when_unselected() {
        let line = precise_search_row("/home/df/codemux/apps/", false, 80);
        assert_eq!(line.spans.len(), 3, "marker + parent + leaf");
        assert_eq!(line.spans[0].content, "   ");
        assert_eq!(line.spans[1].content, "/home/df/codemux/");
        assert!(
            line.spans[1].style.add_modifier.contains(Modifier::DIM),
            "parent prefix must be dimmed",
        );
        assert_eq!(line.spans[2].content, "apps/");
        assert_eq!(line.spans[2].style.fg, Some(Color::Cyan));
        assert!(line.spans[2].style.add_modifier.contains(Modifier::BOLD));
    }

    /// Scroll-into-view: when no row is selected, the wildmenu
    /// renders from the top — no scroll needed.
    #[test]
    fn wildmenu_scroll_offset_no_selection_is_zero() {
        assert_eq!(wildmenu_scroll_offset(None, 6), 0);
    }

    /// Scroll-into-view: a selection inside the first `usable`
    /// rows still renders from the top.
    #[test]
    fn wildmenu_scroll_offset_selection_within_window_is_zero() {
        // usable=6 → rows 0..5 visible at offset 0; sel=4 fits.
        assert_eq!(wildmenu_scroll_offset(Some(4), 6), 0);
        assert_eq!(wildmenu_scroll_offset(Some(5), 6), 0);
    }

    /// Scroll-into-view: a selection past the last visible row
    /// slides the start forward so the selected row sits at the
    /// last visible position.
    #[test]
    fn wildmenu_scroll_offset_slides_when_selection_below_window() {
        // usable=6 → window is 6 rows wide; sel=6 → start=1 (rows 1..6 visible).
        assert_eq!(wildmenu_scroll_offset(Some(6), 6), 1);
        assert_eq!(wildmenu_scroll_offset(Some(10), 6), 5);
    }

    /// Scroll-into-view: zero-width usable window degrades safely
    /// — saturating subtraction keeps offsets non-negative.
    #[test]
    fn wildmenu_scroll_offset_zero_usable_does_not_panic() {
        // `usable.saturating_sub(1)` is 0 for usable in {0, 1};
        // offset just becomes the selection index.
        assert_eq!(wildmenu_scroll_offset(Some(7), 0), 7);
        assert_eq!(wildmenu_scroll_offset(Some(7), 1), 7);
    }

    /// Selected rows render as a single highlighted span — the
    /// uniform cyan background is a stronger signal than a per-
    /// segment color split. Style sits at the Line level
    /// (`Line::styled`) so ratatui composes it across the row.
    #[test]
    fn precise_search_row_selected_uses_single_highlight_span() {
        let line = precise_search_row("/home/df/codemux/apps/", true, 80);
        assert_eq!(line.spans.len(), 1);
        assert!(line.spans[0].content.starts_with(" ▸ "));
        assert_eq!(line.style.bg, Some(Color::Cyan));
        assert_eq!(line.style.fg, Some(Color::Black));
        assert!(line.style.add_modifier.contains(Modifier::BOLD));
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
    /// Empty + focused: the full placeholder is rendered (no `█`
    /// glyph). The real terminal cursor — set by the caller via
    /// `frame.set_cursor_position` — sits on the first placeholder
    /// char so dead-key previews land at the input position. This
    /// replaces the previous `█ocal` rendering, which duplicated the
    /// cursor visually and obscured the placeholder's first char.
    #[test]
    fn zone_spans_empty_focused_renders_full_placeholder_dim() {
        let placeholder_style = Style::default().add_modifier(Modifier::DIM);
        let spans = zone_spans(
            true,
            "",
            "local",
            placeholder_style,
            Style::default(),
            false,
            path_unfocused_value_style(),
        );
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "local");
        assert!(spans[0].style.add_modifier.contains(Modifier::DIM));
        // Critically, the placeholder is NOT rendered in the focused
        // (cyan + bold) style — that would read as real input.
        assert!(!spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_ne!(spans[0].style.fg, Some(Color::Cyan));
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

    /// Non-empty + focused: just the value, no `█` glyph. Terminal
    /// cursor (set by the caller) lands at the cell after the value.
    #[test]
    fn zone_spans_non_empty_focused_emits_value_only() {
        let spans = zone_spans(
            true,
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
        // prefix + tail = 2 spans (no `█` cursor — terminal cursor handles it)
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "/home/df/");
        // Prefix is plain default — no fg, no BOLD.
        assert_eq!(spans[0].style, Style::default());
        assert_eq!(spans[1].content, "repos");
        // Tail uses the focused value style (cyan + bold).
        assert_eq!(spans[1].style.fg, Some(Color::Cyan));
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    /// A path that ends in `/` (e.g. just-seeded remote $HOME) has no
    /// trailing component — just the prefix span; terminal cursor
    /// sits at the cell after.
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
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "/home/df/");
        assert_eq!(spans[0].style, Style::default());
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
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "repos");
        assert_eq!(spans[0].style.fg, Some(Color::Cyan));
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
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
    fn mark_fuzzy_stale_with_non_empty_query_sets_the_flag() {
        // After the move to async fuzzy scoring (`crate::fuzzy_worker`),
        // `mark_fuzzy_stale` no longer scores synchronously — it just
        // tags the wildmenu as stale so the runtime knows to dispatch
        // a worker request and the renderer dims the previous results.
        // The actual hits arrive later via `set_fuzzy_results`.
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.fuzzy_query = "code".to_string();
        // `mb()` populated `filtered` via the Precise-mode refresh of
        // the test runner's cwd; clear it so this assertion checks
        // *only* whether `mark_fuzzy_stale` itself touched the field.
        m.filtered.clear();
        m.mark_fuzzy_stale();
        assert!(m.filtered_stale, "non-empty query must mark stale");
        // mark_fuzzy_stale must NOT touch `filtered` itself — that's
        // the worker's job. (If `filtered` had had stale hits from
        // the previous query, they'd be preserved here for the
        // dimmed overlay.)
        assert!(m.filtered.is_empty());
    }

    #[test]
    fn mark_fuzzy_stale_empty_query_clears_wildmenu() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.fuzzy_query.clear();
        m.filtered = vec!["stale".to_string()];
        m.selected = Some(0);
        m.filtered_stale = true;
        m.mark_fuzzy_stale();
        // Empty query is a synchronous clear so Enter doesn't commit
        // a leftover-highlighted candidate. Stale flag also clears
        // because there's nothing in flight to wait for.
        assert!(m.filtered.is_empty());
        assert_eq!(m.selected, None);
        assert!(!m.filtered_stale);
    }

    #[test]
    fn set_fuzzy_results_with_matching_tag_replaces_filtered() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.fuzzy_query = "code".to_string();
        m.filtered_stale = true;
        let result = FuzzyResult {
            host: HOST_PLACEHOLDER.to_string(),
            query: "code".to_string(),
            hits: vec![
                "/home/df/Workbench/repositories/codemux".to_string(),
                "/home/df/code-utils".to_string(),
            ],
        };
        m.set_fuzzy_results(result);
        assert_eq!(m.filtered.len(), 2);
        assert_eq!(m.selected, Some(0));
        assert!(!m.filtered_stale, "matched result must clear stale flag");
    }

    #[test]
    fn set_fuzzy_results_with_superseded_query_is_dropped() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.fuzzy_query = "code".to_string();
        m.filtered.clear();
        m.filtered_stale = true;
        // Result tagged with the prior query — the user has since
        // typed a superseding character. The modal must reject it so
        // the wildmenu doesn't briefly show "co"-shaped hits while
        // the user is staring at "code".
        let result = FuzzyResult {
            host: HOST_PLACEHOLDER.to_string(),
            query: "co".to_string(),
            hits: vec!["/anything".to_string()],
        };
        m.set_fuzzy_results(result);
        assert!(m.filtered.is_empty());
        assert!(m.filtered_stale, "stale flag must remain set");
    }

    #[test]
    fn set_fuzzy_results_with_wrong_host_is_dropped() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.fuzzy_query = "code".to_string();
        m.filtered.clear();
        // Result for a host other than the modal's `active_host_key()`
        // — happens when the user committed a host change before the
        // worker finished scoring the previous host's query.
        let result = FuzzyResult {
            host: "devpod-web".to_string(),
            query: "code".to_string(),
            hits: vec!["/srv/something".to_string()],
        };
        m.set_fuzzy_results(result);
        assert!(m.filtered.is_empty());
    }

    #[test]
    fn fuzzy_dispatch_request_returns_none_outside_fuzzy_path() {
        // Outside Fuzzy + Path mode, the runtime must not dispatch a
        // worker request — there's no fuzzy scoring to do.
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Precise;
        m.fuzzy_query = "code".to_string();
        assert!(m.fuzzy_dispatch_request().is_none());
        m.search_mode = SearchMode::Fuzzy;
        m.focused = Zone::Host;
        assert!(m.fuzzy_dispatch_request().is_none());
    }

    #[test]
    fn fuzzy_dispatch_request_returns_none_for_empty_query() {
        // Empty query short-circuits in `mark_fuzzy_stale` (synchronous
        // clear); the dispatch helper must agree so the runtime
        // doesn't wake the worker for a no-op.
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.fuzzy_query.clear();
        assert!(m.fuzzy_dispatch_request().is_none());
    }

    #[test]
    fn fuzzy_dispatch_request_returns_host_and_query() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.fuzzy_query = "code".to_string();
        let req = m.fuzzy_dispatch_request().unwrap();
        assert_eq!(req.host, HOST_PLACEHOLDER);
        assert_eq!(req.query, "code");
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

    // ── Project metadata lookup ─────────────────────────────────
    //
    // `build_project_meta` is the unified lookup the modal consults
    // for both commit-time host upgrades (`confirm` reads `host`) and
    // render-time row swaps (`wildmenu_row` reads `name` + `host`).
    // The unit tests below cover the construction (every named
    // project gets an entry; only non-empty `host` fields land in the
    // `host` slot; tilde-expansion matches the wildmenu's emitted
    // strings) and the emission path through `confirm` itself.

    #[test]
    fn build_project_meta_collapses_missing_or_empty_host_to_none() {
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
        let map = build_project_meta(&projects);
        let p = map.get("/local/p").unwrap();
        let q = map.get("/local/q").unwrap();
        assert_eq!(p.name, "local-only");
        assert!(p.host.is_none(), "missing host must collapse to None");
        assert_eq!(q.name, "explicit-empty");
        assert!(q.host.is_none(), "empty-string host must collapse to None");
    }

    #[test]
    fn build_project_meta_keys_by_literal_path_for_host_bound_entries() {
        // Host-bound entries are keyed by the literal `np.path` (NOT
        // local-expanded) so the lookup matches what `score_fuzzy`
        // emits into the wildmenu and what the runtime later hands to
        // the bootstrap library. The bootstrap library expands `~/`
        // against the *remote* `$HOME` during attach; pre-expanding
        // here would send a `/Users/...` path to a Linux remote and
        // fail the daemon's `cwd.exists()` check.
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
        let map = build_project_meta(&projects);
        assert_eq!(
            map.get("/work/code").and_then(|m| m.host.as_deref()),
            Some("devpod-1"),
        );
        assert_eq!(
            map.get("~/dotfiles").and_then(|m| m.host.as_deref()),
            Some("devpod-2"),
            "host-bound entries must key by the literal path, not the \
             local-expanded one — the remote daemon would never see \
             `/Users/...` as a valid cwd.",
        );
    }

    /// Regression for the `[[spawn.projects]] host = "devpod-go"` flow
    /// where every spawn died with `cwd /Users/.../workbench/... does
    /// not exist` on the remote. Captures the full path:
    ///
    ///   1. `score_fuzzy` emits the literal `~/...` candidate (not the
    ///      locally-expanded one).
    ///   2. `confirm()` looks up the candidate in `project_hosts` and
    ///      emits `PrepareHostThenSpawn { path: "~/..." }`.
    ///
    /// The bootstrap library's `expand_remote_tilde` then resolves the
    /// tilde against `prepared.remote_home` during attach.
    #[test]
    fn host_bound_tilde_project_round_trips_as_literal_path() {
        let np = NamedProject {
            name: "eatsfeed".to_string(),
            path: "~/workbench/repositories/go-code/eatsfeed".to_string(),
            host: Some("devpod-go".to_string()),
        };

        // (1) score_fuzzy emits the literal candidate.
        let result = score_fuzzy("eatsfeed", &[], std::slice::from_ref(&np));
        assert_eq!(
            result.first().map(String::as_str),
            Some("~/workbench/repositories/go-code/eatsfeed"),
            "host-bound project must surface its literal path in the \
             wildmenu so the lookup in confirm() hits and the remote \
             daemon receives a tilde-prefixed path it can expand: \
             {result:?}",
        );

        // (2) confirm() round-trips the literal candidate into a
        //     PrepareHostThenSpawn outcome with the same literal
        //     path. Fuzzy mode is the only place named projects
        //     actually surface in the wildmenu (score_fuzzy is the
        //     fuzzy worker's scoring function), so the test has to
        //     run in Fuzzy — Precise mode treats Enter+selection as
        //     "descend," which is the right behavior for directory
        //     completions but not for project picks.
        let mut m = SpawnMinibuffer::open(Path::new("/tmp"), SearchMode::Fuzzy, vec![np]);
        m.host.clear();
        m.fuzzy_query = "eatsfeed".to_string();
        m.filtered = vec!["~/workbench/repositories/go-code/eatsfeed".to_string()];
        m.selected = Some(0);
        m.focused = Zone::Path;
        let outcome = m.confirm(&mut DirLister::Local);
        assert_eq!(
            outcome,
            ModalOutcome::PrepareHostThenSpawn {
                host: "devpod-go".to_string(),
                path: "~/workbench/repositories/go-code/eatsfeed".to_string(),
            },
            "the literal `~/...` path must flow through to \
             PrepareHostThenSpawn unchanged",
        );
    }

    /// Local-only named projects keep today's behavior: their `~/`
    /// path is expanded against the local `$HOME` so the wildmenu
    /// candidate is an absolute local path that dedups against any
    /// indexed dir at the same place. Companion to the host-bound test
    /// above so the two arms of `resolve_named_project_path` are both
    /// covered through `score_fuzzy`.
    #[test]
    fn local_only_tilde_project_emits_locally_expanded_candidate() {
        let Some(home_os) = std::env::var_os("HOME") else {
            panic!("HOME unset; test cannot run in this env");
        };
        let home = PathBuf::from(home_os);
        let np = NamedProject {
            name: "df".to_string(),
            path: "~/.dotfiles".to_string(),
            host: None,
        };
        let result = score_fuzzy("df", &[], std::slice::from_ref(&np));
        assert_eq!(
            result.first().map(PathBuf::from),
            Some(home.join(".dotfiles")),
            "local-only project must surface the locally-expanded path: {result:?}",
        );
    }

    // ── Saved-project wildmenu rendering ─────────────────────────
    //
    // The wildmenu renders saved projects (`[[spawn.projects]]`) with
    // a `★ name  …  ~/path` row instead of the bare path that auto-
    // discovered indexed dirs use. The tilde-collapse helper and the
    // span layout produced by `named_project_row` are covered below
    // (the lookup that drives the swap is exercised through the
    // `build_project_meta_*` and `wildmenu_row_*` tests above).

    #[test]
    fn build_project_meta_carries_name_for_both_local_and_host_bound() {
        let projects = vec![
            NamedProject {
                name: "abs-local".to_string(),
                path: "/work/abs".to_string(),
                host: None,
            },
            NamedProject {
                name: "tilde-host-bound".to_string(),
                path: "~/projects/x".to_string(),
                host: Some("devpod-go".to_string()),
            },
        ];
        let map = build_project_meta(&projects);
        let local = map.get("/work/abs").unwrap();
        let remote = map.get("~/projects/x").unwrap();
        assert_eq!(local.name, "abs-local");
        assert!(local.host.is_none());
        assert_eq!(remote.name, "tilde-host-bound");
        assert_eq!(remote.host.as_deref(), Some("devpod-go"));
    }

    #[test]
    fn build_project_meta_first_wins_on_duplicate_path() {
        let projects = vec![
            NamedProject {
                name: "first".to_string(),
                path: "/work/dup".to_string(),
                host: None,
            },
            NamedProject {
                name: "second".to_string(),
                path: "/work/dup".to_string(),
                host: None,
            },
        ];
        let map = build_project_meta(&projects);
        assert_eq!(
            map.get("/work/dup").map(|m| m.name.as_str()),
            Some("first"),
            "config-order tiebreaker keeps the first matching entry: {map:?}",
        );
    }

    #[test]
    fn tilde_collapse_replaces_home_prefix() {
        let Some(home_os) = std::env::var_os("HOME") else {
            panic!("HOME unset; test cannot run in this env");
        };
        let home = home_os.to_string_lossy().into_owned();
        let path = format!("{home}/Workbench/codemux");
        assert_eq!(tilde_collapse(&path), "~/Workbench/codemux");
        assert_eq!(tilde_collapse(&home), "~");
    }

    #[test]
    fn tilde_collapse_passes_through_unrelated_paths() {
        // /tmp can't be HOME on any machine running this test; if HOME
        // ever pointed there, the previous test would already be
        // surfacing it. Path that doesn't share the HOME prefix must
        // come back unchanged so absolute remote-bound paths render
        // verbatim.
        assert_eq!(tilde_collapse("/tmp/work"), "/tmp/work");
        assert_eq!(tilde_collapse("/"), "/");
        assert_eq!(tilde_collapse(""), "");
    }

    #[test]
    fn tilde_collapse_with_home_returns_input_when_home_unset_or_empty() {
        // The wildmenu wrapper falls back to the bare path when `$HOME`
        // is missing or empty so the row stays legible — host-bound
        // candidates already arrive as literal `~/...` strings; local
        // candidates without HOME just render absolute. Direct test
        // against the inner helper because process-env mutation is
        // unsafe across test threads.
        assert_eq!(
            tilde_collapse_with_home("/Users/x/codemux", None),
            "/Users/x/codemux",
        );
        assert_eq!(
            tilde_collapse_with_home("/Users/x/codemux", Some("")),
            "/Users/x/codemux",
        );
    }

    #[test]
    fn tilde_collapse_with_home_replaces_explicit_home_prefix() {
        assert_eq!(
            tilde_collapse_with_home("/synthetic/home/proj", Some("/synthetic/home")),
            "~/proj",
        );
        assert_eq!(
            tilde_collapse_with_home("/synthetic/home", Some("/synthetic/home")),
            "~",
        );
        // Sibling whose absolute path *starts with* the HOME string but
        // isn't actually under HOME (no `/` separator) — must NOT be
        // collapsed; that would falsely rewrite `/synthetic/home2` as
        // `~2`. The strip-prefix-then-strip-`/` chain catches this.
        assert_eq!(
            tilde_collapse_with_home("/synthetic/home2", Some("/synthetic/home")),
            "/synthetic/home2",
        );
    }

    #[test]
    fn named_project_row_unselected_lays_out_star_name_dim_path() {
        let row = named_project_row("codemux", "/Users/x/codemux", None, false, 60);
        let spans: Vec<&str> = row.spans.iter().map(|s| s.content.as_ref()).collect();
        // Marker, star, name, gap (whitespace), path.
        assert_eq!(spans[0], "   ", "marker is 3 spaces when unselected");
        assert_eq!(spans[1], "★ ", "star + space");
        assert_eq!(spans[2], "codemux", "name verbatim");
        assert!(
            spans[3].chars().all(|c| c == ' ') && !spans[3].is_empty(),
            "gap is whitespace-only padding (got {:?})",
            spans[3],
        );
        // Path span content varies with HOME but must end in /codemux.
        assert!(
            spans[4].ends_with("/codemux"),
            "path span must end in the project's leaf (got {:?})",
            spans[4],
        );
        // Path span carries the DIM modifier; the star carries Yellow+BOLD.
        assert!(
            row.spans[1].style.add_modifier.contains(Modifier::BOLD),
            "star must be bold for emphasis",
        );
        assert!(
            row.spans[4].style.add_modifier.contains(Modifier::DIM),
            "path must render dim",
        );
    }

    #[test]
    fn named_project_row_renders_host_badge_next_to_name() {
        // Remote-bound saved projects show the ` @host` badge
        // immediately after the name (greyed out) so the host is
        // visible in the same eye-stroke as the project name. The
        // badge sits BEFORE the gap+path, not at the end of the row.
        let row = named_project_row("codemux", "~/codemux", Some("devpod-go"), false, 60);
        // Expected layout: [marker, star, name, badge, gap, path].
        assert_eq!(row.spans[0].content.as_ref(), "   ", "marker");
        assert_eq!(row.spans[1].content.as_ref(), "★ ", "star");
        assert_eq!(row.spans[2].content.as_ref(), "codemux", "name");
        assert_eq!(
            row.spans[3].content.as_ref(),
            " @devpod-go",
            "host badge follows the name with a single-space separator",
        );
        assert!(
            row.spans[3].style.add_modifier.contains(Modifier::DIM),
            "host badge must render dim so it reads as secondary to the name",
        );
        assert!(
            row.spans[3].style.fg.is_none(),
            "host badge uses default fg + DIM (greyed out), no accent color",
        );
        // Gap + path still close out the row.
        assert!(
            row.spans[4].content.chars().all(|c| c == ' '),
            "gap is whitespace-only padding (got {:?})",
            row.spans[4].content,
        );
        assert!(
            row.spans[5].content.ends_with("/codemux"),
            "path span trails the row",
        );
    }

    #[test]
    fn named_project_row_omits_badge_when_local() {
        let row = named_project_row("codemux", "/Users/x/codemux", None, false, 60);
        for span in &row.spans {
            assert!(
                !span.content.contains('@'),
                "no host badge expected on local saved projects (got {:?})",
                span.content,
            );
        }
    }

    #[test]
    fn named_project_row_selected_applies_highlight_at_line_level() {
        // Selected rows share the unselected span layout but get the
        // cyan-bg/black-fg/bold highlight applied at the line level via
        // patch_style. Building the spans once for both states avoids
        // the per-frame format!() allocation the old single-span
        // selected branch used.
        let row = named_project_row("codemux", "/Users/x/codemux", Some("devpod-go"), true, 60);
        // Span shape matches the unselected layout: marker, star, name,
        // badge, gap, path.
        assert_eq!(row.spans.len(), 6, "got {:?}", row.spans);
        assert_eq!(row.spans[0].content.as_ref(), " ▸ ", "selected marker");
        assert_eq!(row.spans[1].content.as_ref(), "★ ");
        assert_eq!(row.spans[2].content.as_ref(), "codemux");
        assert_eq!(row.spans[3].content.as_ref(), " @devpod-go");
        // Selection styling lives on the line, not on individual spans.
        assert_eq!(row.style.fg, Some(Color::Black));
        assert_eq!(row.style.bg, Some(Color::Cyan));
        assert!(
            row.style.add_modifier.contains(Modifier::BOLD),
            "selected line must carry BOLD: {:?}",
            row.style,
        );
    }

    #[test]
    fn named_project_row_drops_path_when_width_is_too_tight() {
        // 22 columns: marker(3) + "★ "(2) + "codemux"(7) + " @devpod-go"(11) = 23
        // → no room for path. Falls back to keeping star+name+badge legible.
        let row = named_project_row("codemux", "/Users/x/codemux", Some("devpod-go"), false, 22);
        let path_span_present = row
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::DIM) && s.content.contains('/'));
        assert!(
            !path_span_present,
            "tight-width row should drop the path entirely instead of clobbering the badge",
        );
    }

    #[test]
    fn named_project_row_uses_terminal_columns_for_wide_glyph_names() {
        // A CJK name takes 2 terminal columns per character — using
        // chars().count() instead of UnicodeWidthStr::width() would
        // under-count the budget by half and overflow the row. This
        // test pins the column-aware accounting: with width=20 and
        // a 4-char name (8 columns), marker(3) + "★ "(2) + name(8)
        // + " "(min gap) = 14 columns minimum. The remaining ~6
        // columns either hold a short clipped path or fall to the
        // empty-path branch — either way nothing should overflow.
        let row = named_project_row("こんにちは", "/Users/x/proj", None, false, 20);
        // Unselected layout: spans = [marker, star, name, gap, path].
        // Reconstruct the rendered text and assert column width.
        let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
        let cols = UnicodeWidthStr::width(text.as_str());
        assert!(
            cols <= 20,
            "row width must respect terminal columns: text={text:?}, cols={cols}",
        );
    }

    /// Build a `WildmenuRowContext` matching fuzzy + path mode, the
    /// only mode where saved-project rows surface in the real render.
    fn fuzzy_path_ctx(width: usize) -> WildmenuRowContext {
        WildmenuRowContext {
            width,
            zone: Zone::Path,
            fuzzy: true,
            precise_search: false,
            stale: false,
        }
    }

    #[test]
    fn wildmenu_row_routes_named_project_candidates_through_named_project_row() {
        // Plumbing test: the wildmenu_row method must consult
        // `project_meta` and call `named_project_row` for any
        // candidate that has a matching saved-project entry. Without
        // this branch the user would see a bare path, defeating the
        // whole feature.
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.user_search_mode = SearchMode::Fuzzy;
        m.filtered = vec!["/Users/x/codemux".to_string()];
        m.project_meta.insert(
            "/Users/x/codemux".to_string(),
            NamedProjectMeta {
                name: "codemux".to_string(),
                host: None,
            },
        );
        m.selected = None;
        let row = m.wildmenu_row("/Users/x/codemux", 0, &fuzzy_path_ctx(60));
        let star_present = row.spans.iter().any(|s| s.content.as_ref() == "★ ");
        let name_present = row.spans.iter().any(|s| s.content.as_ref() == "codemux");
        assert!(star_present, "expected ★ marker, got {:?}", row.spans);
        assert!(name_present, "expected name span, got {:?}", row.spans);
    }

    #[test]
    fn wildmenu_row_routes_unnamed_candidates_through_generic_branch() {
        // Negative pairing: candidates without a `project_meta` entry
        // fall through to the generic full-path row (no star, no name
        // swap). Together with the test above this pins the lookup
        // gate so a future refactor can't accidentally upgrade every
        // row to the saved-project layout.
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.user_search_mode = SearchMode::Fuzzy;
        m.filtered = vec!["/work/anon".to_string()];
        m.selected = None;
        let row = m.wildmenu_row("/work/anon", 0, &fuzzy_path_ctx(60));
        let collapsed: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !collapsed.contains('★'),
            "generic row should not carry the saved-project star: {collapsed:?}",
        );
        assert!(
            collapsed.contains("/work/anon"),
            "generic fuzzy + path row must show the full candidate path: {collapsed:?}",
        );
    }

    #[test]
    fn wildmenu_row_routes_precise_search_through_precise_search_row() {
        // The precise_search flag wins over the saved-project lookup —
        // in precise mode the user is autocompleting a path they typed
        // and expects parent/leaf rendering, not the alias swap.
        let mut m = mb("", "/Users/x/co", Zone::Path, &[]);
        m.filtered = vec!["/Users/x/codemux".to_string()];
        m.project_meta.insert(
            "/Users/x/codemux".to_string(),
            NamedProjectMeta {
                name: "codemux".to_string(),
                host: None,
            },
        );
        m.selected = None;
        let ctx = WildmenuRowContext {
            width: 60,
            zone: Zone::Path,
            fuzzy: false,
            precise_search: true,
            stale: false,
        };
        let row = m.wildmenu_row("/Users/x/codemux", 0, &ctx);
        let collapsed: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !collapsed.contains('★'),
            "precise_search must take precedence over saved-project rendering: {collapsed:?}",
        );
    }

    #[test]
    fn wildmenu_row_dims_named_project_when_stale_and_unselected() {
        // Stale results (fresh fuzzy query in flight) dim every row
        // except the selected one. The named-project branch needs the
        // same treatment as the generic branch — without the patch
        // call, saved-project rows would stay full-bright while the
        // generic rows around them dim, which is visually jarring.
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.user_search_mode = SearchMode::Fuzzy;
        m.filtered = vec!["/Users/x/codemux".to_string()];
        m.project_meta.insert(
            "/Users/x/codemux".to_string(),
            NamedProjectMeta {
                name: "codemux".to_string(),
                host: None,
            },
        );
        m.selected = None;
        let ctx = WildmenuRowContext {
            width: 60,
            zone: Zone::Path,
            fuzzy: true,
            precise_search: false,
            stale: true,
        };
        let row = m.wildmenu_row("/Users/x/codemux", 0, &ctx);
        assert!(
            row.style.add_modifier.contains(Modifier::DIM),
            "stale + unselected row must carry the DIM modifier on the line style: {:?}",
            row.style,
        );
    }

    #[test]
    fn wildmenu_row_skips_named_lookup_when_zone_is_host() {
        // Host-zone candidates are SSH host names, never saved-project
        // paths — the lookup is gated on `zone == Path` so a stray
        // entry that happened to share a host alias name wouldn't
        // accidentally upgrade a host row to the saved-project layout.
        let mut m = mb("", "", Zone::Host, &["devpod-go"]);
        m.filtered = vec!["devpod-go".to_string()];
        m.project_meta.insert(
            "devpod-go".to_string(),
            NamedProjectMeta {
                name: "devpod-go".to_string(),
                host: None,
            },
        );
        m.selected = None;
        let ctx = WildmenuRowContext {
            width: 60,
            zone: Zone::Host,
            fuzzy: true,
            precise_search: false,
            stale: false,
        };
        let row = m.wildmenu_row("devpod-go", 0, &ctx);
        let collapsed: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !collapsed.contains('★'),
            "host-zone rows must never render the saved-project marker: {collapsed:?}",
        );
    }

    #[test]
    fn wildmenu_row_generic_branch_selected_row_uses_cyan_highlight() {
        // Pins the selected-row styling on the generic (non-saved-
        // project) branch: when a host or unindexed candidate is
        // highlighted the line gets the modal's cyan-bg + black-fg
        // treatment so the active pick reads from across the row.
        let mut m = mb("", "", Zone::Host, &["devpod-go"]);
        m.filtered = vec!["devpod-go".to_string()];
        m.selected = Some(0);
        let ctx = WildmenuRowContext {
            width: 60,
            zone: Zone::Host,
            fuzzy: false,
            precise_search: false,
            stale: false,
        };
        let row = m.wildmenu_row("devpod-go", 0, &ctx);
        assert_eq!(row.style.bg, Some(Color::Cyan));
        assert_eq!(row.style.fg, Some(Color::Black));
        assert!(
            row.style.add_modifier.contains(Modifier::BOLD),
            "selected row must carry the BOLD modifier: {:?}",
            row.style,
        );
    }

    #[test]
    fn wildmenu_row_generic_branch_dims_unselected_when_stale() {
        // Stale generic rows (fuzzy worker pass in flight) dim along
        // with the saved-project rows so the wildmenu reads as
        // uniformly "results in flight" instead of mixing bright and
        // dim entries.
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.user_search_mode = SearchMode::Fuzzy;
        m.filtered = vec!["/work/anon".to_string()];
        m.selected = Some(99); // off-screen selection so this row is unselected
        let ctx = WildmenuRowContext {
            width: 60,
            zone: Zone::Path,
            fuzzy: true,
            precise_search: false,
            stale: true,
        };
        let row = m.wildmenu_row("/work/anon", 0, &ctx);
        assert!(
            row.style.add_modifier.contains(Modifier::DIM),
            "stale + unselected generic row must carry DIM: {:?}",
            row.style,
        );
    }

    #[test]
    fn confirm_local_project_emits_plain_spawn() {
        // A project without `host` keeps today's behavior: confirming
        // its path emits `Spawn { host: "local", path }`. Fuzzy mode
        // is where named projects actually surface in the wildmenu;
        // Precise mode now treats Enter+selection as descend.
        let mut m = SpawnMinibuffer::open(
            Path::new("/tmp"),
            SearchMode::Fuzzy,
            vec![NamedProject {
                name: "p".to_string(),
                path: "/tmp/p".to_string(),
                host: None,
            }],
        );
        m.host.clear();
        m.fuzzy_query = "p".to_string();
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
        // zone. Fuzzy mode (where named projects actually surface).
        let mut m = SpawnMinibuffer::open(
            Path::new("/tmp"),
            SearchMode::Fuzzy,
            vec![NamedProject {
                name: "p".to_string(),
                path: "/work/p".to_string(),
                host: Some("devpod-1".to_string()),
            }],
        );
        m.host = "ignored-typed-host".to_string();
        m.fuzzy_query = "p".to_string();
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

    /// `~` typed against an autoseeded Precise path (i.e. just-
    /// opened modal) triggers the nav-at-home gesture, just like
    /// in Fuzzy mode. Without this the user types `~` and the
    /// modal appends a literal tilde to the seeded cwd, then the
    /// next char looks like a fuzzy "no matches" result.
    #[test]
    fn tilde_in_precise_with_autoseeded_path_enters_navigation_at_home() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = SpawnMinibuffer::open(dir.path(), SearchMode::Precise, Vec::new());
        assert_eq!(m.path_origin, PathOrigin::AutoSeeded);
        m.handle(&key(KeyCode::Char('~')), &b(), &mut local());
        assert!(m.path.ends_with('/'), "tilde must seed at $HOME/");
        let expected_home = std::env::var_os("HOME").map_or_else(
            || "~/".to_string(),
            |h| format!("{}/", h.to_string_lossy().trim_end_matches('/')),
        );
        assert_eq!(m.path, expected_home);
    }

    /// `/` typed against an autoseeded Precise path replaces the
    /// seed with `/` (root) — the Precise companion to the existing
    /// Fuzzy `/`-at-empty-query behavior.
    #[test]
    fn slash_in_precise_with_autoseeded_path_enters_navigation_at_root() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = SpawnMinibuffer::open(dir.path(), SearchMode::Precise, Vec::new());
        assert_eq!(m.path_origin, PathOrigin::AutoSeeded);
        m.handle(&key(KeyCode::Char('/')), &b(), &mut local());
        assert_eq!(m.path, "/");
    }

    /// `~` typed against a USER-typed Precise path is preserved as
    /// a literal char, since the user has expressed a path-building
    /// intent and we shouldn't blow it away.
    #[test]
    fn tilde_in_precise_with_user_typed_path_is_literal() {
        let mut m = mb("", "/work/proj", Zone::Path, &[]);
        m.path_origin = PathOrigin::UserTyped;
        m.handle(&key(KeyCode::Char('~')), &b(), &mut local());
        assert_eq!(m.path, "/work/proj~");
    }

    /// Intl layouts whose dead-key tilde doesn't compose with the
    /// next keystroke deliver the combining tilde (U+0303) directly.
    /// The auto-nav gesture must accept this variant so users on
    /// these layouts get the same `~`-then-home behavior as users
    /// whose terminals compose into a literal `~`.
    #[test]
    fn combining_tilde_in_fuzzy_with_empty_query_enters_navigation_at_home() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.user_search_mode = SearchMode::Fuzzy;
        let outcome = m.handle(&key(KeyCode::Char('\u{0303}')), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.search_mode, SearchMode::Precise);
        assert!(m.path.ends_with('/'));
        assert!(
            m.tilde_compose_armed,
            "non-literal tilde variant must arm the compose-space swallow",
        );
    }

    /// After a non-literal tilde variant fires the nav-at-home
    /// gesture, the OS / terminal commonly delivers the composing
    /// space as the next event. Swallow it so the user doesn't end
    /// up with `~/ ` (a stray space appended to the seeded home).
    #[test]
    fn space_after_combining_tilde_is_swallowed() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.handle(&key(KeyCode::Char('\u{0303}')), &b(), &mut local());
        let path_before = m.path.clone();
        let outcome = m.handle(&key(KeyCode::Char(' ')), &b(), &mut local());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.path, path_before, "space must not append to seeded path");
        assert!(
            !m.tilde_compose_armed,
            "the swallow is one-shot — flag must drop",
        );
    }

    /// A literal `~` is unambiguous (no dead-key composition needed),
    /// so the compose-space swallow does NOT arm. A subsequent space
    /// behaves like any other Precise-mode char and gets appended,
    /// matching today's behavior on standard layouts.
    #[test]
    fn literal_tilde_does_not_arm_compose_swallow() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.handle(&key(KeyCode::Char('~')), &b(), &mut local());
        assert!(
            !m.tilde_compose_armed,
            "literal `~` must not arm the swallow",
        );
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
    fn mark_fuzzy_stale_is_no_op_outside_fuzzy_path() {
        // In Host zone, even if mode is Fuzzy, mark_fuzzy_stale must
        // not touch `filtered` (which holds host candidates from
        // `refresh`) or set the stale flag (no fuzzy work pending).
        let mut m = mb("dev", "", Zone::Host, &["devpod-web"]);
        m.search_mode = SearchMode::Fuzzy;
        let before = m.filtered.clone();
        m.mark_fuzzy_stale();
        assert_eq!(m.filtered, before, "host wildmenu must not be clobbered");
        assert!(!m.filtered_stale);
    }

    #[test]
    fn set_fuzzy_results_works_with_refreshing_index_dirs() {
        // SWR (Refreshing) doesn't change anything from the modal's
        // perspective — the worker scores against whatever dirs are
        // cached, and the modal accepts the result by `(host, query)`
        // tag. This test validates the full round-trip: the worker
        // produced hits from a Refreshing-style snapshot (synthesised
        // here as a `FuzzyResult`) and the modal applies them.
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.fuzzy_query = "code".to_string();
        m.filtered_stale = true;
        let result = FuzzyResult {
            host: HOST_PLACEHOLDER.to_string(),
            query: "code".to_string(),
            hits: vec![
                "/home/df/code-utils".to_string(),
                "/home/df/Workbench/codemux".to_string(),
            ],
        };
        m.set_fuzzy_results(result);
        assert_eq!(m.filtered.len(), 2);
        assert!(!m.filtered_stale);
    }

    /// Regression: during background indexing, every batch of newly-
    /// indexed dirs triggers a re-score (the runtime clears the
    /// last-pushed-query memo when the per-host generation bumps).
    /// The user-visible bug was: I press Down to pick the second
    /// candidate, the next batch lands a few hundred ms later, and my
    /// selection snaps back to the top because the old code reset
    /// `selected = Some(0)` on every result. Identity-preservation
    /// keeps the user's pick anchored to the path string.
    #[test]
    fn set_fuzzy_results_preserves_user_selection_across_rescore() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.fuzzy_query = "code".to_string();
        let first = FuzzyResult {
            host: HOST_PLACEHOLDER.to_string(),
            query: "code".to_string(),
            hits: vec![
                "/home/df/code-utils".to_string(),
                "/home/df/Workbench/codemux".to_string(),
            ],
        };
        m.set_fuzzy_results(first);
        // User presses Down once: selection now on /home/df/Workbench/codemux.
        m.move_selection_forward();
        assert_eq!(m.selected, Some(1));

        // Indexer batch lands; worker re-scores with a larger dir set
        // and the same hits land in a different order (a new dir
        // outscores /home/df/code-utils).
        let second = FuzzyResult {
            host: HOST_PLACEHOLDER.to_string(),
            query: "code".to_string(),
            hits: vec![
                "/home/df/Workbench/code-stuff".to_string(),
                "/home/df/code-utils".to_string(),
                "/home/df/Workbench/codemux".to_string(),
            ],
        };
        m.set_fuzzy_results(second);
        assert_eq!(
            m.selected,
            Some(2),
            "selection must follow the user's pick to its new index"
        );
    }

    /// When the previously-selected path drops out of the new hits
    /// (e.g. the index now contains a path that displaces the user's
    /// pick beyond the cap), fall back to the first hit so Enter
    /// still commits a valid candidate.
    #[test]
    fn set_fuzzy_results_falls_back_to_first_when_selection_dropped() {
        let mut m = mb("", "", Zone::Path, &[]);
        m.search_mode = SearchMode::Fuzzy;
        m.fuzzy_query = "code".to_string();
        let first = FuzzyResult {
            host: HOST_PLACEHOLDER.to_string(),
            query: "code".to_string(),
            hits: vec![
                "/home/df/code-utils".to_string(),
                "/home/df/Workbench/codemux".to_string(),
            ],
        };
        m.set_fuzzy_results(first);
        m.move_selection_forward();
        assert_eq!(m.selected, Some(1));

        let second = FuzzyResult {
            host: HOST_PLACEHOLDER.to_string(),
            query: "code".to_string(),
            hits: vec![
                "/home/df/code-utils".to_string(),
                "/home/df/code-vault".to_string(),
            ],
        };
        m.set_fuzzy_results(second);
        assert_eq!(
            m.selected,
            Some(0),
            "missing prior selection falls back to first hit"
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

    /// ── Fast-tier snapshot harness (T0 of the E2E plan) ──────────
    ///
    /// Renders the spawn modal in its initial empty state into a
    /// `ratatui::backend::TestBackend` and snapshots it via `insta`.
    /// This is the closest fixture to "empty boot screen" that doesn't
    /// require constructing a full `Runtime` (that's T1): on first
    /// launch with no agents, the spawn modal is what the user sees.
    ///
    /// The minibuffer is built directly (not through `open()`) so the
    /// snapshot is deterministic regardless of the test runner's cwd
    /// and `~/.ssh/config`. `refresh()` is not called for the same
    /// reason — it would shell out to `read_dir` on whatever the test
    /// process inherited as cwd. Empty `filtered` exercises the
    /// `wildmenu_view` "no matches" sentinel branch, which is the
    /// accurate first-frame render.
    ///
    /// No `pub` test-helper wrapper was added to production — the test
    /// builds `SpawnMinibuffer` through its private fields via
    /// `super::*`, which is the whole point of an inline test module.
    #[test]
    fn render_empty_boot_screen_snapshot() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let m = SpawnMinibuffer::new_for_test(PathBuf::from("/test/cwd"));

        // 100×30 leaves comfortable margin above the 8-row strip and
        // is wide enough that the prompt + hint span fits on one line
        // — narrower geometries truncate the hint and obscure the
        // chrome regression the snapshot is meant to catch.
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let bindings = ModalBindings::default();
        let draw_result = terminal.draw(|frame| {
            let area = frame.area();
            m.render(frame, area, &bindings, None);
        });
        assert!(draw_result.is_ok(), "draw failed: {draw_result:?}");

        insta::assert_snapshot!(terminal.backend());
    }

    /// Companion to `render_empty_boot_screen_snapshot` at the smallest
    /// standard terminal geometry (80×24). Locks the chrome-under-width-
    /// pressure path: the hint span on the prompt row is wider than 80
    /// cells, so the renderer must clip it gracefully. A regression that
    /// stops clipping (or clips wrong) shows up here first.
    #[test]
    fn snapshot_spawn_modal_default() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let m = SpawnMinibuffer::new_for_test(PathBuf::from("/test/cwd"));
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let bindings = ModalBindings::default();
        terminal
            .draw(|frame| {
                let area = frame.area();
                m.render(frame, area, &bindings, None);
            })
            .unwrap();
        insta::assert_snapshot!(terminal.backend());
    }
}
