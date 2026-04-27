//! Off-thread drivers for the SSH bootstrap.
//!
//! The TUI event loop polls at ~20 Hz (`runtime::FRAME_POLL` = 50 ms);
//! the SSH bootstrap can take 30-60 s on first contact (the
//! `cargo build --release` step over the wire dominates). Running it
//! inline would freeze every other agent's rendering for the whole
//! window. This module spawns worker threads that drive
//! [`codemuxd_bootstrap::prepare_remote`] and
//! [`codemuxd_bootstrap::attach_agent`] to completion and make the
//! results available to the runtime through non-blocking
//! [`crossbeam_channel`]s.
//!
//! Two handles, mirroring the bootstrap library's split:
//! - [`PrepareHandle`] runs only the probe + install phase and
//!   produces a [`codemuxd_bootstrap::PreparedHost`]. Owned by the
//!   spawn modal between the user "committing" a host and selecting a
//!   remote folder. Cheap to cancel: the only blocking subprocess in
//!   prepare is `cargo build`, and the user typically cancels before
//!   that step starts.
//! - [`AttachHandle`] runs only the daemon spawn + tunnel + handshake
//!   given a `PreparedHost`, producing an [`AgentTransport`]. Owned by
//!   the runtime until the agent transitions into Ready.
//!
//! Cancellation is best-effort: a [`CancelableRunner`] decorator
//! shorts the worker between subprocess calls. A subprocess already in
//! flight (e.g. `cargo build`) cannot be aborted from here without
//! threading subprocess kill plumbing through the [`CommandRunner`]
//! trait — deliberate scope cap. Any leaked ssh subprocess will die on
//! its own when the network call returns.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;

use codemux_session::AgentTransport;
use codemuxd_bootstrap::{
    self, AttachConfig, CommandOutput, CommandRunner, PreparedHost, RealRunner, RemoteFs, Stage,
    attach_agent, default_local_socket_dir, prepare_remote,
};
use crossbeam_channel::{Receiver, unbounded};

/// Stream of events emitted by a [`PrepareHandle`]'s worker thread, in
/// the order the prepare pipeline produces them. The owner drains all
/// available events on each frame poll: every `Stage(_)` updates the
/// modal's locked status row, and the terminating `Done(_)` triggers
/// the unlock-and-pick-folder transition.
///
/// Modeled as one channel rather than two (e.g. separate "progress"
/// and "result" channels) so the owner can't see a `Done` before the
/// last `Stage`. `Done` is always the final event; the channel goes
/// empty after it.
#[derive(Debug)]
pub enum PrepareEvent {
    /// The named stage just started executing on the worker thread.
    /// During prepare, only the first 4 stages are emitted
    /// (`VersionProbe`, `TarballStage`, `Scp`, `RemoteBuild`); the
    /// fast path that hits a cached host emits only `VersionProbe`.
    Stage(Stage),
    /// Prepare finished. `Ok` carries a [`PrepareSuccess`] (named
    /// fields beat a tuple so the call site reads as
    /// `prepared.host.remote_home` / `prepared.fs` rather than
    /// positional `.0` / `.1`); `Err` flips the modal status row to a
    /// stage-tagged error and unlocks back to the host zone.
    Done(Result<PrepareSuccess, codemuxd_bootstrap::Error>),
}

/// Success payload of [`PrepareEvent::Done`]. `prepared` is the
/// [`PreparedHost`] returned by [`prepare_remote`] (carries the remote
/// `$HOME` so the modal can seed remote-path autocomplete). `fs` is the
/// ssh `ControlMaster` the worker opened immediately after the
/// bootstrap stages succeeded so the main thread isn't blocked on a
/// synchronous [`RemoteFs::open`] during the post-`Done` drain (which
/// would freeze the spinner for cached hosts where prepare returns in
/// <100 ms). `None` means [`RemoteFs::open`] failed; the modal then
/// degrades to literal-path mode (the wildmenu stays empty but Enter
/// still spawns at the typed path).
#[derive(Debug)]
pub struct PrepareSuccess {
    pub prepared: PreparedHost,
    pub fs: Option<RemoteFs>,
}

