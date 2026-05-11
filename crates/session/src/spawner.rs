//! Port/Adapter for spawning local agent processes.
//!
//! The runtime never reaches for `LocalPty::spawn` directly. It holds a
//! `&dyn AgentSpawner` and asks it to produce an
//! [`AgentTransport`]; production wires up
//! [`BinaryAgentSpawner`], tests inject a fake binary path through the
//! `--agent-bin` CLI flag (clap resolves `CODEMUX_AGENT_BIN` at the
//! parse boundary), so the same `BinaryAgentSpawner` invokes whatever
//! the test pointed at. No `std::env::var` calls in production logic.
//!
//! See `docs/plans/2026-05-10--e2e-testing.md` (T2) for the rationale.

use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::transport::{AgentTransport, LocalPty};

// `Send + Sync` is forward-looking: today the runtime holds the spawner
// as `&dyn AgentSpawner` on the main thread, but adding the bounds now
// lets a future caller put one behind an `Arc` or move it into a worker
// thread without a breaking-change reshuffle of the trait surface.

/// Inputs for [`AgentSpawner::spawn`]. Bundles the five forwarded-
/// to-PTY facts so the trait method takes a single argument; keeps
/// the call sites readable and lets `spawn_local_agent` assemble the
/// request from its own parameters in one place.
///
/// `label` is advisory (tracing breadcrumbs); `cwd`, `args`, `rows`,
/// `cols` are forwarded verbatim to the child as the existing
/// [`LocalPty::spawn`] contract dictates.
pub struct SpawnRequest<'a> {
    pub label: String,
    pub cwd: Option<&'a Path>,
    pub args: &'a [String],
    pub rows: u16,
    pub cols: u16,
}

/// Port for spawning the local-PTY half of an [`AgentTransport`]. The
/// runtime depends on this trait, not on a concrete spawner, so the
/// E2E harness can substitute a different binary (the in-tree
/// `fake_agent`) without forking a code path.
pub trait AgentSpawner: Send + Sync {
    /// Spawn the agent and return an [`AgentTransport::Local`] wrapping
    /// the resulting PTY.
    ///
    /// # Errors
    /// Same envelope as [`LocalPty::spawn`]: [`Error::Pty`] when the
    /// kernel can't allocate a PTY, [`Error::Spawn`] when the binary
    /// can't be launched (typically "not on PATH").
    fn spawn(&self, request: SpawnRequest<'_>) -> Result<AgentTransport, Error>;
}

/// Production [`AgentSpawner`] adapter: launches the configured binary
/// in a fresh local PTY. Holds the `PathBuf` resolved by clap (default
/// `claude`, overridable via `--agent-bin` / `CODEMUX_AGENT_BIN`); the
/// PTY plumbing itself lives in [`LocalPty`].
pub struct BinaryAgentSpawner {
    binary: PathBuf,
}

impl BinaryAgentSpawner {
    /// Wrap the given binary path. The path is whatever clap resolved
    /// at parse time; relative paths are looked up via `$PATH` by
    /// `portable-pty`'s `CommandBuilder`, same as before the trait
    /// existed.
    #[must_use]
    pub fn new(binary: PathBuf) -> Self {
        Self { binary }
    }
}

impl AgentSpawner for BinaryAgentSpawner {
    fn spawn(&self, request: SpawnRequest<'_>) -> Result<AgentTransport, Error> {
        let SpawnRequest {
            label,
            cwd,
            args,
            rows,
            cols,
        } = request;
        // Pass the binary path as `&OsStr` so non-UTF-8 paths reach the
        // PTY spawner verbatim; `to_string_lossy` substitutes U+FFFD and
        // would corrupt those paths before `CommandBuilder` ever sees them.
        LocalPty::spawn(self.binary.as_os_str(), args, label, cwd, rows, cols)
            .map(AgentTransport::Local)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Composition smoke test: trait dispatch through `BinaryAgentSpawner`
    /// yields an `AgentTransport::Local` wrapping a live PTY. Uses `cat`
    /// because it's universally on PATH on the platforms we support and
    /// is the same fixture `AgentTransport::for_test` already relies on
    /// (see `transport.rs::tests::local_transport_round_trips_*`).
    #[test]
    fn binary_agent_spawner_returns_local_transport() {
        let spawner = BinaryAgentSpawner::new(PathBuf::from("cat"));
        let transport = spawner
            .spawn(SpawnRequest {
                label: "test-spawner".into(),
                cwd: None,
                args: &[],
                rows: 24,
                cols: 80,
            })
            .unwrap();
        assert!(
            matches!(transport, AgentTransport::Local(_)),
            "BinaryAgentSpawner must produce AgentTransport::Local",
        );
        // Drop transport here — `LocalPty::drop` reaps the `cat` child.
    }
}
