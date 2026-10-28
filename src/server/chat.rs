use std::sync::Arc;

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;

use super::error::{json_rejection, ApiError};
use super::turn::{self, finish_str, OutEvent, Started, TurnInput, TurnOptions};
use super::{AppState, RequestId};
use crate::harness::{FinishReason, HarnessKind};
use crate::openai::{
    self, AssistantMessage, ChatCompletion, ChatCompletionChunk, ChatCompletionRequest, Choice,
    ChunkChoice, CompletionTokensDetails, Delta, PromptTokensDetails, ResponseFormat, Usage,
};
use crate::routing::Resolved;

fn reject_unsupported(req: &ChatCompletionRequest, ignore_tools: bool) -> Result<(), ApiError> {
    if let Some(n) = req.n {
        if n != 1 {
            return Err(ApiError::unsupported("n"));
        }
    }
    let tool_fields: [(&str, bool); 4] = [
        ("tools", req.tools.as_ref().is_some_and(non_empty)),
        ("functions", req.functions.as_ref().is_some_and(non_empty)),
        (
            "tool_choice",
            req.tool_choice.is_some() && req.tool_choice != Some(Value::String("none".into())),
        ),
        ("parallel_tool_calls", false),
    ];
    for (name, present) in tool_fields {
        if present && !ignore_tools {
            return Err(ApiError::unsupported(name));
        }
    }
    if req
        .logprobs
        .as_ref()
        .is_some_and(|v| v != &Value::Bool(false) && !v.is_null())
    {
        return Err(ApiError::unsupported("logprobs"));
    }
    if req.logit_bias.as_ref().is_some_and(non_empty) {
        return Err(ApiError::unsupported("logit_bias"));
    }
    if req.audio.is_some() {
        return Err(ApiError::unsupported("audio"));
    }
    if let Some(m) = &req.modalities {
        let only_text = m
            .as_array()
            .is_none_or(|a| a.iter().all(|x| x.as_str() == Some("text")));
        if !only_text {
            return Err(ApiError::unsupported("modalities"));
        }
    }
    for (name, set) in [
        ("temperature", req.temperature.is_some()),
        ("top_p", req.top_p.is_some()),
        ("presence_penalty", req.presence_penalty.is_some()),
        ("frequency_penalty", req.frequency_penalty.is_some()),
        ("seed", req.seed.is_some()),
    ] {
        if set {
            tracing::debug!(param = name, "accepted and ignored");
        }
    }
    Ok(())
}

fn non_empty(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => true,
    }
}

pub struct Prepared {
    pub input: TurnInput,
    pub opts: TurnOptions,
    pub include_usage: bool,
}

