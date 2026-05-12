# List available recipes.
default:
    @just --list

# Run the TUI. Args pass through, e.g. `just run -- --nav left-pane`.
run *ARGS:
    cargo run -p codemux-cli --bin codemux -- {{ARGS}}

# Run the daemon (foreground mode for `cargo run`). Args pass through, e.g.
# `just daemon -- --socket /tmp/dev.sock -- bash` to exec bash instead of
# the default `claude`.
daemon *ARGS:
    cargo run -p codemuxd -- --foreground {{ARGS}}

# Format the workspace.
fmt:
    cargo fmt --all

# Lint with the project's strict clippy settings.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Run tests.
test:
    cargo test --workspace

# Run only the slow-tier E2E suite — boots a real `codemux` binary
# inside an 80x24 PTY against the in-tree `fake_agent` stub (T3) AND a
# real `codemuxd` subprocess against the daemon-side `fake_daemon_agent`
# stub (T4). Gated behind `--ignored` so day-to-day `just test` stays
# fast. See `docs/plans/2026-05-10--e2e-testing.md`.
test-e2e:
    cargo test --workspace --features codemux-cli/test-fakes,codemuxd/test-fakes -- --ignored

# Run the fast and slow tiers together. Useful for "is everything green
# end to end" before a non-trivial change.
test-all:
    cargo test --workspace --features codemux-cli/test-fakes,codemuxd/test-fakes -- --include-ignored

# Review pending insta snapshots. Requires `cargo install cargo-insta` once.
insta-review:
    cargo insta review

# Pre-push gate: format check, lint, test.
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

# Slow-tier pre-push gate. Same shape as `check`, but with the
# `test-fakes` feature on so clippy/tests see the TUI PTY harness AND
# the daemon E2E harness. Run before merging anything that touches the
# spawn path, the daemon protocol surface, or either harness.
check-e2e:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --features codemux-cli/test-fakes,codemuxd/test-fakes -- -D warnings
    cargo test --workspace --features codemux-cli/test-fakes,codemuxd/test-fakes -- --ignored

# Build a release binary for the host target only. cargo-dist owns the
# cross-compile matrix in CI; locally we only need the host artifact for
# smoke tests before tagging.
release-build:
    cargo build --workspace --release

# Print the artifact matrix the next `dist` release would produce.
# Read-only; safe to run on any branch. Requires `dist` installed locally.
release-plan:
    dist plan

# Tag a new release. Bumps two version surfaces in lock step:
#   1. `[workspace.package].version` — the value every crate inherits via
#      `version.workspace = true`.
#   2. Every internal-dep version pin in `[workspace.dependencies]` (the
#      `version = "X.Y.Z"` alongside `path = "crates/..."`). Cargo strips
#      the path on `cargo publish` and uses the version constraint, so
#      these have to track the workspace package version exactly. See
#      AD-31.
# Then commits, tags v$VERSION, and prints the next manual step. Push is
# deliberate — we do not auto-push tags. Usage: `just release-tag 0.2.0`.
# Aborts if the working tree is dirty.
release-tag VERSION:
    @test -z "$(git status --porcelain)" || (echo "working tree dirty; commit or stash first" && exit 1)
    @grep -q '^version = "[0-9]' Cargo.toml || (echo "workspace.package.version not found in Cargo.toml" && exit 1)
    # 1) Bump workspace.package.version (the one bare `version = "..."` line).
    sed -i.bak -E 's/^(version = )"[0-9][^"]*"/\1"{{VERSION}}"/' Cargo.toml && rm Cargo.toml.bak
    # 2) Bump every internal-dep version pin. The pattern matches lines of the
    # shape: `something = { path = "crates/...", version = "X.Y.Z" }` and
    # rewrites the version. Anchored on `path = "crates/` so we never touch
    # third-party deps. Also matches `path = "crates/codemuxd-bootstrap"` etc.
    sed -i.bak -E 's|(path = "crates/[^"]*", version = )"[0-9][^"]*"|\1"{{VERSION}}"|g' Cargo.toml && rm Cargo.toml.bak
    cargo update --workspace
    cargo check --workspace
    git add Cargo.toml Cargo.lock
    git commit -m "chore: release v{{VERSION}}"
    git tag "v{{VERSION}}"
    @echo ""
    @echo "Tagged v{{VERSION}}. To publish:"
    @echo "    git push origin main 'v{{VERSION}}'"
    @echo "cargo-dist will pick up the tag and build GitHub Releases + the"
    @echo "Homebrew tap. The publish-crates.yml workflow will publish to"
    @echo "crates.io. CARGO_REGISTRY_TOKEN and HOMEBREW_TAP_TOKEN must be"
    @echo "set as repo secrets."
