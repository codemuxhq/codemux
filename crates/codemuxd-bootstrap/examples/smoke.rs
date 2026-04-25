//! End-to-end smoke driver for the SSH bootstrap pipeline.
//!
//! Runs the full 7-step bootstrap against a real host, performs the wire
//! handshake, sends a small `PtyData` write, drains a few PTY chunks, then
//! kills the remote child cleanly. Lives outside the unit-test surface so
//! it doesn't run in `cargo test` (no host available in CI), but stays in
//! tree so it can be invoked by hand for verification:
//!
//! ```text
//! cargo run --example smoke -p codemuxd-bootstrap -- <hostname>
//! ```
//!
//! Prints each stage's outcome to stderr so a hang is visible — if the
//! `daemon spawn` line lands but `tunnel` never does, the ssh subprocess
//! is hanging on inherited pipes (the bug the `</dev/null >/dev/null 2>&1`
//! redirect in `spawn_remote_daemon` exists to prevent).

use std::time::{Duration, Instant};

use codemuxd_bootstrap::{RealRunner, default_local_socket_dir, establish_ssh_transport};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = std::env::args().nth(1).ok_or(
        "usage: cargo run --example smoke -p codemuxd-bootstrap -- <hostname>\n\
         (a real ssh-reachable host with `cargo` installed)",
    )?;

    eprintln!("→ bootstrap: host={host}");
    let started = Instant::now();
    let socket_dir = default_local_socket_dir()?;
    let mut transport = establish_ssh_transport(
        &RealRunner,
        |stage| eprintln!("  · stage: {stage:?}"),
        &host,
        "smoke-agent",
        None, // inherit remote $HOME
        &socket_dir,
        24,
        80,
    )?;
    eprintln!("✓ transport established in {:.2?}", started.elapsed());

    eprintln!("→ writing 'echo smoke-test\\n' to remote PTY");
    transport.write(b"echo smoke-test\n")?;

    let drain_until = Instant::now() + Duration::from_secs(3);
    let mut total_bytes = 0usize;
    while Instant::now() < drain_until {
        for chunk in transport.try_read() {
            total_bytes += chunk.len();
            let preview = String::from_utf8_lossy(&chunk[..chunk.len().min(80)]);
            eprintln!("  chunk ({} bytes): {:?}", chunk.len(), preview);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("✓ drained {total_bytes} total bytes from remote PTY");

    eprintln!("→ killing remote child");
    transport.kill()?;
    let kill_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(code) = transport.try_wait() {
            eprintln!("✓ remote child exited with code {code}");
            break;
        }
        if Instant::now() > kill_deadline {
            eprintln!("⚠ remote child did not exit within 2s of kill");
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    drop(transport); // drops tunnel subprocess too
    eprintln!("✓ smoke test complete (total {:.2?})", started.elapsed());
    Ok(())
}
