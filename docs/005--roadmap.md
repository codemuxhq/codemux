# Roadmap

Personal tool. No external deadline. Lanes are independent — pick what hurts most. Items inside each lane are listed in priority order; `next:` is the one to pick up first.

## At a glance

| Lane | Up next | Note |
|---|---|---|
| [Foundations](#foundations) | Persistence (AD-7) | Agents need to survive app restart |
| [Review workflow](#review-workflow) | Diff panel | Needs vision amendment (P5, AD-6) |
| [Navigation](#navigation) | Vim keys everywhere | — |
| [Sessions](#sessions) | Save & archive | — |
| [Terminal panes](#terminal-panes) | Plain local terminal tab | — |
| [Integrations](#integrations) | Agent-agnostic spawn | Aligned with vision non-goal #7 |
| [Scale](#scale) | Groups | Data model already in `shared-kernel` |

## Foundations

- Persistence (AD-7) — agents survive app restart
- E2E test harness over the AC index

`next:` persistence

## Review workflow

Needs vision amendment: principle 5 ("edits are a peek, not a workflow") and AD-6 ("read-only diff panel") both flip with send-back annotations.

- Diff panel + open in `$EDITOR`
- OS notifications on attention-needed
- Needs-input detection
- Send-back annotations from the diff panel
- AI-explain grouped edits, didactically

`next:` diff panel

## Navigation

- Vim keys everywhere (panes, modal, scrollback, file tree)
- File-tree pane (browse + read)
- Command palette
- Nav display modes (icon rail / full / hidden)
- AI-renamed tabs (opt-in)

`next:` vim keys

## Sessions

- Save & archive completed sessions
- Reattach an archived session as read-only

`next:` save & archive

## Terminal panes

- Plain terminal tab, local
- Plain terminal tab over SSH

`next:` local terminal tab

## Integrations

- Agent-agnostic spawn (drop Claude assumptions; aligned with vision non-goal #7)
- Smarter knowledge integration (shape TBD)
- Tmux / Zellij (shape TBD)

`next:` agent-agnostic spawn

## Scale

- Groups / tags (data model already in `shared-kernel`)
- Host overview screen

`next:` groups

## Maybe

Only if the need shows up.

- Phone read-only view (control socket + thin web frontend)
- mosh as opt-in SSH transport
- Multi-window / pane detachment

## Won't do

- Editor / IDE features (LSP, syntax search, code navigation)
- Multi-user / team / sharing / presence
- Workflow enforcement (mandatory worktrees, mandatory review-before-merge)
- MCP registry, prompt manager, "studio"
- Cloud sync of agent state
- Auth beyond the Maybe-lane phone view
- Telemetry

If any of these tempts, re-read `docs/001--vision.md`.
