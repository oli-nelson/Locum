//! Drives one harness turn and normalises it into client-facing events: timeouts, stop
//! sequences, length truncation, micro-batching, size caps and session-cache bookkeeping.

use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_stream::stream;
use futures::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

use super::error::ApiError;
use super::AppState;
use crate::harness::{Event, FinishReason, HarnessKind, Turn, Usage};
use crate::openai::Message;
use crate::routing::Resolved;

#[derive(Debug, Clone)]
pub enum OutEvent {
    Resolved(String),
    Text(String),
    Reasoning(String),
    Usage(Usage),
    Finish(FinishReason),
    Error(ApiError),
}

pub type OutStream = Pin<Box<dyn Stream<Item = OutEvent> + Send + 'static>>;

pub struct TurnOptions {
    pub stream: bool,
    pub max_tokens: Option<u32>,
    pub stop: Vec<String>,
    pub want_reasoning: bool,
    pub json_mode: bool,
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub struct Started {
    pub stream: OutStream,
    pub harness: HarnessKind,
}

pub struct TurnInput {
    pub request_id: String,
    pub resolved: Resolved,
    pub system: Option<String>,
    pub input: String,
    pub images: Vec<crate::harness::ImageInput>,
    pub response_schema: Option<serde_json::Value>,
    pub scratch: Arc<tempfile::TempDir>,
    pub prompt_chars: usize,
    /// Original messages, used for the session cache.
    pub messages: Vec<Message>,
}

pub async fn start(
    state: Arc<AppState>,
    inp: TurnInput,
    opts: TurnOptions,
) -> Result<Started, ApiError> {
    let harness = state
        .registry
        .get(inp.resolved.harness)
        .ok_or_else(|| ApiError::model_not_found(&inp.resolved.model))?;

    let wait_started = Instant::now();
    let permit = tokio::time::timeout(
        state.cfg.limits.queue_timeout,
        state.inflight.clone().acquire_owned(),
    )
    .await
    .map_err(|_| ApiError::busy())?
    .map_err(|_| ApiError::busy())?;
    let pool_wait_ms = wait_started.elapsed().as_millis() as u64;

    let cancel = CancellationToken::new();
    let kind = inp.resolved.harness;

    // Session cache: resume a stored continuation when the prefix matches.
    let mut resume = None;
    let mut cache_input = inp.input.clone();
    let mut cache_system = inp.system.clone();
    if let Some(sc) = &state.session_cache {
        if let Some(hit) = sc.lookup(kind, &inp.resolved.model, &inp.messages) {
            resume = Some(hit.continuation);
            // Only the final user message is needed on a resumed session.
            if let Some(last) = inp.messages.iter().rev().find(|m| m.role == "user") {
                cache_input = crate::prompt::render(
                    std::slice::from_ref(last),
                    None,
                    &crate::config::PromptConfig {
                        preamble: false,
                        ..Default::default()
                    },
                    inp.scratch.path(),
                    true,
                    true,
                )
                .map(|r| r.input)
                .unwrap_or(cache_input);
                cache_system = None;
            }
        }
    }

    let turn = Turn {
        request_id: inp.request_id.clone(),
        model: inp.resolved.model.clone(),
        effort: inp.resolved.effort.clone(),
        system: cache_system,
        input: cache_input,
        images: inp.images.clone(),
        response_schema: inp.response_schema.clone(),
        want_reasoning: opts.want_reasoning,
        stream_hint: opts.stream,
        scratch: inp.scratch.clone(),
        resume: resume.clone(),
    };

    let started = Instant::now();
    let inner = harness
        .run(turn, cancel.clone())
        .await
        .map_err(ApiError::from)?;

    let limits = state.cfg.limits.clone();
    let json_mode = opts.json_mode;
    let max_chars = opts
        .max_tokens
        .filter(|_| state.cfg.compat.max_tokens_truncation)
        .map(|t| t as usize * 4);
    let stops: Vec<String> = if state.cfg.compat.stop_sequences {
        opts.stop.into_iter().filter(|s| !s.is_empty()).collect()
    } else {
        vec![]
    };
    let hold_back = stops
        .iter()
        .map(|s| s.len().saturating_sub(1))
        .max()
        .unwrap_or(0);
    let state2 = state.clone();
    let messages = inp.messages;
    let model_for_cache = inp.resolved.model.clone();
    let request_id = inp.request_id.clone();
    let prompt_chars = inp.prompt_chars;
    let scratch = inp.scratch.clone();

    let out = stream! {
        let _permit = permit;
        let _scratch = scratch;
        let _cancel_guard = CancelOnDrop(cancel.clone());
        let mut inner = inner;
        let deadline = tokio::time::Instant::now() + limits.request_timeout;
        let first_token_deadline = tokio::time::Instant::now() + limits.first_token_timeout;

        let mut acc = String::new();          // everything received from the harness
        let mut emitted = 0usize;             // bytes of `acc` already sent to the client
        let mut got_first = false;
        let mut usage: Option<Usage> = None;
        let mut continuation: Option<String> = None;
        let mut finish: Option<FinishReason> = None;
        let mut error: Option<ApiError> = None;
        let mut ttfb_ms: Option<u64> = None;
        let mut truncated = false;
        let mut pending_text = String::new();
        let mut batch_deadline: Option<tokio::time::Instant> = None;

        // Returns the stop position in `acc` if any stop sequence has appeared.
        let find_stop = |acc: &str| -> Option<usize> {
            stops.iter().filter_map(|s| acc.find(s.as_str())).min()
        };

        loop {
            let now = tokio::time::Instant::now();
            let next_deadline = if !got_first { first_token_deadline.min(deadline) } else { deadline };
            let batch_due = batch_deadline.filter(|d| *d <= now).is_some();
            if batch_due && !pending_text.is_empty() {
                let t = std::mem::take(&mut pending_text);
                batch_deadline = None;
                yield OutEvent::Text(t);
                continue;
            }
            let wait_until = match batch_deadline { Some(b) => b.min(next_deadline), None => next_deadline };

            let ev = tokio::select! {
                _ = tokio::time::sleep_until(wait_until) => {
                    if batch_deadline.is_some_and(|b| b <= tokio::time::Instant::now()) && tokio::time::Instant::now() < next_deadline {
                        continue;
                    }
                    let msg = if !got_first { "harness produced no output before the first-token timeout" } else { "harness exceeded the request timeout" };
                    error = Some(ApiError::from(crate::harness::HarnessError::timeout(msg)));
                    cancel.cancel();
                    break;
                }
                e = inner.next() => e,
            };
            let Some(ev) = ev else {
                // Stream ended without Done/Error: treat as stop if we have text.
                if finish.is_none() && error.is_none() {
                    if acc.is_empty() {
                        error = Some(ApiError::from(crate::harness::HarnessError::failed("harness stream ended unexpectedly")));
                    } else {
                        finish = Some(FinishReason::Stop);
                    }
                }
                break;
            };
            match ev {
                Event::ResolvedModel(m) => yield OutEvent::Resolved(m),
                Event::Continuation(c) => continuation = Some(c),
                Event::Usage(u) => usage = Some(u),
                Event::ReasoningDelta(d) => {
                    if !got_first { got_first = true; ttfb_ms = Some(started.elapsed().as_millis() as u64); }
                    if !pending_text.is_empty() { yield OutEvent::Text(std::mem::take(&mut pending_text)); batch_deadline = None; }
                    yield OutEvent::Reasoning(d);
                }
                Event::TextDelta(d) => {
                    if truncated { continue; }
                    if !got_first { got_first = true; ttfb_ms = Some(started.elapsed().as_millis() as u64); }
                    acc.push_str(&d);
                    if acc.len() > limits.max_completion_bytes {
                        acc.truncate(floor_char(&acc, limits.max_completion_bytes));
                        truncated = true;
                        finish = Some(FinishReason::Length);
                        cancel.cancel();
                    }
                    if let Some(pos) = find_stop(&acc) {
                        acc.truncate(pos);
                        truncated = true;
                        finish = Some(FinishReason::Stop);
                        cancel.cancel();
                    }
                    if let Some(max) = max_chars {
                        if acc.chars().count() > max {
                            let cut = acc.char_indices().nth(max).map(|(i, _)| i).unwrap_or(acc.len());
                            acc.truncate(cut);
                            truncated = true;
                            finish = Some(FinishReason::Length);
                            cancel.cancel();
                        }
                    }
                    // In JSON mode we buffer everything and emit the extracted value at the end.
                    if json_mode { if truncated { break; } continue; }
                    let safe_end = if truncated { acc.len() } else { floor_char(&acc, acc.len().saturating_sub(hold_back)) };
                    if safe_end > emitted {
                        pending_text.push_str(&acc[emitted..safe_end]);
                        emitted = safe_end;
                        if limits.micro_batch.is_zero() {
                            yield OutEvent::Text(std::mem::take(&mut pending_text));
                        } else if batch_deadline.is_none() {
                            batch_deadline = Some(tokio::time::Instant::now() + limits.micro_batch);
                        }
                    }
                    if truncated { break; }
                }
                Event::Done(r) => { if finish.is_none() { finish = Some(r); } break; }
                Event::Error(e) => {
                    if acc.is_empty() || e.kind != crate::harness::ErrorKind::Failed {
                        error = Some(ApiError::from(e));
                    } else {
                        // Mid-stream failure after output: finish what we have and attach the error.
                        error = Some(ApiError::from(e));
                        finish = Some(FinishReason::Stop);
                    }
                    break;
                }
            }
        }

        // Flush held-back text.
        if json_mode {
            let text = crate::prompt::extract_json(&acc).unwrap_or_else(|| acc.clone());
            if !text.is_empty() { yield OutEvent::Text(text); }
        } else {
            if emitted < acc.len() { pending_text.push_str(&acc[emitted..]); }
            if !pending_text.is_empty() { yield OutEvent::Text(std::mem::take(&mut pending_text)); }
        }

        let usage = usage.unwrap_or_else(|| Usage::estimate(prompt_chars, acc.len()));
        let duration_ms = started.elapsed().as_millis() as u64;
        tracing::info!(
            req_id = %request_id,
            harness = %kind,
            model = %model_for_cache,
            ttfb_ms = ttfb_ms.unwrap_or(0),
            duration_ms,
            pool_wait_ms,
            completion_chars = acc.len(),
            prompt_tokens = usage.prompt_tokens,
            completion_tokens = usage.completion_tokens,
            usage_estimated = usage.estimated,
            finish = ?finish,
            error = error.as_ref().map(|e| e.message.as_str()).unwrap_or(""),
            "turn finished"
        );

        if let (Some(sc), Some(c), Some(FinishReason::Stop), None) = (&state2.session_cache, &continuation, finish, &error) {
            if !truncated {
                let mut msgs = messages.clone();
                msgs.push(Message { role: "assistant".into(), content: Some(crate::openai::Content::Text(acc.clone())), name: None, tool_calls: None, tool_call_id: None });
                sc.store(kind, &model_for_cache, &msgs, c.clone(), acc.clone());
            }
        } else if let (Some(c), None) = (&continuation, &state2.session_cache) {
            // Nothing to persist; harness handles ephemeral cleanup.
            let _ = c;
        }

        yield OutEvent::Usage(usage);
        match (finish, error) {
            (Some(f), None) => yield OutEvent::Finish(f),
            (Some(f), Some(e)) => { yield OutEvent::Finish(f); yield OutEvent::Error(e); }
            (None, Some(e)) => yield OutEvent::Error(e),
            (None, None) => yield OutEvent::Finish(FinishReason::Stop),
        }
    };

    Ok(Started {
        stream: Box::pin(out),
        harness: kind,
    })
}

fn floor_char(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

pub fn finish_str(f: FinishReason) -> &'static str {
    match f {
        FinishReason::Stop | FinishReason::Interrupted => "stop",
        FinishReason::Length => "length",
    }
}

#[allow(dead_code)]
pub fn elapsed_since(t: Instant) -> Duration {
    t.elapsed()
}
