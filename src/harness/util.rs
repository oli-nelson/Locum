//! Helpers shared by adapters.

use super::Event;

/// Tracks streamed deltas and reconciles them with a harness's authoritative final text.
#[derive(Default)]
pub struct Reconciler {
    streamed: String,
    finalized: bool,
}

impl Reconciler {
    pub fn delta(&mut self, d: &str) {
        self.streamed.push_str(d);
    }

    pub fn streamed(&self) -> &str {
        &self.streamed
    }

    pub fn has_text(&self) -> bool {
        !self.streamed.is_empty()
    }

    /// Returns the events needed so that `concat(deltas) == final_text`, when sensible.
    pub fn finalize(&mut self, final_text: &str, harness: &str) -> Vec<Event> {
        if self.finalized {
            return vec![];
        }
        self.finalized = true;
        if self.streamed.is_empty() {
            if final_text.is_empty() {
                return vec![];
            }
            self.streamed.push_str(final_text);
            return vec![Event::TextDelta(final_text.to_string())];
        }
        if final_text == self.streamed {
            return vec![];
        }
        if let Some(rest) = final_text.strip_prefix(self.streamed.as_str()) {
            self.streamed.push_str(rest);
            return vec![Event::TextDelta(rest.to_string())];
        }
        tracing::debug!(
            harness,
            streamed = self.streamed.len(),
            final_len = final_text.len(),
            "final text diverges from streamed deltas; trusting deltas"
        );
        vec![]
    }
}

pub fn classify_message(msg: &str) -> super::ErrorKind {
    let m = msg.to_ascii_lowercase();
    if m.contains("context window")
        || m.contains("prompt is too long")
        || m.contains("too long")
        || m.contains("context_length")
        || m.contains("contextwindowexceeded")
    {
        super::ErrorKind::ContextLength
    } else if m.contains("authenticate")
        || m.contains("not logged in")
        || m.contains("login")
        || m.contains("unauthorized")
        || m.contains("oauth")
        || m.contains("api key")
    {
        super::ErrorKind::Unavailable
    } else if m.contains("rate limit")
        || m.contains("usage limit")
        || m.contains("429")
        || m.contains("overloaded")
    {
        super::ErrorKind::RateLimited
    } else {
        super::ErrorKind::Failed
    }
}