/// Stream of events emitted by an [`AttachHandle`]'s worker thread, in
/// the order the attach pipeline produces them. `Stage(_)` updates
/// the modal's second locked status row (after the user picked a
/// folder); `Done(_)` triggers the agent's transition into Ready or
/// Failed.
///
/// `Debug` is implemented by hand because [`AgentTransport`] carries
/// an open PTY/socket and deliberately doesn't implement `Debug`; the
/// success arm prints opaquely (`Done(Ok(<transport>))`) so the wire
/// protocol bytes never accidentally leak through `format!("{:?}")`.
pub enum AttachEvent {
    /// The named stage just started executing on the worker thread.
    /// Only the last 3 stages are emitted (`DaemonSpawn`,
    /// `SocketTunnel`, `SocketConnect`) — the prepare phase is
    /// reported separately via [`PrepareEvent`].
    Stage(Stage),
    /// Attach finished — `Ok(transport)` is ready to swap into the
    /// runtime as a `Ready` agent; `Err` flips the agent state to
    /// `Failed` with the stage-tagged error rendered in red.
    Done(Result<AgentTransport, codemuxd_bootstrap::Error>),
}

impl std::fmt::Debug for AttachEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stage(stage) => f.debug_tuple("Stage").field(stage).finish(),
            Self::Done(Ok(_)) => f.write_str("Done(Ok(<transport>))"),
            Self::Done(Err(e)) => f.debug_tuple("Done").field(&Err::<(), _>(e)).finish(),
        }
    }
}

/// Handle to an in-flight SSH prepare phase.
///
/// The owner polls [`Self::try_recv`] every event-loop tick and
/// transitions UI state once the worker reports `Done`. [`Drop`] is
/// cooperative: it sets the cancel flag so the worker exits at the
/// next subprocess boundary. The worker thread is detached at
/// `thread::spawn` time — the TUI never blocks on a slow prepare.
pub struct PrepareHandle {
    cancel: Arc<AtomicBool>,
    rx: Receiver<PrepareEvent>,
}

impl PrepareHandle {
    /// Non-blocking poll for the worker's next event. `None` = no
    /// event ready right now (still in flight), `Some(_)` = event
    /// dequeued. The owner drains in a tight loop until `None` so
    /// queued progress events don't render stale.
    #[must_use]
    pub fn try_recv(&self) -> Option<PrepareEvent> {
        self.rx.try_recv().ok()
    }

    /// Signal the worker to stop at the next subprocess boundary.
    /// Idempotent. The worker still completes any in-flight subprocess
    /// call before observing the flag.
    pub fn cancel(&self) {
        self.cancel.store(true, Relaxed);
    }
}

impl Drop for PrepareHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Handle to an in-flight SSH attach phase.
///
/// Same shape as [`PrepareHandle`] but yields [`AttachEvent`]s and
/// terminates with an [`AgentTransport`] rather than a
/// [`PreparedHost`].
pub struct AttachHandle {
    cancel: Arc<AtomicBool>,
    rx: Receiver<AttachEvent>,
}

impl AttachHandle {
    /// Non-blocking poll for the worker's next event. See
    /// [`PrepareHandle::try_recv`].
    #[must_use]
    pub fn try_recv(&self) -> Option<AttachEvent> {
        self.rx.try_recv().ok()
    }

    /// Signal the worker to stop at the next subprocess boundary.
    pub fn cancel(&self) {
        self.cancel.store(true, Relaxed);
    }
}

impl Drop for AttachHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Spawn a worker thread that runs only the prepare phase
/// (`prepare_remote`) and reports its progress via [`PrepareHandle`].
///
/// Production calls this when the spawn modal sees the user "commit"
/// a host (Tab from host zone with text, or Enter on host with empty
/// path). The handle is owned by the modal until prepare returns;
/// dropping it cancels.
pub fn start_prepare(host: String) -> PrepareHandle {
    start_prepare_with_runner(Box::new(RealRunner), host)
}

