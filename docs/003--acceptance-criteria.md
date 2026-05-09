# Acceptance criteria

Testable user-task specs that map onto E2E tests. Each AC is one deterministic flow: preconditions, exact keystrokes, and the observable state that must hold. Format is Given / When / Then.

ACs cite their target test in `apps/tui/tests/pty_*.rs` for TUI-driven flows or `apps/daemon/tests/proto_*.rs` for daemon protocol flows. Cite `TBD` when the test does not exist yet. The AC is the contract; the test catches up.

Scope: ACs cover the four scenarios in [`002--scenarios.md`](002--scenarios.md). Add an AC the moment a scenario is concrete enough to verify. Skip the ones still in flux.

---

## AC-1: <imperative title — the user-visible task being verified>

**Maps to:** `apps/<crate>/tests/<file>.rs::<test_fn>` (or `TBD`)

**Given:**
- <precondition: env, which agents are running, what config is in effect>

**When:**
1. <step: exact keystroke or command, one per line>

**Then:**
- <observable outcome: what's on screen, what status flips, what state the PTY holds>
