//! In-memory tail of the most recent tracing log line.
//!
//! Plumbed into the tracing subscriber as a custom [`Layer`] so it
//! captures the same formatted lines that the file writer sees.
//! [`runtime`](crate::runtime) reads the latest line on each frame
//! poll to render the bottom log strip when the TUI is launched with
//! `--log`. The layer is attached unconditionally (regardless of
//! `--log`); the runtime simply doesn't render the strip when the
//! handle is `None`. Cost is one mutex lock per log event, which is
//! invisible at our log volume.
//!
//! Single-line buffer rather than a ring of N lines because the
//! status surface only renders one row and the user's chosen UX
//! (per the Stage 5 follow-up) is "show the most recent line." A
//! ring buffer would be straightforward to add if a future toggle
//! wants to expand into a scrollable overlay.

use std::fmt::Write;
use std::sync::{Arc, Mutex};

use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// Shared handle to the latest formatted log line. `Clone` is cheap
/// (Arc bump). Both the tracing layer (writer) and the runtime
/// (reader) hold a clone.
#[derive(Clone)]
pub struct LogTail {
    inner: Arc<Mutex<Option<String>>>,
}

impl LogTail {
    /// Create an empty tail. The first log event populates it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
        }
    }

    /// Latest formatted log line (or `None` if no event has fired
    /// yet). Returns an owned string so the caller doesn't hold the
    /// lock across a render.
    #[must_use]
    pub fn latest(&self) -> Option<String> {
        // Lock only fails if poisoned (a panicking writer); in that
        // case we'd rather show nothing than propagate a panic into
        // the render loop. Same rationale as elsewhere in the TUI.
        self.inner.lock().ok().and_then(|g| g.clone())
    }

    /// Attach this tail as a `tracing_subscriber` layer. The same
    /// tail can be subscribed once and read by the runtime; the
    /// `Clone` impl makes that ergonomic.
    #[must_use]
    pub fn layer(&self) -> TailLayer {
        TailLayer { tail: self.clone() }
    }

    fn store(&self, line: String) {
        if let Ok(mut g) = self.inner.lock() {
            *g = Some(line);
        }
    }
}

impl Default for LogTail {
    fn default() -> Self {
        Self::new()
    }
}

/// Custom `tracing_subscriber::Layer` that formats each event into a
/// single line and pushes it into the shared [`LogTail`].
///
/// Intentionally minimal — no ANSI, no targets, no spans, just
/// `HH:MM:SS LEVEL message`. Matches the visual budget of a single
/// terminal row (the user's chosen "bottom strip" UX).
pub struct TailLayer {
    tail: LogTail,
}

impl<S> Layer<S> for TailLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let day_secs = secs % 86_400;
        let h = day_secs / 3_600;
        let m = (day_secs % 3_600) / 60;
        let s = day_secs % 60;
        let level = *event.metadata().level();
        let mut line = String::with_capacity(64 + visitor.message.len() + visitor.fields.len());
        // UTC, not local — the file writer also uses UTC, so keeping
        // the strip consistent matters more than matching the user's
        // wall clock.
        let _ = write!(line, "{h:02}:{m:02}:{s:02} {level:<5} {}", visitor.message);
        if !visitor.fields.is_empty() {
            line.push_str(&visitor.fields);
        }
        self.tail.store(line);
    }
}

/// Drains an event's recorded fields into a single-line string. The
/// `message` field is captured separately so the tail layer can
/// place it before the structured fields.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            // Strip the surrounding quotes Debug puts on strings;
            // the message is almost always a string and the quotes
            // are noise.
            let raw = format!("{value:?}");
            self.message = raw.trim_matches('"').to_string();
        } else {
            let _ = write!(&mut self.fields, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            let _ = write!(&mut self.fields, " {}={value}", field.name());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use tracing::info;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    use super::*;

    /// A `tracing::info!` event lands in the tail's latest slot
    /// formatted as `HH:MM:SS LEVEL message`. We don't pin the time
    /// portion — only that the level + message text are present.
    #[test]
    fn captures_latest_event_with_level_and_message() {
        let tail = LogTail::new();
        let _guard = Registry::default().with(tail.layer()).set_default();
        info!("hello world");
        let line = tail
            .latest()
            .unwrap_or_else(|| panic!("tail should have a line"));
        assert!(line.contains("INFO"), "level missing in {line:?}");
        assert!(line.contains("hello world"), "message missing in {line:?}",);
    }

    /// Multiple events overwrite — only the most recent one stays.
    /// (The user's chosen UX is "latest line"; a ring buffer would
    /// keep history but isn't what they asked for.)
    #[test]
    fn keeps_only_the_most_recent_event() {
        let tail = LogTail::new();
        let _guard = Registry::default().with(tail.layer()).set_default();
        info!("first");
        info!("second");
        let line = tail.latest().unwrap();
        assert!(line.contains("second"), "expected second, got {line:?}");
        assert!(!line.contains("first"), "first should be overwritten");
    }

    /// Structured fields appear after the message, space-separated.
    /// The runtime renders the line as-is, so this format is the
    /// final visual.
    #[test]
    fn formats_structured_fields_after_message() {
        let tail = LogTail::new();
        let _guard = Registry::default().with(tail.layer()).set_default();
        info!(host = "devpod-go", agent_id = "agent-3", "started worker");
        let line = tail.latest().unwrap();
        assert!(line.contains("started worker"));
        assert!(
            line.contains("host=devpod-go"),
            "host field missing in {line:?}",
        );
        assert!(
            line.contains("agent_id=agent-3"),
            "agent_id field missing in {line:?}",
        );
    }

    /// `latest` returns `None` before any event fires.
    #[test]
    fn returns_none_before_any_event() {
        let tail = LogTail::new();
        let _guard = Registry::default().with(tail.layer()).set_default();
        assert!(tail.latest().is_none());
    }
}
