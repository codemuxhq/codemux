//! PTY spawning for the daemon.
//!
//! Adapted from `apps/tui/src/runtime.rs::spawn_agent` (P0 / P1 walking
//! skeleton). The daemon owns the same shape — master + writer + child +
//! reader-channel — so the supervisor can write to the PTY synchronously
//! from the conn loop and drain output without blocking that loop.

use std::io::{Read, Write};
use std::path::Path;
use std::thread;

use crossbeam_channel::{Receiver, unbounded};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::error::Error;

/// PTY read chunk size. Matches `apps/tui/src/runtime.rs::READ_BUFFER_SIZE`
/// for consistency — 8 KiB is a comfortable middle ground between syscall
/// overhead and queue burst size for terminal output.
const READ_BUFFER_SIZE: usize = 8 * 1024;

/// A child process attached to a PTY, decomposed for the supervisor's
/// concurrent access pattern: `writer` is moved into the conn loop on the
/// accept thread; `rx` is drained from the same loop; `master` stays here
/// for resize calls (Stage 1+); `child` is held for `try_wait` / `kill`.
pub struct PtyChild {
    pub master: Box<dyn MasterPty + Send>,
    pub writer: Box<dyn Write + Send>,
    pub child: Box<dyn Child + Send + Sync>,
    pub rx: Receiver<Vec<u8>>,
}

/// Spawn `command args...` inside a fresh PTY of size `rows x cols`,
/// optionally with `cwd` as the working directory.
pub fn spawn(
    command: &str,
    args: &[String],
    cwd: Option<&Path>,
    rows: u16,
    cols: u16,
) -> Result<PtyChild, Error> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| Error::Pty {
            source: Box::new(std::io::Error::other(format!("open pty: {e}"))),
        })?;

    let mut cmd = CommandBuilder::new(command);
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.cwd(cwd);
    }
    let child = pair.slave.spawn_command(cmd).map_err(|e| Error::Spawn {
        command: command.to_string(),
        source: Box::new(std::io::Error::other(format!("spawn: {e}"))),
    })?;
    // Closing the slave fd on the parent side is required so the child
    // sees EOF on its tty when it exits — without this, the master read
    // never returns EOF and the reader thread would spin.
    drop(pair.slave);

    let writer = pair.master.take_writer().map_err(|e| Error::Pty {
        source: Box::new(std::io::Error::other(format!("take writer: {e}"))),
    })?;
    let reader = pair.master.try_clone_reader().map_err(|e| Error::Pty {
        source: Box::new(std::io::Error::other(format!("clone reader: {e}"))),
    })?;
    let master = pair.master;
    let rx = spawn_reader_thread(reader);
    Ok(PtyChild {
        master,
        writer,
        child,
        rx,
    })
}

/// Background reader: drains the PTY master and pushes chunks into a
/// channel. Exits on EOF or read error (including the master being dropped
/// by the supervisor, which will close the underlying fd).
fn spawn_reader_thread(mut reader: Box<dyn Read + Send>) -> Receiver<Vec<u8>> {
    let (tx, rx) = unbounded::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = vec![0u8; READ_BUFFER_SIZE];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn spawn_short_lived_child_reaps_cleanly() -> Result<(), Box<dyn std::error::Error>> {
        // `true` exits 0 immediately. We give the child up to 2s to be
        // collectable; on slow CI this could conceivably take longer, but
        // local laptops finish in milliseconds.
        let mut pty = spawn("true", &[], None, 24, 80)?;

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = pty.child.try_wait()? {
                assert!(status.success(), "child `true` should exit 0");
                return Ok(());
            }
            assert!(
                Instant::now() < deadline,
                "child `true` did not exit within 2s",
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn spawn_missing_command_returns_spawn_error() {
        let result = spawn(
            "definitely-not-a-real-command-xyz-codemuxd",
            &[],
            None,
            24,
            80,
        );
        let Err(err) = result else {
            unreachable!("spawn of nonexistent command should fail");
        };
        assert!(
            matches!(err, Error::Spawn { .. }),
            "expected Error::Spawn, got {err:?}",
        );
    }

    /// Exercises the args-passing and cwd branches: `pwd -P` resolves
    /// symlinks and prints its working directory; we set `cwd` to a
    /// tempdir and verify the canonicalized path shows up in PTY output.
    #[test]
    fn spawn_with_cwd_sets_child_working_directory() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        // macOS `/tmp` is a symlink to `/private/tmp`; canonicalizing both
        // sides keeps the contains-check honest.
        let expected = std::fs::canonicalize(dir.path())?;
        let mut pty = spawn("pwd", &["-P".to_string()], Some(dir.path()), 24, 80)?;

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = Vec::new();
        while Instant::now() < deadline {
            if let Ok(chunk) = pty.rx.recv_timeout(Duration::from_millis(50)) {
                got.extend_from_slice(&chunk);
                let s = String::from_utf8_lossy(&got);
                if s.contains(&*expected.to_string_lossy()) {
                    let _ = pty.child.kill();
                    let _ = pty.child.wait();
                    return Ok(());
                }
            }
            if pty.child.try_wait()?.is_some() && got.is_empty() {
                break;
            }
        }
        let _ = pty.child.kill();
        let _ = pty.child.wait();
        panic!(
            "expected pwd output to contain {expected:?}, got {:?}",
            String::from_utf8_lossy(&got),
        );
    }
}