/// Test-friendly entry point: inject a [`CommandRunner`] so the
/// cancel-mid-prepare path can be exercised without touching the
/// network.
pub fn start_prepare_with_runner(runner: Box<dyn CommandRunner>, host: String) -> PrepareHandle {
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = unbounded();
    let cancel_for_thread = Arc::clone(&cancel);
    thread::spawn(move || {
        let cancelable = CancelableRunner {
            inner: runner,
            cancel: cancel_for_thread,
        };
        let tx_for_stage = tx.clone();
        let on_stage = move |stage: Stage| {
            let _ = tx_for_stage.send(PrepareEvent::Stage(stage));
        };
        let result = prepare_remote(&cancelable, &on_stage, &host).map(|prepared| {
            // Open the ssh ControlMaster on the worker thread (rather
            // than on the main thread after `Done` arrives) so the
            // 100 ms – 3 s `RemoteFs::open` poll for the control socket
            // doesn't block the runtime's render loop. On cached hosts
            // `prepare_remote` finishes in <100 ms with only one
            // `Stage(VersionProbe)` event; if `RemoteFs::open` runs on
            // the main thread the spinner freezes on the pre-stage
            // "starting…" frame for the entire open, then the modal
            // jumps straight to remote-path mode. Doing it here keeps
            // the spinner spinning on the last bootstrap stage label
            // ("probing host" for cached hosts, "building remote
            // daemon" for fresh hosts) until the open returns.
            //
            // Failure is non-fatal: log + degrade to literal-path mode
            // (matches the runtime's prior behavior — the wildmenu is
            // best-effort autocomplete, the path field is the source
            // of truth and Enter still spawns at the typed path).
            let fs = match RemoteFs::open(&host) {
                Ok(fs) => Some(fs),
                Err(e) => {
                    tracing::warn!(
                        host = %host,
                        error = %e,
                        "RemoteFs::open failed; modal will use literal-path mode",
                    );
                    None
                }
            };
            PrepareSuccess { prepared, fs }
        });
        let _ = tx.send(PrepareEvent::Done(result));
    });
    PrepareHandle { cancel, rx }
}

/// Spawn a worker thread that runs only the attach phase
/// (`attach_agent`) given a [`PreparedHost`], reporting progress via
/// [`AttachHandle`].
///
/// Production calls this once the modal closes with a chosen remote
/// path. The handle is owned by the runtime until the agent's
/// transport is returned via `Done(Ok(_))`; dropping it cancels.
pub fn start_attach(
    prepared: PreparedHost,
    host: String,
    agent_id: String,
    cwd: Option<PathBuf>,
    rows: u16,
    cols: u16,
) -> AttachHandle {
    start_attach_with_runner(
        Box::new(RealRunner),
        prepared,
        host,
        agent_id,
        cwd,
        rows,
        cols,
    )
}

/// Test-friendly entry point for the attach phase.
pub fn start_attach_with_runner(
    runner: Box<dyn CommandRunner>,
    prepared: PreparedHost,
    host: String,
    agent_id: String,
    cwd: Option<PathBuf>,
    rows: u16,
    cols: u16,
) -> AttachHandle {
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = unbounded();
    let cancel_for_thread = Arc::clone(&cancel);
    thread::spawn(move || {
        let cancelable = CancelableRunner {
            inner: runner,
            cancel: cancel_for_thread,
        };
        let socket_dir = match default_local_socket_dir() {
            Ok(d) => d,
            Err(e) => {
                let _ = tx.send(AttachEvent::Done(Err(e)));
                return;
            }
        };
        let cfg = AttachConfig {
            host,
            agent_id,
            cwd,
            local_socket_dir: socket_dir,
            rows,
            cols,
        };
        let tx_for_stage = tx.clone();
        let on_stage = move |stage: Stage| {
            let _ = tx_for_stage.send(AttachEvent::Stage(stage));
        };
        let result = attach_agent(&cancelable, &on_stage, &prepared, &cfg);
        let _ = tx.send(AttachEvent::Done(result));
    });
    AttachHandle { cancel, rx }
}