pub fn prepare(
    state: &Arc<AppState>,
    request_id: &str,
    req: ChatCompletionRequest,
) -> Result<Prepared, ApiError> {
    let cfg = &state.cfg;
    reject_unsupported(&req, cfg.compat.ignore_tools)?;
    if req.messages.is_empty() {
        let mut e = ApiError::invalid("`messages` must contain at least one message");
        e.param = Some("messages".into());
        return Err(e);
    }

    let resolved: Resolved = {
        let mut r = crate::routing::resolve(cfg, &req.model)?;
        if let Some(e) = &req.reasoning_effort {
            r.effort = Some(e.clone());
        }
        r
    };

    let scratch = Arc::new(
        tempfile::Builder::new()
            .prefix("locum-")
            .tempdir()
            .map_err(|e| {
                ApiError::from(crate::harness::HarnessError::failed(format!(
                    "scratch dir: {e}"
                )))
            })?,
    );

    // OpenCode carries the preamble in its agent definition; everything else gets it in `system`.
    let mut prompt_cfg = cfg.prompt.clone();
    if resolved.harness == HarnessKind::Opencode {
        prompt_cfg.preamble = false;
    }
    let allow_images = resolved.harness == HarnessKind::Codex;
    let rendered = crate::prompt::render(
        &req.messages,
        req.response_format.as_ref(),
        &prompt_cfg,
        scratch.path(),
        cfg.compat.ignore_tools,
        allow_images,
    )?;
    if rendered.prompt_chars > cfg.limits.max_prompt_chars {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            kind: "invalid_request_error",
            code: Some("context_length_exceeded"),
            message: format!(
                "prompt is {} characters; limit is {}",
                rendered.prompt_chars, cfg.limits.max_prompt_chars
            ),
            param: Some("messages".into()),
        });
    }

    let json_mode = matches!(
        req.response_format,
        Some(ResponseFormat::JsonObject) | Some(ResponseFormat::JsonSchema { .. })
    );
    let max_tokens = req.max_completion_tokens.or(req.max_tokens);
    let include_usage = req.stream_options.as_ref().is_some_and(|s| s.include_usage);

    Ok(Prepared {
        input: TurnInput {
            request_id: request_id.to_string(),
            resolved,
            system: rendered.system,
            input: rendered.input,
            images: rendered.images,
            response_schema: rendered.response_schema,
            scratch,
            prompt_chars: rendered.prompt_chars,
            messages: req.messages,
        },
        opts: TurnOptions {
            stream: req.stream,
            max_tokens,
            stop: req.stop.map(|s| s.into_vec()).unwrap_or_default(),
            want_reasoning: cfg.compat.reasoning_content,
            json_mode,
        },
        include_usage,
    })
}

pub fn to_openai_usage(u: &crate::harness::Usage) -> Usage {
    Usage {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: u.completion_tokens,
        total_tokens: u.prompt_tokens + u.completion_tokens,
        prompt_tokens_details: (u.cached_prompt_tokens > 0).then_some(PromptTokensDetails {
            cached_tokens: u.cached_prompt_tokens,
        }),
        completion_tokens_details: (u.reasoning_tokens > 0).then_some(CompletionTokensDetails {
            reasoning_tokens: u.reasoning_tokens,
        }),
    }
}

pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    payload: Result<Json<ChatCompletionRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(req) = payload.map_err(json_rejection)?;
    let requested_model = req.model.clone();
    let stream = req.stream;
    let prepared = prepare(&state, &request_id, req)?;
    let resolved_label = format!(
        "{}/{}",
        prepared.input.resolved.harness, prepared.input.resolved.model
    );
    let started = turn::start(state.clone(), prepared.input, prepared.opts).await?;

    if stream {
        Ok(stream_response(
            started,
            requested_model,
            resolved_label,
            prepared.include_usage,
            state.fingerprint(),
        ))
    } else {
        collect_response(
            started,
            requested_model,
            resolved_label,
            state.fingerprint(),
        )
        .await
    }
}

fn common_headers(res: &mut Response, harness: HarnessKind, resolved: &str) {
    let h = res.headers_mut();
    h.insert(
        "x-locum-harness",
        HeaderValue::from_static(harness.as_str()),
    );
    if let Ok(v) = HeaderValue::from_str(resolved) {
        h.insert("x-locum-resolved-model", v);
    }
}

