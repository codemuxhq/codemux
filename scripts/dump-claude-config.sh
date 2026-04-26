#!/usr/bin/env bash
# Dump everything that could affect what title the `claude` binary emits:
# version, settings.json, and any claude/anthropic env vars. Used to diff
# two machines side-by-side when one is showing dynamic conversation
# titles in its terminal tab and the other is not.
#
# Pair with `scripts/capture-claude-titles.py` — that script answers
# "what *does* claude emit on this machine?", this one answers "what
# config could be making it behave that way?". Run both on each machine,
# paste the output back, and the gate (env var, setting, version) shows
# up in the diff.
#
# Usage:
#   ./scripts/dump-claude-config.sh                    # human-readable
#   ./scripts/dump-claude-config.sh > out.txt          # capture for paste
#
# The settings dump may include API key helper paths, custom OTEL
# endpoints, plugin lists, etc. Nothing should be a literal secret
# (settings.json is rendered into $HOME from a chezmoi template, not a
# secret store), but skim before pasting into a public channel.

set -u

section() {
    printf '\n=== %s ===\n' "$1"
}

section "claude --version"
if command -v claude >/dev/null 2>&1; then
    claude --version 2>&1
    printf '\n(resolved via: %s)\n' "$(type claude 2>&1 | head -1)"
else
    echo "(claude not on PATH)"
fi

section "~/.claude/settings.json"
if [ -f "$HOME/.claude/settings.json" ]; then
    cat "$HOME/.claude/settings.json"
else
    echo "(not present)"
fi

section "claude / anthropic env vars"
env | grep -iE 'claude|anthropic' | sort || echo "(none set)"

section "TERM / TERM_PROGRAM"
# Surfaces ghostty version + terminfo, useful when the same binary
# behaves differently across terminal emulators.
env | grep -E '^(TERM|TERM_PROGRAM|TERM_PROGRAM_VERSION|COLORTERM|TERMINFO)=' | sort

section "shell alias for claude (if any)"
# `type` resolves aliases in the current shell. Safer than `which`,
# which silently skips them.
type claude 2>&1
