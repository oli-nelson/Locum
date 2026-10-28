//! Deterministic in-process harness used by tests and `LOCUM_FAKE_HARNESS=1`.
//!
//! Behaviour is driven by the prompt text:
//! - `!error` → mid-stream failure, `!context` → context-length error, `!unavailable` → 503
//! - `!slow` → 50 ms between words, `!reasoning` → emits a reasoning delta first
//! - otherwise the input is echoed back word by word.

use async_stream::stream;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{
    ErrorKind, Event, EventStream, FinishReason, Harness, HarnessError, HarnessInfo, HarnessKind,
    ModelInfo, Turn, Usage,
};

#[derive(Default)]
pub struct FakeHarness;

#[async_trait]
impl Harness for FakeHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Fake
    }

    async fn probe(&self) -> Result<HarnessInfo, HarnessError> {
        Ok(HarnessInfo {
            executable: None,
            version: Some("fake".into()),
            authenticated: Some(true),
            default_model: "fake".into(),
            detail: None,
        })
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, HarnessError> {
        Ok(vec![ModelInfo {
            id: "fake".into(),
            default_effort: None,
            efforts: vec![],
        }])
    }

    async fn run(
        &self,
        turn: Turn,
        cancel: CancellationToken,
    ) -> Result<EventStream, HarnessError> {
        if turn.input.contains("!unavailable") {
            return Err(HarnessError::unavailable("fake harness unavailable"));
        }
        let input = turn.input.clone();
        let prompt_chars = turn.input.len() + turn.system.as_deref().map_or(0, str::len);
        let s = stream! {
            yield Event::ResolvedModel("fake-1".into());
            if input.contains("!reasoning") {
                yield Event::ReasoningDelta("thinking about it".into());
            }
            if input.contains("!context") {
                yield Event::Error(HarnessError::new(ErrorKind::ContextLength, "prompt too long"));
                return;
            }
            let words: Vec<&str> = input.split_whitespace().filter(|w| !w.starts_with('!')).collect();
            let mut out = String::new();
            for (i, w) in words.iter().enumerate() {
                if cancel.is_cancelled() {
                    yield Event::Done(FinishReason::Interrupted);
                    return;
                }
                if input.contains("!slow") {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                if input.contains("!error") && i == words.len() / 2 {
                    yield Event::Error(HarnessError::failed("fake mid-stream failure"));
                    return;
                }
                let piece = if i == 0 { w.to_string() } else { format!(" {w}") };
                out.push_str(&piece);
                yield Event::TextDelta(piece);
            }
            yield Event::Usage(Usage { estimated: false, ..Usage::estimate(prompt_chars, out.len()) });
            yield Event::Done(FinishReason::Stop);
        };
        Ok(Box::pin(s))
    }
}