async fn collect_response(
    started: Started,
    model: String,
    resolved_label: String,
    fingerprint: String,
) -> Result<Response, ApiError> {
    let Started {
        mut stream,
        harness,
    } = started;
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut usage: Option<crate::harness::Usage> = None;
    let mut finish: Option<FinishReason> = None;
    let mut error: Option<ApiError> = None;
    while let Some(ev) = stream.next().await {
        match ev {
            OutEvent::Resolved(_) => {}
            OutEvent::Text(t) => content.push_str(&t),
            OutEvent::Reasoning(r) => reasoning.push_str(&r),
            OutEvent::Usage(u) => usage = Some(u),
            OutEvent::Finish(f) => finish = Some(f),
            OutEvent::Error(e) => error = Some(e),
        }
    }
    if let Some(e) = error {
        if content.is_empty() {
            return Err(e);
        }
        tracing::warn!(error = %e.message, "harness error after partial output; returning partial completion");
    }
    let usage = usage.unwrap_or_default();
    let body = ChatCompletion {
        id: openai::completion_id(),
        object: "chat.completion",
        created: openai::now_unix(),
        model,
        system_fingerprint: Some(fingerprint),
        choices: vec![Choice {
            index: 0,
            message: AssistantMessage {
                role: "assistant",
                content: Some(content),
                reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
                refusal: None,
            },
            finish_reason: finish_str(finish.unwrap_or(FinishReason::Stop)),
            logprobs: None,
        }],
        usage: to_openai_usage(&usage),
    };
    let mut res = Json(body).into_response();
    common_headers(&mut res, harness, &resolved_label);
    res.headers_mut().insert(
        "x-locum-usage",
        HeaderValue::from_static(if usage.estimated {
            "estimated"
        } else {
            "exact"
        }),
    );
    Ok(res)
}

fn sse_frame<T: serde::Serialize>(v: &T) -> Bytes {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"data: ");
    serde_json::to_writer(&mut buf, v).expect("serialisable");
    buf.extend_from_slice(b"\n\n");
    Bytes::from(buf)
}

fn stream_response(
    started: Started,
    model: String,
    resolved_label: String,
    include_usage: bool,
    fingerprint: String,
) -> Response {
    let Started { stream, harness } = started;
    let id = openai::completion_id();
    let created = openai::now_unix();

    let chunk = {
        let id = id.clone();
        let model = model.clone();
        let fp = fingerprint.clone();
        move |delta: Delta, finish: Option<&'static str>| ChatCompletionChunk {
            id: id.clone(),
            object: "chat.completion.chunk",
            created,
            model: model.clone(),
            system_fingerprint: Some(fp.clone()),
            choices: vec![ChunkChoice {
                index: 0,
                delta,
                finish_reason: finish,
                logprobs: None,
            }],
            usage: None,
        }
    };

    let body_stream = async_stream::stream! {
        let mut stream = stream;
        yield Ok::<Bytes, std::convert::Infallible>(sse_frame(&chunk(Delta { role: Some("assistant"), content: Some(String::new()), reasoning_content: None }, None)));
        let mut usage: Option<crate::harness::Usage> = None;
        let mut sent_finish = false;
        while let Some(ev) = stream.next().await {
            match ev {
                OutEvent::Resolved(_) => {}
                OutEvent::Text(t) => yield Ok(sse_frame(&chunk(Delta { content: Some(t), ..Default::default() }, None))),
                OutEvent::Reasoning(r) => yield Ok(sse_frame(&chunk(Delta { reasoning_content: Some(r), ..Default::default() }, None))),
                OutEvent::Usage(u) => usage = Some(u),
                OutEvent::Finish(f) => {
                    sent_finish = true;
                    yield Ok(sse_frame(&chunk(Delta::default(), Some(finish_str(f)))));
                }
                OutEvent::Error(e) => {
                    if !sent_finish {
                        sent_finish = true;
                        yield Ok(sse_frame(&chunk(Delta::default(), Some("stop"))));
                    }
                    yield Ok(sse_frame(&e.envelope()));
                }
            }
        }
        if !sent_finish {
            yield Ok(sse_frame(&chunk(Delta::default(), Some("stop"))));
        }
        if include_usage {
            let u = usage.unwrap_or_default();
            let usage_chunk = ChatCompletionChunk {
                id: id.clone(),
                object: "chat.completion.chunk",
                created,
                model: model.clone(),
                system_fingerprint: Some(fingerprint.clone()),
                choices: vec![],
                usage: Some(to_openai_usage(&u)),
            };
            yield Ok(sse_frame(&usage_chunk));
        }
        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
    };

    let mut res = Response::new(Body::from_stream(body_stream));
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    common_headers(&mut res, harness, &resolved_label);
    res
}
