//! End-to-end API conformance tests against the in-process fake harness.

use std::sync::Arc;

use futures::StreamExt;
use locum::config::Config;
use locum::harness::{
    fake::FakeHarness, Event, EventStream, FinishReason, Harness, HarnessError, HarnessInfo,
    HarnessKind, ModelInfo, Registry, Turn,
};
use locum::server::{router, AppState};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

async fn spawn_server(mut cfg: Config) -> String {
    cfg.models.default = "fake".into();
    cfg.models.discover = false;
    cfg.models
        .aliases
        .insert("gpt-4o".into(), "fake/fake".into());
    cfg.limits.micro_batch = std::time::Duration::from_millis(0);
    let cfg = Arc::new(cfg);
    let registry = Arc::new(Registry::with(vec![Arc::new(FakeHarness)]));
    let state = AppState::new(cfg, registry);
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[tokio::test]
async fn models_list_and_get() {
    let base = spawn_server(Config::default()).await;
    let v: Value = client()
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["object"], "list");
    let ids: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"default"));
    assert!(ids.contains(&"gpt-4o"));
    assert!(ids.contains(&"fake"));

    let r = client()
        .get(format!("{base}/v1/models/nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "model_not_found");
}

#[tokio::test]
async fn non_streaming_completion() {
    let base = spawn_server(Config::default()).await;
    let r = client()
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth("sk-anything")
        .json(&json!({ "model": "gpt-4o", "messages": [{ "role": "user", "content": "hello big world" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers().get("x-locum-harness").unwrap(), "fake");
    assert_eq!(r.headers().get("x-locum-usage").unwrap(), "exact");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["model"], "gpt-4o");
    assert_eq!(v["choices"][0]["message"]["content"], "hello big world");
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert!(v["usage"]["total_tokens"].as_u64().unwrap() > 0);
    assert!(v["id"].as_str().unwrap().starts_with("chatcmpl-"));
}

async fn read_sse(r: reqwest::Response) -> Vec<String> {
    let mut frames = Vec::new();
    let mut buf = String::new();
    let mut body = r.bytes_stream();
    while let Some(chunk) = body.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        while let Some(pos) = buf.find("\n\n") {
            let frame = buf[..pos].to_string();
            buf = buf[pos + 2..].to_string();
            if let Some(d) = frame.strip_prefix("data: ") {
                frames.push(d.to_string());
            }
        }
    }
    frames
}

#[tokio::test]
async fn streaming_completion_with_usage() {
    let base = spawn_server(Config::default()).await;
    let r = client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "fake", "stream": true,
            "stream_options": { "include_usage": true },
            "messages": [{ "role": "user", "content": "one two three" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    let frames = read_sse(r).await;
    assert_eq!(frames.last().unwrap(), "[DONE]");
    let chunks: Vec<Value> = frames[..frames.len() - 1]
        .iter()
        .map(|f| serde_json::from_str(f).unwrap())
        .collect();
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    let id = chunks[0]["id"].as_str().unwrap();
    assert!(chunks.iter().all(|c| c["id"] == id));
    let text: String = chunks
        .iter()
        .filter_map(|c| {
            c["choices"][0]["delta"]["content"]
                .as_str()
                .map(String::from)
        })
        .collect();
    assert_eq!(text, "one two three");
    let finish = chunks
        .iter()
        .find(|c| c["choices"][0]["finish_reason"] == "stop")
        .expect("finish chunk");
    assert!(finish["choices"][0]["delta"]
        .as_object()
        .unwrap()
        .is_empty());
    let usage = chunks.last().unwrap();
    assert!(usage["choices"].as_array().unwrap().is_empty());
    assert!(usage["usage"]["completion_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn streaming_mid_stream_error_terminates_cleanly() {
    let base = spawn_server(Config::default()).await;
    let r = client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "fake", "stream": true, "messages": [{ "role": "user", "content": "a b c d !error" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let frames = read_sse(r).await;
    assert_eq!(frames.last().unwrap(), "[DONE]");
    let n = frames.len();
    let err: Value = serde_json::from_str(&frames[n - 2]).unwrap();
    assert_eq!(err["error"]["code"], "harness_error");
    let finish: Value = serde_json::from_str(&frames[n - 3]).unwrap();
    assert_eq!(finish["choices"][0]["finish_reason"], "stop");
}

#[tokio::test]
async fn error_envelopes() {
    let base = spawn_server(Config::default()).await;
    let c = client();

    let r = c
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "fake", "n": 2, "messages": [{ "role": "user", "content": "x" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "unsupported_parameter");
    assert_eq!(v["error"]["param"], "n");

    let r = c
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "fake", "tools": [{"type":"function"}], "messages": [{ "role": "user", "content": "x" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    let r = c
        .post(format!("{base}/v1/chat/completions"))
        .json(
            &json!({ "model": "does-not-exist", "messages": [{ "role": "user", "content": "x" }] }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    let r = c
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "fake", "messages": [{ "role": "user", "content": "!context" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "context_length_exceeded");

    let r = c
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "fake", "messages": [{ "role": "user", "content": "!unavailable" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);

    let r = c
        .post(format!("{base}/v1/chat/completions"))
        .body("{not json")
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn auth_keys_enforced() {
    let mut cfg = Config::default();
    cfg.auth.keys = vec!["sk-local-secret".into()];
    let base = spawn_server(cfg).await;
    let c = client();
    let body = json!({ "model": "fake", "messages": [{ "role": "user", "content": "hi" }] });
    let r = c
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = c
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth("wrong")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = c
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth("sk-local-secret")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = c
        .post(format!("{base}/v1/chat/completions"))
        .header("x-api-key", "sk-local-secret")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = c.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn stop_sequences_and_max_tokens() {
    let base = spawn_server(Config::default()).await;
    let c = client();
    let r = c
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "fake", "stop": ["three"], "messages": [{ "role": "user", "content": "one two three four" }] }))
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "one two ");

    let r = c
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "fake", "max_tokens": 1, "messages": [{ "role": "user", "content": "abcdefgh ijklmnop" }] }))
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "length");
    assert_eq!(
        v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        4
    );
}

#[tokio::test]
async fn json_object_mode_extracts_json() {
    let base = spawn_server(Config::default()).await;
    let r = client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "fake", "response_format": {"type":"json_object"}, "messages": [{ "role": "user", "content": "Sure: {\"a\": 1} done" }] }))
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "{\"a\": 1}");
}

#[tokio::test]
async fn reasoning_content_when_enabled() {
    let mut cfg = Config::default();
    cfg.compat.reasoning_content = true;
    let base = spawn_server(cfg).await;
    let r = client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "fake", "messages": [{ "role": "user", "content": "hi !reasoning" }] }))
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(
        v["choices"][0]["message"]["reasoning_content"],
        "thinking about it"
    );
    assert_eq!(v["choices"][0]["message"]["content"], "hi");
}

#[tokio::test]
async fn legacy_completions() {
    let base = spawn_server(Config::default()).await;
    let r = client()
        .post(format!("{base}/v1/completions"))
        .json(&json!({ "model": "fake", "prompt": "ping pong" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["object"], "text_completion");
    assert_eq!(v["choices"][0]["text"], "ping pong");
}

#[tokio::test]
async fn multi_turn_is_rendered_as_transcript() {
    let base = spawn_server(Config::default()).await;
    let r = client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "fake", "messages": [
            { "role": "system", "content": "be terse" },
            { "role": "user", "content": "a" },
            { "role": "assistant", "content": "b" },
            { "role": "user", "content": "c" }
        ] }))
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    let text = v["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(text.contains("<conversation>"));
    assert!(text.ends_with(" c"));
}

#[tokio::test]
async fn client_disconnect_cancels_turn() {
    let base = spawn_server(Config::default()).await;
    let r = client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "model": "fake", "stream": true, "messages": [{ "role": "user", "content": "!slow w1 w2 w3 w4 w5 w6 w7 w8 w9 w10 w11 w12 w13 w14 w15 w16 w17 w18 w19 w20" }] }))
        .send()
        .await
        .unwrap();
    let mut body = r.bytes_stream();
    let _ = body.next().await;
    drop(body);
    // If cancellation did not propagate the permit would stay held; check the server is still responsive
    // and the in-flight gauge returns to idle.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let v: Value = client()
        .get(format!("{base}/healthz"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["inflight_available"], 8);
}

/// Streams until cancelled, then records that its cleanup path actually ran.
struct CancelProbe(Arc<tokio::sync::Notify>);

#[async_trait::async_trait]
impl Harness for CancelProbe {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Fake
    }

    async fn probe(&self) -> Result<HarnessInfo, HarnessError> {
        FakeHarness.probe().await
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, HarnessError> {
        FakeHarness.list_models().await
    }

    async fn run(
        &self,
        _turn: Turn,
        cancel: CancellationToken,
    ) -> Result<EventStream, HarnessError> {
        let cleaned_up = self.0.clone();
        Ok(Box::pin(async_stream::stream! {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        // Real harnesses await an interrupt request here.
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        cleaned_up.notify_one();
                        yield Event::Done(FinishReason::Interrupted);
                        return;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {
                        yield Event::TextDelta("word ".into());
                    }
                }
            }
        }))
    }
}

#[tokio::test]
async fn client_disconnect_runs_harness_cancel_path() {
    let cleaned_up = Arc::new(tokio::sync::Notify::new());
    let mut cfg = Config::default();
    cfg.models.default = "fake".into();
    cfg.models.discover = false;
    cfg.limits.micro_batch = std::time::Duration::from_millis(0);
    let registry = Arc::new(Registry::with(vec![Arc::new(CancelProbe(
        cleaned_up.clone(),
    ))]));
    let app = router(AppState::new(Arc::new(cfg), registry));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    for stream in [true, false] {
        let request = client()
            .post(format!("{base}/v1/chat/completions"))
            .json(&json!({ "model": "fake", "stream": stream, "messages": [{ "role": "user", "content": "hi" }] }));
        if stream {
            let mut body = request.send().await.unwrap().bytes_stream();
            let _ = body.next().await;
        } else {
            // Abandon a non-streaming request while the turn is still running.
            let _ =
                tokio::time::timeout(std::time::Duration::from_millis(200), request.send()).await;
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), cleaned_up.notified())
            .await
            .unwrap_or_else(|_| panic!("harness cancel path did not run (stream={stream})"));
    }
}
