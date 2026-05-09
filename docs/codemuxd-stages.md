# `codemuxd` build-out

Operational tracker for the daemon work that realises AD-3
(`docs/architecture.md`) and unblocks SSH-host agents in the spawn modal.
Each stage = one commit, each `just check`-clean before progressing. Pause
for review after every stage.

Commit style: `feat(p1): subject`. No AI trailers.

## Status

- ✅ **Stage 0** — Daemon walking skeleton (`8dbf805`)
- ✅ **Stage 1** — Wire protocol with Hello/HelloAck handshake (`1452c4f`)
- ✅ **Stage 2** — Filesystem layout, exclusivity, log redirection
- ✅ **Stage 3** — `AgentTransport` enum + `LocalPty` (refactor only)
- ✅ **Stage 4** — `SshDaemonPty` adapter + bootstrap
- ✅ **Stage 5** — Wire SSH transport into the spawn modal
- ✅ **Stage 6** — Modal-driven spawn flow + remote folder picker

End-to-end ship test (after Stage 5) is in **Verification** below.

---

## Stage 2 — Filesystem layout, exclusivity, log redirection

Daemon learns to live alongside other daemon instances on the same host
and to write its tracing output to a file when not in `--foreground`.

### Scope

- New CLI flags wired through `apps/daemon/src/cli.rs`:
  - `--agent-id <id>` — required when not `--foreground`; used to derive
    socket / pid / log paths
  - `--pid-file <path>` — exclusive-create; refuses to overwrite a live
    pid; reaps stale pid files (`kill -0` returns ESRCH)
  - `--log-file <path>` — tracing redirected here when not `--foreground`
  - `--cwd <path>` — already accepted, but reject early with a clear
    `Error::Spawn` if missing on the remote (no silent fallback per
    vision principle 6)
- Default fs layout per AD-3:
  ```
  ~/.cache/codemuxd/
    bin/codemuxd
    sockets/{agent-id}.sock     (mode 0600)
    pids/{agent-id}.pid         (exclusive create)
    logs/{agent-id}.log
    src/codemuxd-{version}.tar.gz
    agent.version               (text: "codemuxd-{cargo-pkg-version}")
  ```
- New module `apps/daemon/src/fs_layout.rs` — pure path resolution,
  `HOME` overrideable for tests via env var (no global state)
- `Supervisor::bind` extended:
  - Pid file is exclusive-create with `O_EXCL`; on EEXIST, read the pid,
    `kill(pid, 0)` — if ESRCH, unlink and retry; otherwise return
    `Error::AlreadyAttached`
  - Replace the existing `let _ = std::fs::remove_file(&cli.socket)`
    blanket-unlink with the same liveness check
  - `chmod` socket to 0600 immediately after `bind`
- Tracing setup: when `!cli.foreground`, redirect the `tracing_subscriber`
  to a `RollingFileAppender` (or just plain `File::create(&log_file)`)
  scoped to `EnvFilter::new("codemuxd=info,warn")`. Default `--foreground`
  keeps stderr behaviour from Stage 0.

### Files to add / touch

- New: `apps/daemon/src/fs_layout.rs`
- Modify: `apps/daemon/src/cli.rs`, `apps/daemon/src/main.rs`,
  `apps/daemon/src/supervisor.rs`, `apps/daemon/src/error.rs`
  (add `Error::PidFileLocked { pid: u32, path: PathBuf }` and
  `Error::CwdNotFound { path: PathBuf }`)

### Tests

- `fs_layout::tests` — tempdir-based path resolution; HOME-override
  produces the expected `{home}/.cache/codemuxd/...` paths; missing
  parent dirs are created on demand
- `supervisor::tests::stale_pid_file_is_reaped_on_bind` —
  write a pid file with a definitely-dead pid (`u32::MAX`), bind, assert
  it's overwritten
- `supervisor::tests::live_pid_file_blocks_bind` —
  spawn a long-running child (e.g. `sleep 30`), write its pid, bind,
  assert `Error::AlreadyAttached`
- `supervisor::tests::socket_mode_is_0600` —
  bind, `stat` socket, assert `mode & 0o777 == 0o600`

