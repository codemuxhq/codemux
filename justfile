# List available recipes.
default:
    @just --list

# Run the TUI. Args pass through, e.g. `just run -- --nav left-pane`.
run *ARGS:
    cargo run -p codemux-tui --bin codemux -- {{ARGS}}

# Run the daemon (foreground mode for `cargo run`). Args pass through, e.g.
# `just daemon -- --socket /tmp/dev.sock -- bash` to exec bash instead of
# the default `claude`.
daemon *ARGS:
    cargo run -p codemux-daemon -- --foreground {{ARGS}}

# Format the workspace.
fmt:
    cargo fmt --all

# Lint with the project's strict clippy settings.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Run tests.
test:
    cargo test --workspace

# Pre-push gate: format check, lint, test.
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
