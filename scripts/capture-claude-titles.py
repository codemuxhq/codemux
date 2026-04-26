#!/usr/bin/env python3
"""Capture every OSC 0/1/2 window-title sequence the `claude` binary emits.

Why this exists
---------------
Codemux labels each navigator tab using the title the foreground process
writes via OSC 0/2 (`ESC ] 0;...BEL` / `ESC ] 2;...BEL`). On one machine
the user reported seeing a conversation summary in the title; on another
machine the same `claude` binary appeared to only emit the static
`Claude Code` brand label. This script is the diagnostic that decides
which of the two is happening: it spawns `claude` inside a PTY, drives it
through one short turn, and prints every distinct OSC title it observed.

If the output contains anything other than `'✳ Claude Code'` plus
Braille spinner frames (`⠂ Claude Code`, `⠐ Claude Code`, …) then claude
*is* emitting a richer title and codemux's renderer is allowed to
surface it. If the output is *only* those frames, the static label is
the truth and any "summary" the user is seeing is coming from somewhere
else (ghostty's own surface label, a shell hook, or an aifx wrapper).

Usage
-----
    python3 scripts/capture-claude-titles.py                # use `claude` from PATH
    CLAUDE_BIN=/path/to/claude python3 scripts/capture-claude-titles.py

The script blocks for ~60 seconds, sends one short prompt at the 8s mark,
then prints a deduplicated list of titles in the order they were first
seen. Designed to be run on each machine you want to compare and the
output pasted back side-by-side.

The execvp call deliberately bypasses any shell aliases (e.g. the
`claude → aifx agent run claude` alias on Uber laptops) so we exercise
the same code path codemux itself takes when it spawns a child via
portable-pty.
"""

import fcntl
import os
import pty
import re
import select
import struct
import sys
import termios
import time

CAPTURE_SECONDS = 60
PROMPT_AT_SECONDS = 8
PROMPT = b"help me write a python function that reverses a string\r"

# OSC 0 / 1 / 2 — `ESC ] {0,1,2} ; <title> {BEL | ST}`. We tolerate both
# terminators because tools split on which one they emit.
OSC_TITLE = re.compile(rb"\x1b\][012];([^\x07\x1b]+?)(?:\x07|\x1b\\)")


def main() -> int:
    binary = os.environ.get("CLAUDE_BIN", "claude")
    pid, fd = pty.fork()
    if pid == 0:
        # Child: replace ourselves with claude. execvp searches PATH unless
        # CLAUDE_BIN was an absolute path, in which case execvp uses it
        # directly (matching codemux's portable-pty behaviour).
        os.execvp(binary, [binary])

    # Give the child a sensible window size so claude doesn't render a
    # 24x80 fallback that suppresses its richer status line.
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 160, 0, 0))

    buf = b""
    deadline = time.time() + CAPTURE_SECONDS
    sent_prompt = False
    titles: list[str] = []
    seen: set[str] = set()

    while time.time() < deadline:
        rlist, _, _ = select.select([fd], [], [], 0.1)
        if rlist:
            try:
                chunk = os.read(fd, 8192)
            except OSError:
                break
            if not chunk:
                break
            buf += chunk
            for match in OSC_TITLE.finditer(chunk):
                try:
                    title = match.group(1).decode("utf-8", errors="replace")
                except Exception:  # noqa: BLE001 - capture, don't fail
                    continue
                if title and title not in seen:
                    seen.add(title)
                    titles.append(title)
        if not sent_prompt and time.time() > deadline - CAPTURE_SECONDS + PROMPT_AT_SECONDS:
            sent_prompt = True
            try:
                os.write(fd, PROMPT)
            except OSError:
                pass

    try:
        os.kill(pid, 9)
    except ProcessLookupError:
        pass

    print(f"=== {len(titles)} distinct OSC titles ===")
    for t in titles:
        leading = " ".join(f"U+{ord(c):04X}" for c in t[:2])
        print(f"  {t!r:80} [{leading}]")
    return 0


if __name__ == "__main__":
    sys.exit(main())
