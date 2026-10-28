//! Legacy `POST /v1/completions` shim: the prompt becomes one user message.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, State};
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures::StreamExt;

use super::chat::{prepare, to_openai_usage};
use super::error::{json_rejection, ApiError};
use super::turn::{self, finish_str, OutEvent, Started};
use super::{AppState, RequestId};
use crate::harness::FinishReason;
use crate::openai::{
    self, ChatCompletionRequest, CompletionRequest, Content, Message, Prompt, TextChoice,
    TextCompletion,
};

pub async fn completions(
    State(state): State<Arc<AppState>>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    payload: Result<Json<CompletionRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(req) = payload.map_err(json_rejection)?;
    let prompt = match req.prompt {
        Some(Prompt::One(s)) => s,
        Some(Prompt::Many(v)) if v.len() == 1 => v.into_iter().next().unwrap(),
        Some(Prompt::Many(_)) => {
            return Err(ApiError::unsupported("prompt[] with more than one entry"))
        }
        Some(Prompt::Tokens(_)) => return Err(ApiError::unsupported("prompt as token ids")),
        None => return Err(ApiError::invalid("`prompt` is required")),
    };
    if req.best_of.is_some_and(|b| b > 1) {
        return Err(ApiError::unsupported("best_of"));
    }
    if req.echo == Some(true) {
        return Err(ApiError::unsupported("echo"));
    }
    if req.suffix.is_some() {
        return Err(ApiError::unsupported("suffix"));
    }

    let chat_req = ChatCompletionRequest {
        model: req.model.clone(),
        messages: vec![Message {
            role: "user".into(),
            content: Some(Content::Text(prompt)),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        stream: req.stream,
        stream_options: req.stream_options,
        max_tokens: req.max_tokens,
        max_completion_tokens: None,
        temperature: req.temperature,
        top_p: req.top_p,
        presence_penalty: req.presence_penalty,
        frequency_penalty: req.frequency_penalty,
        seed: req.seed,
        stop: req.stop,
        user: req.user,
        n: req.n,
        reasoning_effort: None,
        response_format: None,
        tools: None,
        functions: None,
        tool_choice: None,
        logprobs: req.logprobs,
        top_logprobs: None,
        logit_bias: req.logit_bias,
        audio: None,
        modalities: None,
        parallel_tool_calls: None,
    };
    let stream = req.stream;
    let model = req.model;
    let prepared = prepare(&state, &request_id, chat_req)?;
    let include_usage = prepared.include_usage;
    let started = turn::start(state.clone(), prepared.input, prepared.opts).await?;

    if stream {
        Ok(stream_text(started, model, include_usage))
    } else {
        collect_text(started, model).await
    }
}

async fn collect_text(started: Started, model: String) -> Result<Response, ApiError> {
    let Started {
        mut stream,
        harness,
    } = started;
    let mut text = String::new();
    let mut usage = None;
    let mut finish = None;
    let mut error = None;
    while let Some(ev) = stream.next().await {
        match ev {
            OutEvent::Text(t) => text.push_str(&t),
            OutEvent::Usage(u) => usage = Some(u),
            OutEvent::Finish(f) => finish = Some(f),
            OutEvent::Error(e) => error = Some(e),
            _ => {}
        }
    }
    if let Some(e) = error {
        if text.is_empty() {
            return Err(e);
        }
    }
    let body = TextCompletion {
        id: openai::completion_id().replace("chatcmpl-", "cmpl-"),
        object: "text_completion",
        created: openai::now_unix(),
        model,
        choices: vec![TextChoice {
            index: 0,
            text,
            finish_reason: Some(finish_str(finish.unwrap_or(FinishReason::Stop))),
            logprobs: None,
        }],
        usage: Some(to_openai_usage(&usage.unwrap_or_default())),
    };
    let mut res = Json(body).into_response();
    res.headers_mut().insert(
        "x-locum-harness",
        HeaderValue::from_static(harness.as_str()),
    );
    Ok(res)
}

fn frame<T: serde::Serialize>(v: &T) -> Bytes {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"data: ");
    serde_json::to_writer(&mut buf, v).expect("serialisable");
    buf.extend_from_slice(b"\n\n");
    Bytes::from(buf)
}

fn stream_text(started: Started, model: String, include_usage: bool) -> Response {
    let Started { stream, harness } = started;
    let id = openai::completion_id().replace("chatcmpl-", "cmpl-");
    let created = openai::now_unix();
    let mk = {
        let id = id.clone();
        let model = model.clone();
        move |text: String, finish: Option<&'static str>, usage: Option<openai::Usage>| {
            TextCompletion {
                id: id.clone(),
                object: "text_completion",
                created,
                model: model.clone(),
                choices: vec![TextChoice {
                    index: 0,
                    text,
                    finish_reason: finish,
                    logprobs: None,
                }],
                usage,
            }
        }
    };
    let body = async_stream::stream! {
        let mut stream = stream;
        let mut usage = None;
        let mut finished = false;
        while let Some(ev) = stream.next().await {
            match ev {
                OutEvent::Text(t) => yield Ok::<Bytes, std::convert::Infallible>(frame(&mk(t, None, None))),
                OutEvent::Usage(u) => usage = Some(u),
                OutEvent::Finish(f) => { finished = true; yield Ok(frame(&mk(String::new(), Some(finish_str(f)), None))); }
                OutEvent::Error(e) => {
                    if !finished { finished = true; yield Ok(frame(&mk(String::new(), Some("stop"), None))); }
                    yield Ok(frame(&e.envelope()));
                }
                _ => {}
            }
        }
        if !finished { yield Ok(frame(&mk(String::new(), Some("stop"), None))); }
        if include_usage {
            yield Ok(frame(&TextCompletion { id: id.clone(), object: "text_completion", created, model: model.clone(), choices: vec![], usage: Some(to_openai_usage(&usage.unwrap_or_default())) }));
        }
        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
    };
    let mut res = Response::new(Body::from_stream(body));
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    h.insert(
        "x-locum-harness",
        HeaderValue::from_static(harness.as_str()),
    );
    res
}
