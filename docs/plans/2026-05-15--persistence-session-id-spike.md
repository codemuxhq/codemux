# Persistence: how codemux obtains a stable Claude session ID

## Question

How should codemux associate a stable session ID with each spawned `claude`
process, so that focusing a Dead agent later can re-spawn it via
`claude --resume <id>` without violating AD-1?

## Method

- Ran `claude --help` on the locally-installed `claude` binary
  (Claude Code `2.1.142`) and read the full options list.
- Grepped the relevant flags out of the help text:
  `--session-id`, `--resume`, `--continue`, `--fork-session`,
  `--no-session-persistence`.
- Searched the codemux tree for prior references:
  `rg -n -- '--session-id|--resume|session_id' apps/ crates/`.
- Inspected `crates/session/src/domain.rs` to confirm the existing
  `Agent::session_id: Option<String>` field shape.
- Inspected `apps/tui/src/runtime.rs::build_claude_args` to see the
  current argv constructed for `claude` (today it only injects
  `--settings <json>` for the statusLine hook).
- Sanity-checked the on-disk layout under `~/.claude/projects/` to
  confirm that Claude already stores per-session state keyed by UUID.

## Findings

### `claude --help` (verbatim excerpts)

Claude Code version installed:

```
2.1.142 (Claude Code)
```

Relevant flag excerpts from `claude --help`:

```
  -c, --continue                                    Continue the most recent conversation in the current directory
  --fork-session                                    When resuming, create a new session ID instead of reusing the original (use with --resume or --continue)
  --no-session-persistence                          Disable session persistence - sessions will not be saved to disk and cannot be resumed (only works with --print)
  -r, --resume [value]                              Resume a conversation by session ID, or open interactive picker with optional search term
  --session-id <uuid>                               Use a specific session ID for the conversation (must be a valid UUID)
```

Two things matter here:

1. `--session-id <uuid>` is a first-class CLI flag whose contract is
   "use *this* UUID for the conversation". Claude Code accepts a
   caller-supplied UUID and uses it as the session identifier. It is
   not a read-only field that Claude emits — it is an input we control.
2. `--resume <value>` accepts a session ID as its positional value.
   So the round-trip is: codemux generates a UUID, passes it to
   `claude --session-id <uuid>` at spawn, persists it, and later
   passes the same UUID to `claude --resume <uuid>` to revive the
   conversation.

The `--fork-session` flag confirms the model further: session IDs are
stable handles, and Claude explicitly exposes a way to ask for a
*new* ID when resuming. That only makes sense if the caller (us)
owns the ID end-to-end.

`--no-session-persistence` is `--print`-only — irrelevant for our
interactive PTY case, and noted just to rule it out.

### On-disk shape (sanity check only)

`~/.claude/projects/<encoded-cwd>/` contains files named like
`8e3c7632-f5ad-4e8c-bcbf-960c4a7d7c7d.jsonl` — i.e. UUID-keyed.
This is informational only; AD-1 forbids parsing these files, and
we do not need to: the UUID we pass via `--session-id` is the same
UUID Claude uses for that filename. We never have to read the file
back to learn the ID we already authored.

### Existing codemux references

```
crates/session/src/domain.rs:29:    pub session_id: Option<String>,
```

The field exists and is wired through the domain but unpopulated.
Every other hit in the grep is unrelated (POSIX session IDs in
daemon tests).

### Current spawn argv (where the wiring will land)

`apps/tui/src/runtime.rs::build_claude_args` returns
`vec!["--settings".to_string(), json]`. The persistence change
extends this argv with `--session-id <uuid>` — the UUID is generated
by codemux at spawn time and stored on the `Agent`.

## Recommendation

**Option (a): codemux generates a v4 UUID at spawn time and passes
`claude --session-id <uuid>` in the argv. Persist the UUID on the
`Agent`. Resume later with `claude --resume <uuid>`.**

Rationale, anchored on AD-1:

- We never read Claude's output, log files, or JSONL transcripts to
  learn the ID. The ID is something *codemux authors* and *Claude
  consumes*. That's the cleanest possible direction of information
  flow under AD-1: codemux configures Claude's environment, codemux
  renders Claude's PTY, codemux does not interpret what Claude says.
- It is robust to Claude Code updates. The flag is documented,
  stable across the recent versions of Claude Code (it pre-dates
  this version), and Anthropic has a strong incentive to keep it —
  it's the primary integration seam for IDE plugins and wrappers.
  If they ever rename or remove it we will get a hard, immediate
  spawn failure rather than silent drift.
- It composes trivially with `--resume`: same flag-shape, same UUID.
- It composes trivially with `--fork-session` if we later want a
  "branch this session" UX feature.
