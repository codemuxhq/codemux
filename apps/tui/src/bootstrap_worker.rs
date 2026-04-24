//! Off-thread driver for the SSH bootstrap.
//!
//! The TUI event loop polls at ~20 Hz (`runtime::FRAME_POLL` = 50 ms);
//! the SSH bootstrap can take 30-60 s on first contact (the
//! `cargo build --release` step over the wire dominates). Running it
//! inline would freeze every other agent's rendering for the whole
//! window. This module spawns a worker thread that drives
//! [`codemuxd_bootstrap::establish_ssh_transport`] to completion and
//! makes the result available to the runtime through a non-blocking
//! [`crossbeam_channel`].
//!
//! Cancellation is best-effort: a [`CancelableRunner`] decorator
//! shorts the worker between subprocess calls. A subprocess already in
//! flight (e.g. `cargo build`) cannot be aborted from here without
//! threading subprocess kill plumbing through the [`CommandRunner`]
//! trait — deliberate scope cap. The user's typical "wait, wrong
//! host" cancel happens before the long-running build step anyway,
//! and any leaked ssh subprocess will die on its own when the network
//! call returns.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use codemux_session::AgentTransport;
use codemuxd_bootstrap::{
    self, CommandOutput, CommandRunner, RealRunner, default_local_socket_dir,
    establish_ssh_transport,
};
use crossbeam_channel::{Receiver, bounded};

/// Handle to an in-flight SSH bootstrap.
///
/// The runtime polls [`Self::try_recv`] every event-loop tick and
/// transitions the placeholder agent into a real one once the worker
/// reports success. [`Drop`] is cooperative: it sets the cancel flag
/// so the worker exits at the next subprocess boundary. The
/// `JoinHandle` is detached (Rust's default `JoinHandle::drop`
/// semantics) so the TUI never blocks on a slow bootstrap.
pub struct BootstrapHandle {
    cancel: Arc<AtomicBool>,
    rx: Receiver<Result<AgentTransport, codemuxd_bootstrap::Error>>,
    /// Kept only to anchor the worker thread's lifetime in the type
    /// system. We never join — `JoinHandle::drop` detaches, which is
    /// the behavior we want (`Drop` must not block the TUI).
    _join: thread::JoinHandle<()>,
}

impl BootstrapHandle {
    /// Non-blocking poll for the worker's result. `None` = still in
    /// flight, `Some(Ok(t))` = ready to swap into the runtime,
    /// `Some(Err(e))` = bootstrap failed (the runtime renders the
    /// stage-tagged error in the placeholder pane).
    #[must_use]
    pub fn try_recv(&self) -> Option<Result<AgentTransport, codemuxd_bootstrap::Error>> {
        self.rx.try_recv().ok()
    }

