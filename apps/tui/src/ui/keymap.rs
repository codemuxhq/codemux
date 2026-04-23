//! Prefix-key state machine (AD-12).
//!
//! Tmux-style. A prefix key (default Ctrl-B) indicates that the next keystroke
//! is a codemux command. Without the prefix, keystrokes are forwarded verbatim
//! to the focused agent's PTY.

#[allow(dead_code)] // Wired into the event loop in P1.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PrefixState {
    #[default]
    Idle,
    AwaitingCommand,
}

// TODO(P1): command dispatch table — e.g., `n` next agent, `p` prev agent,
// `c` spawn new agent, `k` kill focused agent.
