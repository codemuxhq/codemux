//! CLI surface for `codemuxd`.
//!
//! Stage 2 grows the surface to support real daemon mode: a stable
//! `--agent-id` (used in logs and as the source of derived defaults via
//! `fs_layout::Layout`), an exclusive-acquire `--pid-file`, and a
//! `--log-file` that tracing redirects to when not running in
//! `--foreground`. The three filesystem flags are required when not
//! `--foreground`; foreground mode keeps the Stage 0 surface
//! (`--socket` + the trailing argv) for ergonomic `cargo run` iteration
//! and tests.

use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "codemuxd", version, about = "codemux remote PTY daemon")]
pub struct Cli {
    /// Unix socket path the daemon listens on.
    #[arg(long)]
    pub socket: PathBuf,

    /// Run in the foreground: leave logging on stdout/stderr instead of
    /// redirecting to a log file. Useful for `cargo run` and tests.
    /// When set, `--agent-id`, `--pid-file`, and `--log-file` become
    /// optional.
    #[arg(long, default_value_t = false)]
    pub foreground: bool,

    /// Working directory for the spawned child. If omitted, the child
    /// inherits the daemon's cwd. If set but the directory does not
    /// exist, bind fails with [`Error::CwdNotFound`] before any side
    /// effects (no socket, no pid file) — vision principle 6.
    ///
    /// [`Error::CwdNotFound`]: crate::error::Error::CwdNotFound
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Initial PTY size (rows). The TUI overrides this on first attach
    /// (Stage 1) — the value here is just the bootstrap size.
    #[arg(long, default_value_t = 24)]
    pub rows: u16,

    /// Initial PTY size (cols).
    #[arg(long, default_value_t = 80)]
    pub cols: u16,

    /// Stable identifier for this agent. Required when not
    /// `--foreground`. Appears in tracing fields and is used by Stage 4's
    /// bootstrap (via [`fs_layout::Layout`]) to derive the canonical
    /// `--socket`/`--pid-file`/`--log-file` paths it then passes
    /// explicitly to this binary.
    ///
    /// [`fs_layout::Layout`]: crate::fs_layout::Layout
    #[arg(long, required_unless_present = "foreground")]
    pub agent_id: Option<String>,

    /// Pid file. Acquired exclusively at bind time (`O_CREAT | O_EXCL`).
    /// On contention the daemon checks the held pid for liveness via
    /// `kill -0`; a stale entry is reaped, a live one returns
    /// [`Error::PidFileLocked`]. Required when not `--foreground`.
    ///
    /// [`Error::PidFileLocked`]: crate::error::Error::PidFileLocked
    #[arg(long, required_unless_present = "foreground")]
    pub pid_file: Option<PathBuf>,

    /// Log file. Tracing is redirected here when not `--foreground`,
    /// replacing the Stage 0 stderr behaviour. Required when not
    /// `--foreground`. Parent directories are created on demand.
    #[arg(long, required_unless_present = "foreground")]
    pub log_file: Option<PathBuf>,

    /// Command and arguments to exec inside the PTY. Defaults to `claude`
    /// with no arguments. Use `-- <cmd> [args...]` to override.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

impl Cli {
    /// Resolved command + argv for the PTY child. Returns `("claude", [])`
    /// when no positional arguments were supplied.
    #[must_use]
    pub fn child_command(&self) -> (String, Vec<String>) {
        if let Some((head, tail)) = self.command.split_first() {
            (head.clone(), tail.to_vec())
        } else {
            ("claude".to_string(), Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;

    use super::*;

    #[test]
    fn defaults_to_claude_with_no_args() {
        let cli = Cli::parse_from(["codemuxd", "--socket", "/tmp/x", "--foreground"]);
        assert_eq!(cli.child_command(), ("claude".to_string(), vec![]));
    }

    #[test]
    fn explicit_command_overrides_default() {
        let cli = Cli::parse_from([
            "codemuxd",
            "--socket",
            "/tmp/x",
            "--foreground",
            "--",
            "bash",
            "-l",
            "-c",
            "exit",
        ]);
        assert_eq!(
            cli.child_command(),
            (
                "bash".to_string(),
                vec!["-l".to_string(), "-c".to_string(), "exit".to_string()],
            ),
        );
    }

    /// Without `--foreground`, all three new flags must be present;
    /// omitting `--agent-id` is a parse error from clap's
    /// `required_unless_present`.
    #[test]
    fn non_foreground_requires_agent_id() {
        let result = Cli::try_parse_from([
            "codemuxd",
            "--socket",
            "/tmp/x",
            "--pid-file",
            "/tmp/x.pid",
            "--log-file",
            "/tmp/x.log",
        ]);
        let Err(err) = result else {
            unreachable!("missing --agent-id without --foreground must error");
        };
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    /// Without `--foreground`, omitting `--pid-file` is a parse error.
    #[test]
    fn non_foreground_requires_pid_file() {
        let result = Cli::try_parse_from([
            "codemuxd",
            "--socket",
            "/tmp/x",
            "--agent-id",
            "test",
            "--log-file",
            "/tmp/x.log",
        ]);
        let Err(err) = result else {
            unreachable!("missing --pid-file without --foreground must error");
        };
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    /// Without `--foreground`, omitting `--log-file` is a parse error.
    #[test]
    fn non_foreground_requires_log_file() {
        let result = Cli::try_parse_from([
            "codemuxd",
            "--socket",
            "/tmp/x",
            "--agent-id",
            "test",
            "--pid-file",
            "/tmp/x.pid",
        ]);
        let Err(err) = result else {
            unreachable!("missing --log-file without --foreground must error");
        };
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    /// `--foreground` waives the requirement on the three filesystem
    /// flags, restoring the Stage 0 surface for ergonomic `cargo run`.
    #[test]
    fn foreground_waives_filesystem_requirements() {
        let cli = Cli::parse_from(["codemuxd", "--socket", "/tmp/x", "--foreground"]);
        assert!(cli.foreground);
        assert!(cli.agent_id.is_none());
        assert!(cli.pid_file.is_none());
        assert!(cli.log_file.is_none());
    }

    /// All three filesystem flags supplied alongside `--socket` parses
    /// cleanly without `--foreground` — the canonical "real daemon" form
    /// Stage 4's bootstrap will use.
    #[test]
    fn full_daemon_flag_set_parses() {
        let cli = Cli::parse_from([
            "codemuxd",
            "--socket",
            "/tmp/codemuxd/sockets/a.sock",
            "--agent-id",
            "alpha",
            "--pid-file",
            "/tmp/codemuxd/pids/a.pid",
            "--log-file",
            "/tmp/codemuxd/logs/a.log",
        ]);
        assert!(!cli.foreground);
        assert_eq!(cli.agent_id.as_deref(), Some("alpha"));
        assert_eq!(
            cli.pid_file.as_deref(),
            Some(std::path::Path::new("/tmp/codemuxd/pids/a.pid")),
        );
        assert_eq!(
            cli.log_file.as_deref(),
            Some(std::path::Path::new("/tmp/codemuxd/logs/a.log")),
        );
    }
}