### Exit criteria

- `just check` clean
- Two daemons launched against the same `--agent-id` cleanly conflict
  (second exits with `AlreadyAttached`); two daemons with different
  `--agent-id` coexist
- `RUST_LOG=codemuxd=trace cargo run -p codemux-daemon -- ... --log-file
  /tmp/d.log` writes structured output to `/tmp/d.log`, nothing on stderr

---

## Stage 3 — `AgentTransport` enum + `LocalPty` (refactor only)

Pure refactor. **No user-visible change.** Introduces the seam Stage 4
will hang the SSH variant on. Keeping the enum closed from the start
(both variants present, `SshDaemon::spawn_ssh` returns "not yet
implemented") avoids later mutation of the type.

### Scope

- New module `crates/session/src/transport.rs`:
  ```rust
  #[non_exhaustive]
  pub enum AgentTransport {
      Local(LocalPty),
      SshDaemon(SshDaemonPty),  // stub in Stage 3
  }
  ```
- Methods on `AgentTransport` (match the existing `RuntimeAgent` field
  uses at `apps/tui/src/runtime.rs:121-128, 240, 246, 273-279, 391`):
  - `spawn_local(label, cwd, rows, cols) -> Result<Self, Error>`
  - `spawn_ssh(host, agent_id, cwd, rows, cols) -> Result<Self, Error>`
    — **Stage 3 returns `Err("not yet implemented")`**
  - `try_read() -> Vec<Vec<u8>>` (drains the per-transport channel)
  - `write(&[u8]) -> Result<(), Error>`
  - `resize(rows, cols) -> Result<(), Error>`
  - `signal(SignalKind) -> Result<(), Error>`
  - `try_wait() -> Option<i32>` (None = alive, Some(code) = died)
  - `kill() -> Result<(), Error>`
- `LocalPty::spawn` body comes from
  `apps/tui/src/runtime.rs::spawn_agent` (current lines 174-214 — verify
  range when starting Stage 3, file moves)
- `spawn_reader_thread` (currently `apps/tui/src/runtime.rs:216-232`)
  moves into `transport.rs` as a free function — same crossbeam-channel
  shape, no behavioural change
- `RuntimeAgent` collapses from 6 fields to 3:
  `{label, parser, transport}`. `parser: Parser` stays in the runtime
  (rendering concern per AD-1)
- Add deps to `crates/session/Cargo.toml`: `portable-pty`,
  `crossbeam-channel`, `codemux-wire` (the third for `Signal` reuse —
  no need for a duplicate enum)

### Tests

- `crates/session/src/transport.rs::tests` —
  spawn local PTY with `cat`, write/read/resize/kill cycle. The Stage 0
  daemon tests in `apps/daemon/src/session.rs` are a good template
- TUI tests should keep passing without modification (the refactor is
  invisible at the runtime boundary)

### Exit criteria

- `just check` clean — same 177 (or +N) tests, no regressions
- `just run` produces an identical user experience to before the refactor
- `runtime.rs` is shorter; `RuntimeAgent` is the 3-field shape

---

## Stage 4 — `SshDaemonPty` adapter + bootstrap

The hard stage. Auto-installs the daemon on first SSH connect and tunnels
the local TUI to the remote socket.

### Deviations from plan (as shipped)

- **Bootstrap module lives in its own adapter crate `crates/codemuxd-bootstrap`,
  not `crates/session/src/bootstrap.rs` (initial implementation) and not
  `apps/tui/src/bootstrap.rs` (original plan).** The first deviation
  put it next to `AgentTransport::spawn_ssh` for the cleanest call
  site; review then flagged that as a Hexagonal violation (the
  `session` crate is the application core and should not own SSH/scp
  orchestration). The fix carved off `crates/codemuxd-bootstrap` as a
  Secondary/Driven Adapter that depends on `session` (correct
  direction). Public surface:
  `bootstrap::bootstrap(runner, host, agent_id, cwd, local_socket_dir)`,
  `bootstrap::establish_ssh_transport(...)` (composes bootstrap +
  `SshDaemonPty::attach`), `bootstrap::CommandRunner`,
  `bootstrap::RealRunner`, `bootstrap::default_local_socket_dir`,
  `bootstrap::Error`, `bootstrap::Stage`. The session crate keeps
  `Error::Handshake` for post-bootstrap wire-handshake failures
  (formerly bundled under `Error::Bootstrap{Handshake}`).
- **Daemon-side `Resize`/`Signal::Kill`/`Ping`/`Pong` and real
  `ChildExited` exit codes were wired in this stage** (Stage 2 left them
  as `tracing::warn!("Stage 2 will apply") and `exit_code: 0`
  placeholders). Without them an SSH session spawned via Stage 5 would
  have known UX bugs (terminal resize lost, Ctrl-C → Kill silently
  dropped, exit code always 0). Out-of-scope per the original plan but
  in-scope here so Stage 5 isn't shipping known-broken UX.
- **Tarball assembly via `build.rs` + `include_bytes!(OUT_DIR)`** rather
  than runtime tar generation. The build script walks `apps/daemon` and
  `crates/wire`, bundles `Cargo.lock`/`rust-toolchain.toml`, and stamps
  in `crates/session/bootstrap-root/Cargo.toml` (a self-contained
  workspace manifest with concrete dep versions) as the tarball's root
  `Cargo.toml`. Cache-invalidation key is a SipHash of the tarball
  bytes (`bootstrap_version()`), so any source change auto-bumps the
  remote-installed version.
- **`local_socket_dir` is an explicit parameter** of `bootstrap()` (with
  a `default_local_socket_dir() -> Result<PathBuf>` helper that reads
  `$HOME`). This avoids `unsafe { std::env::set_var }` in tests
  (workspace `unsafe_code = "forbid"`).
- **`SshDaemonPty::attach` accepts an `Option<Child>` for the tunnel**.
  Production passes `Some(child)` from `bootstrap()`; tests pass `None`
  to attach against an in-process socket without spinning up `ssh -L`.

### Scope

- New module `apps/tui/src/bootstrap.rs` — implements the 7-step flow:
  1. **Probe**:
     `ssh -o BatchMode=yes -o ConnectTimeout=5 host 'cat ~/.cache/codemuxd/agent.version 2>/dev/null'`
     → exit 0 + matching version → skip to step 5
  2. **Tarball assembly** (in-process, cached for the session):
     generate `target/codemuxd-bootstrap.tar.gz` containing:
     - `apps/daemon/`
     - `crates/wire/`
     - root `Cargo.lock`, `rust-toolchain.toml`
     - a generated stub `Cargo.toml` listing only those two members with
       hardcoded dependency versions (NO `workspace = true` inheritance —
       must be self-contained on the remote)
  3. **scp**:
     `scp -B local.tar.gz host:~/.cache/codemuxd/src/codemuxd-{version}.tar.gz`
  4. **Remote build**:
     plain `cargo build --release --bin codemuxd` (NO musl target — uses
     remote's native libc; musl is a future release-pipeline path).
     Move binary to `~/.cache/codemuxd/bin/codemuxd`, write
     `agent.version`. Tee build output to `src/build.log` for diagnostics
  5. **Spawn daemon**:
     `ssh host 'setsid -f ~/.cache/codemuxd/bin/codemuxd
       --socket ~/.cache/codemuxd/sockets/{agent-id}.sock
       --pid-file ~/.cache/codemuxd/pids/{agent-id}.pid
       --log-file ~/.cache/codemuxd/logs/{agent-id}.log
       --agent-id {agent-id}
       --cwd {cwd}'`
     `setsid -f` is the daemonization mechanism — no `nix`/`daemonize`
     workspace deps
  6. **Tunnel socket**:
     `ssh -N -L /tmp/codemux-{uid}/{agent-id}.sock:~/.cache/codemuxd/sockets/{agent-id}.sock host`
     in a background thread. Verify OpenSSH ≥6.7 (unix-socket `-L`
     support). Fallback if the user's devpods don't honour it: socat TCP
     bridge — ugly, defer
  7. **Connect, send Hello, receive HelloAck.** From here the runtime
     treats it identically to a local PTY
- New `BootstrapStage` enum (`VersionProbe`, `TarballBuild`, `Scp`,
  `RemoteBuild`, `DaemonSpawn`, `SocketConnect`, `Handshake`) — each
  failure mode produces a user-visible message keyed off the stage:
  - exit 127 + "cargo" in stderr →
    "`cargo` not found on {host}; install rustup first: https://rustup.rs"
  - cwd-not-found → fail fast with `Error::Bootstrap { stage: DaemonSpawn,
    source: ChildSpawnFailed }` (no silent fallback)
- `apps/tui/src/runtime.rs::spawn_reader_thread` for SSH variant routes
  through the tunnel socket
- `crates/session/src/error.rs` — add
  `Error::Bootstrap { stage: BootstrapStage, source: BoxedSource }`
- `AgentTransport::spawn_ssh` becomes real (no longer "not yet
  implemented")
- Wire-protocol mismatch from probe (`agent.version` says "codemuxd-0.1.x"
  but local is "0.2.y") triggers a re-bootstrap, not a shim

### Tests

- `apps/tui/src/bootstrap.rs::tests` — tarball-assembly tests using
  `tempfile::TempDir`. Mock-`Command` runner trait so we can assert the
  expected ssh/scp invocations without hitting the network. Each
  `BootstrapStage` failure mode gets a test
- `crates/session/src/transport.rs::tests` — round-trip against an
  in-process daemon spawned via the daemon's `lib.rs` re-export (Stage 0
  surface). This exercises the full Hello/HelloAck/PtyData path locally
  without any SSH

### Exit criteria

- `just check` clean
- TUI's spawn-modal SSH branch **still logs warn** —
  `runtime.rs:351-354` is wired in Stage 5

---

## Stage 5 — Wire SSH transport into the spawn modal

Replace the `tracing::warn!` placeholder in
`apps/tui/src/runtime.rs:351-354` with a real
`AgentTransport::spawn_ssh` call.

### Deviations from plan (as shipped)

- **The bootstrap entry point is `codemuxd_bootstrap::establish_ssh_transport`,
  not `AgentTransport::spawn_ssh`.** The original plan named a method on
  the transport enum, but Stage 4 (intentionally) did not add such a
  method — the orchestration lives in the `codemuxd-bootstrap` adapter
  crate per the Hexagonal split (see Stage 4 deviations). Stage 5 honors
  that boundary: the runtime calls into `codemuxd-bootstrap`, which
  composes its own bootstrap pipeline with `SshDaemonPty::attach` and
  hands back an `AgentTransport`. This keeps the `session` core free of
  ssh/scp orchestration.
- **Cancellation is cooperative, not preemptive.** The plan called for
  `Child::kill` on any in-flight ssh subprocess. As shipped,
  cancellation goes through a `CancelableRunner` decorator wrapping
  `CommandRunner`: each subprocess call checks an `Arc<AtomicBool>` and
  short-circuits with `io::ErrorKind::Interrupted` when set. A
  subprocess already running (e.g. `cargo build`) finishes on its own
  before cancellation is observed. Pre-emptive kill would require
  threading subprocess `Child` handles back through the
  `CommandRunner` trait — deliberate scope cap because the typical
  user-cancel ("wait, wrong host") happens before the long build step
  anyway.
- **No granular per-stage progress events.** The plan suggested
  draining progress through a channel alongside PTY data and
  rendering `BootstrapStage` updates. As shipped, the placeholder
  pane shows a single static "bootstrapping codemuxd on {host}…"
  message. The error path *is* stage-aware (the
  `format_bootstrap_error` helper in `runtime.rs` keys off
  `Stage::*`), so a failure surfaces an actionable hint. Granular
  progress would require either threading a callback through
  `bootstrap()` (changes its public surface) or re-implementing the
  7-step orchestration in the worker (duplication). Either is a
  worthwhile follow-up if the build step proves long enough that the
  user wants live feedback.
- **Failed bootstraps are sticky, not auto-dismissed.** The plan said
  "keep the placeholder visible until the user dismisses it" — there
  is no per-agent dismiss key in the codebase yet, so a failed
  bootstrap stays in the navigator until the user exits the TUI
  (`prefix + q`). The error text is rendered in red inside the agent
  pane so it's immediately visible. A `prefix + x` close-current-agent
  binding is a natural P2 follow-up.

### Scope (as built)

- New module `apps/tui/src/bootstrap_worker.rs`:
  - `BootstrapHandle` — holds an `Arc<AtomicBool>` cancel flag, a
    `crossbeam_channel::Receiver` for the result, and a detached
    `JoinHandle` (Rust's `JoinHandle::drop` is detach, which is
    exactly what we want — `BootstrapHandle::drop` must not block
    the TUI).
  - `start(host, agent_id, cwd, rows, cols)` — production entry
    using `RealRunner`.
  - `start_with_runner(...)` — test entry that injects a
    `Box<dyn CommandRunner>`.
  - Internal `CancelableRunner` decorator that intercepts subprocess
    calls and returns `Interrupted` once the cancel flag is set.
- `RuntimeAgent` (in `apps/tui/src/runtime.rs`) gains an `AgentState`
  enum with two variants:
  - `Bootstrapping { host, handle, error }` — placeholder while the
    worker runs; `error` is `Some` once the worker reports failure.
  - `Ready { parser, transport }` — steady state, identical to
    Stage 3's shape modulo `Box<Parser>` (clippy `large_enum_variant`
    flagged the disparity once `Bootstrapping` was added).
- The event loop's drain phase polls `handle.try_recv()` for
  `Bootstrapping` agents each tick; on `Ok(transport)` it transitions
  to `Ready` (and immediately `transport.resize` to current geometry,
  in case the terminal was resized during the bootstrap).
- The spawn-modal SSH branch (formerly the `tracing::warn!`
  placeholder) now calls `bootstrap_worker::start` and pushes a
  `Bootstrapping` agent into the navigator.
- Render: `render_agent_pane` switches on `AgentState`. Ready →
  `tui-term` `PseudoTerminal`. Bootstrapping →
  `render_bootstrap_placeholder` with stage-keyed error text.
- Key writes (`KeyDispatch::Forward`) drop bytes destined for a
  `Bootstrapping` agent (with a `tracing::trace!` breadcrumb).

### Tests

- `bootstrap_worker::tests::cancel_short_circuits_at_next_subprocess_call`
  — uses a `BlockingRunner` (records calls, blocks the first call on
  a one-shot channel); test arms cancellation, releases the in-flight
  call, polls the worker for completion, and asserts that the inner
  runner saw exactly one call (the second stage was intercepted by
  `CancelableRunner` before reaching it).
- `bootstrap_worker::tests::cancel_is_idempotent` — explicit
  double-cancel + Drop-cancel does not panic.
- Manual end-to-end smoke per the script in **Verification** below
  (no automated SSH integration test — that requires real network).

### Exit criteria

End-to-end smoke passes:

1. Reachable SSH host (e.g. a remote dev VM) with `cargo` installed
2. `just run` (local codemux)
3. Open spawn modal (default: prefix + `c`), type
   `<hostname> : ~/some-repo`, Enter
4. Watch the navigator: agent enters with status "bootstrapping" for
   ~30-60s on first contact, then "running" with the claude TUI rendered
5. Verify daemon survives SSH disconnect: from another terminal,
   `ssh <hostname> ps aux | grep codemuxd` → daemon is alive
6. Kill local TUI mid-session, restart it. (Reattach across TUI restart
   needs P1 persistence work — AD-7, separately scoped — but the daemon
   itself is still running on the remote, verifiable via step 5)
7. Spawn a second agent on the same host. Verify a second daemon process
   exists (one-per-agent model from AD-3)
8. Switch focus between local and remote agents in a single keystroke.
   This reproduces Scenario 1 from `docs/002--use-cases.md`

---

## Stage 6 — Modal-driven spawn flow + remote folder picker

Restructure the SSH spawn UX so the bootstrap progress lives *inside*
the spawn modal (not in the agent pane), and add remote-`$HOME` folder
autocomplete once the daemon is reachable.

### Motivation

Stage 5 shipped the SSH path with a placeholder agent created the
moment the user pressed Enter — the agent pane went into a 30-60 s
spinner while `cargo build` ran on the remote. Two problems:

- The user had to type the remote `cwd` *before* the daemon existed,
  so there was no way to validate it; typos surfaced as "directory not
  found" in the agent pane after the long build.
- The placeholder agent cluttered the navigator and required a
  `prefix + x` (which doesn't exist yet) to clean up after a failed
  bootstrap.

The fix: keep the modal open through the whole flow. Lock the path
zone with a per-stage spinner during prepare, unlock it with a remote
folder picker once the daemon is reachable, lock again briefly during
attach, then close once the agent is Ready.

### Scope (as built)

- **Bootstrap split** in `crates/codemuxd-bootstrap`:
  - `prepare_remote(runner, on_stage, host) -> Result<PreparedHost>` —
    probe + tarball stage + scp + remote build. Returns the remote
    `$HOME` so the modal can seed the folder picker.
  - `attach_agent(runner, on_stage, prepared, host, agent_id, cwd,
    socket_dir, rows, cols) -> Result<AgentTransport>` — daemon spawn
    + tunnel + handshake.
  - The legacy `bootstrap()` and `establish_ssh_transport()` entry
    points are deleted; the smoke example calls prepare then attach in
    sequence.
  - `Stage::label() -> &'static str` lifts the human-readable stage
    name out of the runtime so the modal can render it.
- **`RemoteFs`** (new module `crates/codemuxd-bootstrap/src/remote_fs.rs`):
  - Long-lived `ssh -M -N -S {socket} -o ControlPersist=no -o
    ExitOnForwardFailure=yes -o BatchMode=yes {host}` ControlMaster
    spawned during the prepare→attach handoff. Subsequent `list_dir`
    calls reuse it via `ssh -S {socket} {host} -- ls -1pA -- {path}`,
    so each completion request is sub-100 ms even on a slow link.
  - Drop-killable: `Drop` kills the master and unlinks the socket file
    so a cancelled spawn doesn't leak ssh subprocesses.
  - Path argument is char-allowlisted and shell-escaped — same defense
    the daemon-spawn path uses for `--cwd`.
  - `RemoteFs::open` failure is non-fatal: the modal degrades to
    literal-path mode with a wildmenu hint instead of blocking the
    user from typing a remote path by hand.
- **Two-handle worker** in `apps/tui/src/bootstrap_worker.rs`:
  - `start_prepare(host) -> PrepareHandle` — owned by the modal
    between the user "committing" a host and selecting a folder.
  - `start_attach(prepared, host, agent_id, cwd, rows, cols) ->
    AttachHandle` — owned by the runtime until the agent goes Ready.
  - Both use the existing `CancelableRunner` decorator from Stage 5.
  - `start_full_pipeline` is kept as a legacy single-handle shim for
    the cancel-mid-bootstrap regression test and ad-hoc smoke runs.
- **Spawn modal** (`apps/tui/src/spawn.rs`):
  - New `bootstrap_view: Option<BootstrapView>` field — pure render
    data (host, current stage, started_at). Setters
    `lock_for_bootstrap` / `set_bootstrap_stage` /
    `unlock_for_remote_path` / `unlock_back_to_host` are driven by the
    runtime as worker events arrive.
  - New `path_mode: PathMode { Local | Remote { remote_home, cache } }` —
    keeps the per-directory completion cache so prefix-narrowing
    keystrokes filter in process and only crossing a `/` re-shells.
  - `DirLister<'a>` borrow-only enum (Local / Remote { fs, runner })
    supplied per keystroke by the runtime; no `Box<dyn>` allocation in
    the hot path.
  - New outcomes `ModalOutcome::PrepareHost` (Tab from host with text
    or Enter on host with empty path) and `ModalOutcome::CancelBootstrap`
    (Esc / `@` while locked).
- **Runtime** (`apps/tui/src/runtime.rs`):
  - `AgentState::Bootstrapping` removed. Two states left: `Failed` (so
    bootstrap errors still have a render surface after the modal
    closes) and `Ready`.
  - New `prepare: Option<PendingPrepare>` for the in-flight prepare
    phase (modal owns one prepare at a time) and
    `attaches: Vec<PendingAttach>` so the user can fire-and-forget
    multiple spawns in quick succession. The "modal-owned" attach is
    flagged so cancel finds it; detached attaches keep running on
    their own thread.
  - Drain phase: prepare events first (synchronous `RemoteFs::open` on
    `Done(Ok)`), then attach events (deferred mutation — collect
    `finished_attaches`, `new_agents`, `focus_new`, `close_modal`
    during iteration, apply after — to avoid borrow conflicts).
  - Modal dispatch: `PrepareHost` kicks off `start_prepare` and locks
    the modal; `CancelBootstrap` drops the prepare or modal-owned
    attach; remote `Spawn` takes the prepared host out, drops the
    `RemoteFs`, kicks off `start_attach`, and re-locks the modal until
    the attach completes.

### Tests

- `crates/codemuxd-bootstrap`: `prepare_remote` happy path,
  `attach_agent` happy path, per-stage failure tests stay where their
  stage now lives. New `remote_fs::tests` covers `open`+`Drop` not
  leaking the master subprocess, `list_dir` parsing, and quote
  rejection (mirrors `spawn_remote_daemon_rejects_quote_in_cwd`).
- `apps/tui/src/bootstrap_worker.rs`: `cancel_short_circuits_at_next_subprocess_call`
  duplicated for both `PrepareHandle` and `AttachHandle`, plus the
  legacy `start_full_pipeline` test as the regression target.
- `apps/tui/src/spawn.rs`: `lock_for_bootstrap` /
  `set_bootstrap_stage` / `unlock_*` setters, new outcomes,
  `Remote` path mode round-trip via a `ScriptedRunner` mock that
  intercepts the `ssh -S {socket} -- ls` invocation. Covers cache hit
  on prefix-narrowing keystrokes and cache miss on `/` traversal.
- `apps/tui/src/runtime.rs`: tests for `AgentState::Bootstrapping`
  removed; `format_bootstrap_error_*` kept (still used by the Failed
  renderer).

### Exit criteria

End-to-end smoke (manual, single-user):

1. `prefix + c`, type `@<host>`, Tab. Path zone locks with spinner;
   stages cycle (probing → uploading → building → daemon → tunnel →
   connect).
2. During build, hit `Esc`. Focus returns to host zone with text
   preserved; the worker thread shuts down (verifiable via
   `RUST_LOG=codemux=debug`).
3. Re-do the spawn, let prepare finish. Path zone unlocks and shows
   `~`-relative entries from remote `$HOME`. Type a prefix; completions
   filter without a new `list_dir` call. Cross a `/`; new `list_dir`
   fires.
4. Hit Enter. Path zone re-locks briefly (DaemonSpawn → SocketTunnel
   → SocketConnect), then modal closes and the agent appears in the
   navigator already at the chosen cwd.
5. Repeat the spawn while the previous attach is still in flight
   (rapid `prefix + c`). Both attaches complete; both agents land in
   the navigator.
6. Failure modes: unreachable host renders the error inline in the
   modal (not a full-screen pane); modal returns to host zone after
   Esc. `RemoteFs::open` failure (e.g. ssh socket forwarding blocked)
   degrades to literal-path mode with a wildmenu hint.

---

## Verification

### Per-stage gates

- `just check` (fmt --check + clippy strict + workspace tests) at the end
  of every stage. Cannot proceed if it fails.
- Inline tests added per stage as listed.

### Mid-implementation things to verify (cannot promise from the plan)

- `setsid -f` actually detaches the daemon when the originating SSH
  session is killed. Test in Stage 0+: spawn over SSH, kill SSH, daemon
  survives.
- OpenSSH unix-socket forwarding via `-L /local:/remote` is reliable on
  the user's devpods. Fallback if not: socat TCP bridge.
- Signal forwarding (Ctrl-C from local through Resize/Signal frames)
  reaches the remote claude correctly. Test in Stage 2.

---

## Plan provenance

Original plan: `~/.claude/plans/cosmic-zooming-star.md` (snapshot at
start of Stage 0). This doc is the live tracker; the plan file is the
historical record. If they diverge, this doc wins.
