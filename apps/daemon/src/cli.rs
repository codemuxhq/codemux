//! CLI surface for `codemuxd`.
//!
//! Stage 0 keeps the surface minimal: where to listen, whether to log to
//! stdout (`--foreground` for `cargo run`) or a file (Stage 2 wires the file
//! path), and what to exec inside the PTY. The exec command is taken as a
//! trailing positional argv (after `--`) so users can run `codemuxd --socket
//! ... -- bash -l` for hermetic tests; if no command is given it defaults to
//! `claude`.

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
    /// Stage 0 is foreground-only; the flag exists for forward compat.
    #[arg(long, default_value_t = false)]
    pub foreground: bool,

    /// Working directory for the spawned child. If omitted, the child
    /// inherits the daemon's cwd.
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Initial PTY size (rows). The TUI overrides this on first attach
    /// (Stage 1) — the value here is just the bootstrap size.
    #[arg(long, default_value_t = 24)]
    pub rows: u16,

    /// Initial PTY size (cols).
    #[arg(long, default_value_t = 80)]
    pub cols: u16,

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
    use super::*;

    #[test]
    fn defaults_to_claude_with_no_args() {
        let cli = Cli::parse_from(["codemuxd", "--socket", "/tmp/x"]);
        assert_eq!(cli.child_command(), ("claude".to_string(), vec![]));
    }

    #[test]
    fn explicit_command_overrides_default() {
        let cli = Cli::parse_from([
            "codemuxd", "--socket", "/tmp/x", "--", "bash", "-l", "-c", "exit",
        ]);
        assert_eq!(
            cli.child_command(),
            (
                "bash".to_string(),
                vec!["-l".to_string(), "-c".to_string(), "exit".to_string()],
            ),
        );
    }
}
