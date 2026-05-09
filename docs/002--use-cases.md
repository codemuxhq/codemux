# Use cases

## Shape of the work

A typical working day: 3-4 Claude Code agents alive in parallel. Some local on my laptop. Some on a server or a devpod, often more than one devpod on the same host (e.g., two concurrent workspaces on `devpod-work`). Each agent is its own independent session, doing its own task. I move between them.

The pain is in the parallelization. Agents finish at unpredictable times. I want to know quickly, review what they did, push back if something's off, and move on. Current tools make me alt-tab between terminals to check status and hunt for the right session.

codemux exists to remove that friction. The four scenarios below are the concrete workflows it's designed for.

## Scenario 1: Parallel across hosts

I have four agents going:

- `work-feed` on `devpod-work-1`, a bugfix
- `work-carousel` on `devpod-work-2`, a feature
- `agents-window` on my laptop, a personal side project
- `knowledge-curate` on my laptop, an ops task

I scan the navigator. For each agent I see: status (running / idle / needs-input / done), pwd (which repo), host. I pick one and focus it in a single keystroke. No mental bookkeeping of "which tmux window was `work-feed` again."

## Scenario 2: "Is it done yet?"

`work-feed` was running `go test` under Claude's orchestration. I moved to `work-carousel` to give it instructions. Three minutes later `work-feed` finishes.

Without codemux: I periodically alt-tab to my `devpod-work-1` terminal to check. Wasteful, breaks flow.

With codemux: the navigator's status dot for `work-feed` flips from running to idle (or to needs-input if Claude is asking a permission question). Eventually (P2), an OS notification fires. I see it, jump to that agent with one keystroke, read what happened, respond.

## Scenario 3: Peek-to-review spectrum

Sometimes I want a quick glance at what the agent changed. Sometimes I want to read the diff carefully before letting it continue.

- **Peek**: the edits panel shows `git diff` for the focused agent, syntax-highlighted. Three files changed, +14/-3. Good enough. I type back: "looks right, keep going."
- **Deep review**: same agent, but the change is load-bearing. I hit the "open in editor" shortcut. My `$EDITOR` opens with the diff. I read carefully, close the editor, and type corrections back to the agent.

codemux gives me a one-keystroke bridge between the two depths.

## Scenario 4: Cross-host launch

I want to spin up a new agent for a bugfix on a specific host, in a specific repo.

Without codemux: SSH to the host, `cd` to the repo, start `claude`, type the prompt. Context switch heavy.

With codemux: I trigger "new agent" from the TUI. Pick the host (recent hosts first). Pick the repo (recent paths under that host first). Optionally, an initial prompt. Enter. The agent spawns on the chosen host, appears in the navigator, and I focus into it.

## Anti-scenarios: things codemux explicitly won't help with

These are real needs, but codemux is the wrong place for them:

- **Team visibility**: sharing sessions, showing a teammate what an agent is doing. codemux is single-user by design.
- **Editor or IDE functions**: LSP, syntax search, git operations beyond `git diff`. Use the editor.
- **Workflow enforcement**: required worktrees, mandatory review before merge, approval chains. codemux expresses no opinion on how I work.

If I want any of the above, I use a different tool. codemux stays focused.