/// Wraps a [`CommandRunner`] with a cancel flag. Each subprocess call
/// checks the flag first and short-circuits with an `Interrupted`
/// `io::Error` when set; the surrounding bootstrap stage maps that
/// into a stage-tagged [`codemuxd_bootstrap::Error::Bootstrap`].
struct CancelableRunner {
    inner: Box<dyn CommandRunner>,
    cancel: Arc<AtomicBool>,
}

impl CommandRunner for CancelableRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CommandOutput> {
        if self.cancel.load(Relaxed) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "bootstrap cancelled",
            ));
        }
        self.inner.run(program, args)
    }

    fn spawn_detached(&self, program: &str, args: &[&str]) -> std::io::Result<std::process::Child> {
        if self.cancel.load(Relaxed) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "bootstrap cancelled",
            ));
        }
        self.inner.spawn_detached(program, args)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver as StdReceiver, Sender as StdSender, channel};
    use std::time::{Duration, Instant};

    use codemuxd_bootstrap::CommandOutput;

    use super::*;

    /// `AttachEvent`'s manual `Debug` impl prints `Stage(...)` via
    /// the inner `Stage`'s derived `Debug` and `Done(Ok(<transport>))`
    /// opaquely (because [`AgentTransport`] doesn't implement `Debug`
    /// and we don't want a logged event to leak wire-protocol bytes).
    /// `Done(Err(_))` falls back to the bootstrap error's `Debug`.
    #[test]
    fn attach_event_debug_redacts_transport_in_done_ok_arm() {
        let stage_event = AttachEvent::Stage(Stage::VersionProbe);
        assert_eq!(format!("{stage_event:?}"), "Stage(VersionProbe)");

        let err_event = AttachEvent::Done(Err(codemuxd_bootstrap::Error::Bootstrap {
            stage: Stage::SocketConnect,
            source: "boom".into(),
        }));
        let formatted = format!("{err_event:?}");
        assert!(
            formatted.starts_with("Done("),
            "expected Done(Err(...)) shape, got {formatted}",
        );
        assert!(
            formatted.contains("SocketConnect"),
            "stage info should bubble through, got {formatted}",
        );
    }

    /// Test runner that:
    ///   1. records every `(program, args)` call,
    ///   2. blocks the *first* call on a one-shot channel until the
    ///      test releases it,
    ///   3. returns success for every call (the test asserts that
    ///      cancellation prevents the runner from being called past
    ///      the first stage — the `CancelableRunner` decorator
    ///      short-circuits subsequent calls before they reach this
    ///      inner runner).
    struct BlockingRunner {
        calls: Mutex<Vec<String>>,
        first_call_started: Mutex<Option<StdSender<()>>>,
        release: Mutex<Option<StdReceiver<()>>>,
    }

    impl BlockingRunner {
        fn new() -> (Arc<Self>, StdReceiver<()>, StdSender<()>) {
            let (started_tx, started_rx) = channel();
            let (release_tx, release_rx) = channel();
            let runner = Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                first_call_started: Mutex::new(Some(started_tx)),
                release: Mutex::new(Some(release_rx)),
            });
            (runner, started_rx, release_tx)
        }

        fn record(&self, program: &str, args: &[&str]) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
        }

        /// On the first call only: notify the test the call is in
        /// flight, then block until the test releases it. Subsequent
        /// calls fall through (no senders/receivers left to take).
        fn block_first_call(&self) {
            let started = self.first_call_started.lock().unwrap().take();
            let release = self.release.lock().unwrap().take();
            if let (Some(started), Some(release)) = (started, release) {
                let _ = started.send(());
                let _ = release.recv();
            }
        }
    }

    impl CommandRunner for BlockingRunner {
        fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CommandOutput> {
            self.record(program, args);
            self.block_first_call();
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }

        fn spawn_detached(
            &self,
            program: &str,
            args: &[&str],
        ) -> std::io::Result<std::process::Child> {
            self.record(program, args);
            Err(std::io::Error::other(
                "BlockingRunner.spawn_detached unexpectedly invoked",
            ))
        }
    }

    /// Forwarding adapter so the worker (which takes `Box<dyn
    /// CommandRunner>`) can drive an `Arc<BlockingRunner>` while the
    /// test keeps a second handle for assertions.
    struct ArcRunner(Arc<dyn CommandRunner + Send + Sync>);

    impl CommandRunner for ArcRunner {
        fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CommandOutput> {
            self.0.run(program, args)
        }

        fn spawn_detached(
            &self,
            program: &str,
            args: &[&str],
        ) -> std::io::Result<std::process::Child> {
            self.0.spawn_detached(program, args)
        }
    }

    /// Cancelling a [`PrepareHandle`] short-circuits the worker at the
    /// next subprocess boundary. The version-probe `ssh` call is
    /// in-flight when we cancel; the next stage's call goes through
    /// [`CancelableRunner`] which returns `Interrupted` without
    /// touching the inner runner. Verified by counting the inner
    /// runner's recorded calls.
    #[test]
    fn cancel_prepare_short_circuits_at_next_subprocess_call() {
        let (runner, started_rx, release_tx) = BlockingRunner::new();
        let runner_arc: Arc<dyn CommandRunner + Send + Sync> = runner.clone();
        let handle = start_prepare_with_runner(Box::new(ArcRunner(runner_arc)), "host".into());

        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker should issue its first call within 2s");

        handle.cancel();
        let _ = release_tx.send(());

        let deadline = Instant::now() + Duration::from_secs(5);
        let result = loop {
            match handle.try_recv() {
                Some(PrepareEvent::Done(r)) => break r,
                Some(PrepareEvent::Stage(_)) => {}
                None => {
                    assert!(
                        Instant::now() <= deadline,
                        "worker did not finish within 5s of cancel"
                    );
                    thread::sleep(Duration::from_millis(20));
                }
            }
        };
        assert!(
            result.is_err(),
            "worker should report a Bootstrap error after cancel"
        );

        let calls = runner.calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "expected exactly 1 inner-runner call, got {calls:?}"
        );
        assert!(
            calls[0].starts_with("ssh "),
            "first call should be the ssh version probe, got {:?}",
            calls[0]
        );
    }

    /// Cancelling an [`AttachHandle`] short-circuits at the next
    /// subprocess boundary, mirroring the prepare test. The first
    /// blocking call is `ssh ... codemuxd ...` (the daemon spawn).
    /// The cancel arrives while that's in flight; the next stage
    /// (`SocketTunnel`'s `spawn_detached`) is intercepted by the
    /// decorator.
    #[test]
    fn cancel_attach_short_circuits_at_next_subprocess_call() {
        use std::path::PathBuf;

        let (runner, started_rx, release_tx) = BlockingRunner::new();
        let runner_arc: Arc<dyn CommandRunner + Send + Sync> = runner.clone();
        let prepared = PreparedHost {
            remote_home: PathBuf::from("/home/test"),
            binary_was_updated: false,
        };
        let handle = start_attach_with_runner(
            Box::new(ArcRunner(runner_arc)),
            prepared,
            "host".into(),
            "agent-1".into(),
            Some(PathBuf::from("/tmp/x")),
            24,
            80,
        );

        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker should issue its first call within 2s");

        handle.cancel();
        let _ = release_tx.send(());

        let deadline = Instant::now() + Duration::from_secs(5);
        let result = loop {
            match handle.try_recv() {
                Some(AttachEvent::Done(r)) => break r,
                Some(AttachEvent::Stage(_)) => {}
                None => {
                    assert!(
                        Instant::now() <= deadline,
                        "worker did not finish within 5s of cancel"
                    );
                    thread::sleep(Duration::from_millis(20));
                }
            }
        };
        assert!(
            result.is_err(),
            "worker should report a Bootstrap error after cancel"
        );

        let calls = runner.calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "expected exactly 1 inner-runner call, got {calls:?}"
        );
        assert!(
            calls[0].starts_with("ssh "),
            "first call should be the ssh daemon spawn, got {:?}",
            calls[0]
        );
    }

    /// `PrepareHandle::cancel` is idempotent — calling it twice does
    /// not panic or double-deliver. Drop also calls cancel; the
    /// handle's worker thread is detached, so this also exercises the
    /// "cancel during drop" path.
    #[test]
    fn prepare_cancel_is_idempotent() {
        let (runner, started_rx, release_tx) = BlockingRunner::new();
        let runner_arc: Arc<dyn CommandRunner + Send + Sync> = runner.clone();
        let handle = start_prepare_with_runner(Box::new(ArcRunner(runner_arc)), "host".into());
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker should issue its first call within 2s");
        handle.cancel();
        handle.cancel();
        let _ = release_tx.send(());
        drop(handle);
    }

    /// `AttachHandle::cancel` is idempotent. Companion to the prepare
    /// idempotency test.
    #[test]
    fn attach_cancel_is_idempotent() {
        let (runner, started_rx, release_tx) = BlockingRunner::new();
        let runner_arc: Arc<dyn CommandRunner + Send + Sync> = runner.clone();
        let prepared = PreparedHost {
            remote_home: PathBuf::from("/home/test"),
            binary_was_updated: false,
        };
        let handle = start_attach_with_runner(
            Box::new(ArcRunner(runner_arc)),
            prepared,
            "host".into(),
            "agent-1".into(),
            Some(PathBuf::from("/tmp/x")),
            24,
            80,
        );
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker should issue its first call within 2s");
        handle.cancel();
        handle.cancel();
        let _ = release_tx.send(());
        drop(handle);
    }

    /// `start_prepare` returns a handle that cancels and drops cleanly
    /// even against an unreachable host. Smoke test for the production
    /// `RealRunner` path.
    #[test]
    fn start_prepare_returns_a_handle_that_drops_cleanly() {
        let handle = start_prepare("192.0.2.1".into());
        handle.cancel();
        drop(handle);
    }

    /// Tiny inner runner shared by the `CancelableRunner`
    /// branch-coverage tests. Records whether `spawn_detached` was
    /// reached and returns an `io::Error` so the test can distinguish
    /// "intercepted by decorator" (Interrupted) from "delegated"
    /// (the inner-error message).
    struct Inner {
        called: Arc<AtomicBool>,
    }

    impl CommandRunner for Inner {
        fn run(&self, _: &str, _: &[&str]) -> std::io::Result<CommandOutput> {
            unreachable!("not used in CancelableRunner branch-coverage tests")
        }

        fn spawn_detached(&self, _: &str, _: &[&str]) -> std::io::Result<std::process::Child> {
            self.called.store(true, Relaxed);
            Err(std::io::Error::other("inner reached"))
        }
    }

    /// `CancelableRunner::spawn_detached` checks the cancel flag and
    /// short-circuits with `Interrupted` when set, mirroring the
    /// `run` arm. The phase-cancel tests only exercise the `run` arm;
    /// this one targets `spawn_detached` so both arms of the decorator
    /// are covered.
    #[test]
    fn cancelable_runner_spawn_detached_short_circuits_when_flag_set() {
        let inner_called = Arc::new(AtomicBool::new(false));
        let cancelable = CancelableRunner {
            inner: Box::new(Inner {
                called: Arc::clone(&inner_called),
            }),
            cancel: Arc::new(AtomicBool::new(true)),
        };
        let err = cancelable
            .spawn_detached("ssh", &["-N"])
            .expect_err("cancel flag set → must error");
        assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
        assert!(
            !inner_called.load(Relaxed),
            "inner spawn_detached must not be reached when cancel is armed"
        );
    }

    /// Companion to the short-circuit test: when the cancel flag is
    /// not set, `CancelableRunner::spawn_detached` delegates to the
    /// inner runner. Together the two tests cover both branches of
    /// the decorator.
    #[test]
    fn cancelable_runner_spawn_detached_delegates_when_flag_unset() {
        let inner_called = Arc::new(AtomicBool::new(false));
        let cancelable = CancelableRunner {
            inner: Box::new(Inner {
                called: Arc::clone(&inner_called),
            }),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let err = cancelable
            .spawn_detached("ssh", &["-N"])
            .expect_err("inner returns an error which must propagate");
        assert!(
            err.to_string().contains("inner reached"),
            "expected inner error to propagate, got {err}"
        );
        assert!(
            inner_called.load(Relaxed),
            "inner spawn_detached must be reached when cancel is unset"
        );
    }
}