    /// Signal the worker to stop at the next subprocess boundary.
    /// Idempotent. The worker still completes any in-flight subprocess
    /// call before observing the flag.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

impl Drop for BootstrapHandle {
    fn drop(&mut self) {
        // Cooperative cancel — the worker checks the flag at its next
        // subprocess call and bails. `JoinHandle::drop` detaches the
        // thread; we deliberately don't join, so the TUI doesn't block
        // when the user dismisses a placeholder mid-bootstrap.
        self.cancel();
    }
}

/// Spawn a worker thread that runs the production SSH bootstrap end
/// to end. Returns immediately; poll the returned [`BootstrapHandle`]
/// for the result.
pub fn start(
    host: String,
    agent_id: String,
    cwd: PathBuf,
    rows: u16,
    cols: u16,
) -> BootstrapHandle {
    start_with_runner(Box::new(RealRunner), host, agent_id, cwd, rows, cols)
}

/// Test-friendly entry point: inject a [`CommandRunner`] so the
/// cancel-mid-bootstrap path can be exercised without touching the
/// network. Production calls [`start`] which delegates here with
/// [`RealRunner`].
pub fn start_with_runner(
    runner: Box<dyn CommandRunner>,
    host: String,
    agent_id: String,
    cwd: PathBuf,
    rows: u16,
    cols: u16,
) -> BootstrapHandle {
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = bounded(1);
    let cancel_for_thread = Arc::clone(&cancel);
    let join = thread::spawn(move || {
        let cancelable = CancelableRunner {
            inner: runner,
            cancel: cancel_for_thread,
        };
        let socket_dir = match default_local_socket_dir() {
            Ok(d) => d,
            Err(e) => {
                // Receiver may already be dropped; we don't care.
                let _ = tx.send(Err(e));
                return;
            }
        };
        let result =
            establish_ssh_transport(&cancelable, &host, &agent_id, &cwd, &socket_dir, rows, cols);
        let _ = tx.send(result);
    });
    BootstrapHandle {
        cancel,
        rx,
        _join: join,
    }
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
        if self.cancel.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "bootstrap cancelled",
            ));
        }
        self.inner.run(program, args)
    }

    fn spawn_detached(&self, program: &str, args: &[&str]) -> std::io::Result<std::process::Child> {
        if self.cancel.load(Ordering::SeqCst) {
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
            // Should not be reached: the test cancels before the
            // bootstrap reaches the SocketTunnel stage. If we somehow
            // get here we'd return an Err so the worker exits cleanly.
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

    /// Cancelling the [`BootstrapHandle`] short-circuits the worker at
    /// the next subprocess boundary: the in-flight call finishes, the
    /// next stage's call goes through [`CancelableRunner`] which
    /// returns `Interrupted` without touching the inner runner.
    /// Verified by counting the inner runner's recorded calls.
    #[test]
    fn cancel_short_circuits_at_next_subprocess_call() {
        let (runner, started_rx, release_tx) = BlockingRunner::new();
        let runner_arc: Arc<dyn CommandRunner + Send + Sync> = runner.clone();
        let handle = start_with_runner(
            Box::new(ArcRunner(runner_arc)),
            "host".into(),
            "agent-1".into(),
            PathBuf::from("/tmp/x"),
            24,
            80,
        );

        // Step 1: wait for the first subprocess call (the version
        // probe) to start. Any timeout here points at a regression in
        // the worker startup path, not at cancellation.
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker should issue its first call within 2s");

        // Step 2: arm cancellation, *then* let the in-flight call
        // return. Order matters: the SeqCst store must happen-before
        // the channel send so the worker's next CancelableRunner.run
        // sees the flag.
        handle.cancel();
        let _ = release_tx.send(());

        // Step 3: poll for the worker's result. Cancellation surfaces
        // as a Bootstrap error from a later stage (typically Scp,
        // since stage 1 returned Ok with empty stdout → bootstrap
        // proceeds to stage 2 which calls runner.run for `mkdir`).
        let deadline = Instant::now() + Duration::from_secs(5);
        let result = loop {
            if let Some(r) = handle.try_recv() {
                break r;
            }
            assert!(
                Instant::now() <= deadline,
                "worker did not finish within 5s of cancel"
            );
            thread::sleep(Duration::from_millis(20));
        };
        assert!(
            result.is_err(),
            "worker should report a Bootstrap error after cancel"
        );

        // Step 4: the inner runner should have been called exactly
        // once. The CancelableRunner intercepted call #2 before it
        // reached the inner runner.
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

    /// `BootstrapHandle::cancel` is idempotent — calling it twice
    /// (e.g. once explicitly, once via Drop) does not panic or
    /// double-deliver.
    #[test]
    fn cancel_is_idempotent() {
        let (runner, started_rx, release_tx) = BlockingRunner::new();
        let runner_arc: Arc<dyn CommandRunner + Send + Sync> = runner.clone();
        let handle = start_with_runner(
            Box::new(ArcRunner(runner_arc)),
            "host".into(),
            "agent-1".into(),
            PathBuf::from("/tmp/x"),
            24,
            80,
        );
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker should issue its first call within 2s");
        handle.cancel();
        handle.cancel();
        let _ = release_tx.send(());
        // Drop also calls cancel; that's the third invocation. Must
        // not panic.
        drop(handle);
    }
}