- The existing `Agent::session_id: Option<String>` field shape is
  already correct for this design.

Implementation shape: extend `build_claude_args` to accept a
`session_id: &str` (or `Uuid`) and append `--session-id <id>` after
the `--settings` pair. Generation site is `spawn_local_agent` (and
its remote counterpart for daemon-spawned agents) — a fresh `Uuid::new_v4()`
on the codemux side, propagated into the `RuntimeAgent` and the
domain `Agent`.

### Architectural boundary: where this CLI knowledge lives

The strings `--session-id` and `--resume` are knowledge about an
external tool (Claude Code's CLI surface). They must remain
encapsulated in the **spawn-argv builders** —
`apps/tui/src/runtime.rs::build_claude_args` for local spawns and
the equivalent argv construction inside `apps/daemon` for remote
spawns. Those builders are the driven adapters that translate a
domain intent ("resume this agent's session") into a concrete CLI
invocation.

The domain crate (`crates/session`) stays flag-agnostic. Today that
is already the case: `Agent::session_id: Option<String>` carries
the UUID as opaque data, with no knowledge that it is later emitted
as `--session-id <id>`. The persistence and resume tasks must
preserve this property — no `"--session-id"` or `"--resume"`
literal anywhere outside `apps/tui/` and `apps/daemon/`. If the
flag name ever changes upstream, the blast radius is confined to
those two argv builders.

## Rejected alternatives

- **(b) Env var / file-drop discovery from Claude.** No such
  mechanism is exposed by `claude --help`, and we don't need one:
  we control the ID, so there's nothing to discover.
- **(c) Parse stdout for a "session ID is …" line.** Violates
  AD-1's spirit (we'd be reading semantic meaning out of the PTY
  stream) and is also brittle — any wording change breaks it.
  No reason to do this when `--session-id` exists.
- **(d) Tail `~/.claude/projects/*.jsonl`.** Flatly violates AD-1
  ("Don't be tempted to tail `~/.claude/projects/*.jsonl`.") and
  is unnecessary — we already know the UUID because we generated it.

## Open questions / risks

- **UUID collisions across hosts.** v4 collision probability is
  negligible per host, but if codemux were ever to push a session
  to a remote daemon by re-using its locally-generated UUID, two
  hosts could in principle author the same ID for unrelated
  sessions. In practice the UUID is scoped per `(host, cwd)` on
  Claude's side, so collision risk is theoretical only. Worth a
  one-line comment at the generation site; not worth defensive
  code.
- **`--session-id` rejects non-UUIDs.** The help says "must be a
  valid UUID". Use `uuid::Uuid::new_v4().to_string()` and we're
  fine. If we ever stringify it ourselves, make sure we emit the
  canonical hyphenated form.
- **Old/dead session IDs.** A persisted ID may refer to a session
  Claude has since pruned (user ran `claude /clear`, deleted the
  project dir, etc.). `claude --resume <unknown-uuid>` will exit
  with an error. The persistence layer needs to handle "resume
  failed; spawn fresh" gracefully — surface the error in the pane
  and offer a re-spawn affordance. This is a UX concern for the
  resume-on-focus task (#5), not for this spike.
- **Phantom-session risk on spawn failure.** We generate the UUID
  *before* `claude` is invoked, so if the spawn fails (binary
  missing, bad argv, immediate crash), the UUID exists on the
  codemux side with no corresponding session on Claude's side.
  Two viable strategies:
  - (i) Persist the `Agent` row only after the PTY reports a
    successful spawn (status transitions `Starting → Running`).
    Failed spawns leave no DB residue. Simpler.
  - (ii) Persist eagerly and reconcile on resume — accept that
    some persisted UUIDs may be orphans and let the
    resume-on-focus fallback handle the "unknown UUID" case the
    same way it handles a Claude-side pruned session.

  Strategy (i) is the cleaner default; (ii) is only interesting if
  we ever want to expose `Starting` rows in the tab list across a
  crash. Decide in the schema task (#2).
- **Remote agents (daemon path).** The same flag works on the
  daemon side — `codemuxd` already constructs the `claude` argv
  for remote spawns. The implementation must thread the
  caller-generated UUID through the wire protocol so the codemux
  side and the daemon side agree on the ID. That's an additive
  field on the spawn wire message; flagging here so the persistence
  task plans for it.
- **`--bare` mode interaction.** `--bare` skips a lot of machinery
  but the help does not list `--session-id` as among the things it
  disables. We don't use `--bare`, but if a user ever configures
  codemux to launch `claude --bare`, validate that `--session-id`
  still works there. Low priority.
