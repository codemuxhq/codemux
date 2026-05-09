# Testing

How we test codemux end-to-end. Short by design — update as we ship.

## Stack

Two test shapes, two stacks. They share `insta` and `pretty_assertions`; nothing else.

**TUI tests** (`apps/tui/tests/`)

| Tool | Role |
|---|---|
| `ratatui::backend::TestBackend` | In-process render-layer harness for the fast tier |
| `portable-pty` | Spawn the real `codemux` binary in PTY tests (already a prod dep) |
| `vt100` | Parse the master-side byte stream into a cell grid (already transitive via `tui-term`) |
| `insta` (+ `cargo-insta`) | Snapshot diffing of `Buffer` / `vt100::Screen` |
| `serial_test` | `#[serial]` on PTY tests so terminal-size negotiation doesn't race |
| `pretty_assertions` | Readable diffs on buffer asserts |

**Daemon tests** (`apps/daemon/tests/`)

| Tool | Role |
|---|---|
| `std::process::Command` | Spawn the daemon binary as a regular subprocess (no PTY) |
| `crates/wire` (as a client) | Drive the daemon via its own protocol — same surface real codemux uses |
| `insta` | Snapshot diffing of decoded protocol responses |
| `pretty_assertions` | Readable diffs |

We are **not** adopting a Playwright-style framework. Microsoft's `tui-test` is the only mature option in that category and it would drag a Node toolchain into a pure-Rust repo. Termwright (Rust-native) is too young (15 stars, v0.2). Skipped after evaluation: `expectrl`, `rexpect`, `assert_cmd`, `vhs`, `ratatui-testlib`. Re-evaluate Termwright in ~12 months.

## Layout

Each app owns its own E2E suite. The TUI and daemon share **no test infrastructure** — only the real `crates/wire` protocol crate, which is already part of the architecture.

```
apps/tui/
├── Cargo.toml                       # [[bin]] fake_agent gated by test-fakes feature
└── tests/
    ├── common/mod.rs                # PTY harness: spawn_codemux, send_keys, screen_eventually
    ├── bin/fake_agent.rs            # stub agent — only built with --features test-fakes
    ├── render_*.rs                  # fast tier (TestBackend), runs on every just check
    └── pty_*.rs                     # slow tier, #[ignore] by default

apps/daemon/
└── tests/
    ├── common/mod.rs                # protocol harness: spawn_codemuxd, wire client wiring
    └── proto_*.rs                   # daemon scenarios, #[ignore] by default
```

`fake_agent` lives in `apps/tui/tests/bin/fake_agent.rs` and is wired into `apps/tui/Cargo.toml` as:

```toml
[[bin]]
name = "fake_agent"
path = "tests/bin/fake_agent.rs"
required-features = ["test-fakes"]

[features]
test-fakes = []
```

`cargo build --release` does not build it. `cargo test --features test-fakes` does. Tests resolve the path via `env!("CARGO_BIN_EXE_fake_agent")`.

A `just check-e2e` recipe wraps `cargo test --features test-fakes -- --ignored` so day-to-day flow stays `just check`.

**No `crates/e2e/` workspace member.** Earlier drafts proposed one; daemon E2E doesn't share infrastructure with TUI E2E (different spawn mechanism, different observation surface, different assertions), so a unified test crate would be false coupling.

**Cross-cutting tests** (TUI → daemon → fake agent) live in `apps/tui/tests/` because the TUI is the driving end. The PTY harness gains a small `spawn_codemuxd()` helper alongside `spawn_codemux()` — ~10 lines around `std::process::Command`, no separate crate needed.

## Invariants

- TUI deps stay out of `crates/session`, `crates/shared-kernel`, `crates/wire`, `crates/codemuxd-bootstrap`. PTY/vt100 deps are confined to `apps/tui`'s `[dev-dependencies]`.
- Snapshot review goes through `cargo insta review`. No hand-edited `.snap` files.
- Frame stability has exactly one rule: poll `vt100::Screen` until the assertion holds or a timeout fires; on timeout, print the actual screen. Defined once in `apps/tui/tests/common/mod.rs::screen_eventually` and used everywhere. Never `sleep()` in tests.
- A `CODEMUX_AGENT_BIN` env var is the single indirection point for swapping the spawned agent in tests. Production codepath defaults to `claude`.
- Daemon tests speak `crates/wire` directly. They do not parse text logs, screen output, or anything other than the real protocol.

## Roadmap

### T0 — Fast-tier scaffolding (TUI)

- Add `insta`, `pretty_assertions` as dev-deps on `apps/tui`.
- First snapshot test: empty boot screen renders the expected chrome.
- Wire `cargo insta review` into the dev workflow.

**Ship test**: `just check` runs at least one snapshot test that fails meaningfully when chrome layout changes.

### T1 — Fast-tier coverage of core TUI flows

- Spawn modal: host/path zones, autocomplete, host-badge rendering.
- Navigator: agent rows, status dots, focus cycling.
- Help screen: keymap rendering stays in sync with the `Bindings` POD.

Requires `Runtime` to be constructible in tests with an injected event source. Do this refactor once at the start of T1, not piecemeal.

**Ship test**: a chrome regression in any of the three views above is caught before commit.

### T2 — PTY harness + fake_agent

- Add `apps/tui/tests/common/mod.rs` with `spawn_codemux`, `send_keys`, `screen_eventually`.
- Add `apps/tui/tests/bin/fake_agent.rs` and the `test-fakes` feature.
- `CODEMUX_AGENT_BIN` plumbing in `apps/tui` agent-spawn path.
- One end-to-end test: boot codemux, spawn fake agent, assert prompt renders.
- `just check-e2e` recipe.

**Ship test**: `just check-e2e` boots a real `codemux` binary against the stub and passes deterministically across 20 consecutive runs.

### T3 — TUI regression coverage

Add slow-tier TUI tests for the things that have actually broken:

- Non-blocking SSH writes (commit `a263ded`).
- Agent lifecycle: spawn → detach → reattach → kill.
- Persistence round-trip: spawn agents, kill codemux, restart, agents reappear.

Add tests as bugs surface, not speculatively.

### T4 — Daemon E2E

- `apps/daemon/tests/common/mod.rs` with `spawn_codemuxd` and a wire-client wrapper.
- Cover the protocol surface that already exists.
- Daemon-only tests live in `apps/daemon/tests/`. Cross-cutting tests (codemux + codemuxd together) live in `apps/tui/tests/` and call `spawn_codemuxd` from the TUI harness.

**Ship test**: an E2E test that boots `codemuxd`, connects from `codemux` over wire, spawns a fake agent through the full path, and asserts the rendered output.

## Open decisions

- Fast-tier reach: render-only (gitui-style) vs full runtime with injected key channel (helix-style). Lean helix-style — more upfront work, far more useful coverage. Lock in during T1.
- vhs as documentation-grade goldens for `docs/`: separate axis from correctness testing. Skip until there's a screenshots pipeline.
- Real sshd vs fake-ssh stub for T3 SSH coverage. Stub-first; revisit if real protocol bugs slip through.
- Re-evaluate Termwright at ~v1.0 if it gets there. If it stabilizes with a real maintainer base, the PTY harness is a candidate to replace.
