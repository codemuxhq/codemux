# Vision

A single TUI where every Claude Code agent I have running — local or on a remote SSH host — shows up as a navigable pane. I switch between them in a single keystroke, see what each is doing at a glance, and peek at what each one has changed without leaving the app.

The Cursor agents window UX, but for Claude Code, cross-machine, as a TUI that lives where I already live.

## Why I'm building this myself

I've tried the adjacent tools. Each gets one corner right and falls down somewhere else:

- **Claude Code Desktop** — closest in spirit, weak at the multi-host grouping I actually use.
- **Nimbalyst / Crystal** — nice diff view, local-only, opinionated about worktrees in ways I don't want.
- **ccmanager** — TUI, but no spatial layout, no edits view, thin on remote.
- **tmux / zellij** — powerful, but the chrome is generic. I want chrome optimized for *this* job.
- **IDE extensions** — bind me to one editor and one machine.

The constant: each tool tried to be smart about Claude Code. I want a dumb host. Claude Code is the brain; my job is the rectangles, the routing, and the shelf they sit on.

## Non-goals

If I find myself adding any of the below, I've drifted:

- Not a Claude Code replacement
- Not a workflow opinion (no enforced worktree-per-agent, no enforced "review before merge")
- Not an editor
- Not a multi-user / team product
- Not an IDE
- Not an MCP registry, prompt manager, or "studio"
- Not cross-tool (no Codex, Cursor, Gemini — just Claude Code)

## UX principles

The rules. When in doubt, the principle wins over the feature.

1. **Claude Code renders itself.** codemux parses VT escape sequences only to put Claude's own output into a pane. It never interprets conversation state, tool calls, or session content. If Claude Code's UI changes tomorrow, codemux keeps working.

2. **One keystroke to switch.** No menus, no clicks, no animation. Spatial memory is the point.

3. **The navigator is the map.** Whatever its shape (list, tabs, popup), it answers "what do I have running, where, in what state" at a glance.

4. **Grouping is mine.** I tag things how I think about them, not how the filesystem or git is organized. The app remembers.

5. **Edits are a peek, not a workflow.** The diff panel is for awareness. Deep review happens in `$EDITOR`. codemux is never a code-review tool.

6. **No surprise resurrection.** If a session is dead, it shows dead. If reattach fails, it tells me. Never silently start a new conversation behind a familiar label.

7. **Runs where my work runs.** As a TUI, codemux lives inside a terminal — local, on a devpod, anywhere I can SSH. No GUI install per machine.

8. **Latency is a feature.** Switching between agents must feel instant. Spawning may take a second. Reattach may take a few. Switching is not.
