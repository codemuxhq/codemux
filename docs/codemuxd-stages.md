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
- ⏳ **Stage 3** — `AgentTransport` enum + `LocalPty` (refactor only)
- ⏳ **Stage 4** — `SshDaemonPty` adapter + bootstrap
- ⏳ **Stage 5** — Wire SSH transport into the spawn modal

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

### Scope

- A "bootstrapping..." placeholder agent appears in the navigator while
  the bootstrap runs. **Critical**: the bootstrap must NOT block the
  event loop:
  - Spawn the bootstrap on a worker thread
  - Drain progress events through a crossbeam channel alongside PTY data
    (re-use the `BootstrapStage` enum to render granular status if useful)
  - On success, transition the placeholder agent into a real running
    agent (transport swap + status flip)
  - On failure, surface the actionable error message in the navigator
    status line; keep the placeholder visible until the user dismisses
    it
- Cancellation: dropping the placeholder agent mid-bootstrap must
  signal the worker thread to stop (channel close + `Child::kill` on
  any in-flight ssh subprocess)

### Tests

- Worker-thread cancellation test — start a bootstrap with a slow ssh
  mock, drop the placeholder, assert no leaked subprocesses
- Manual end-to-end smoke per the script in **Verification** below
  (no automated SSH integration test — that requires real network)

### Exit criteria

End-to-end smoke passes:

1. Reachable SSH host (e.g. an Uber devpod) with `cargo` installed
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
   This reproduces Scenario 1 from `docs/use-cases.md`

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
