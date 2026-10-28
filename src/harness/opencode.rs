//! OpenCode v2 adapter: a private `opencode serve` process driven over HTTP + SSE.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::StreamExt;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::util::{classify_message, Reconciler};
use super::{
    ErrorKind, Event, EventStream, FinishReason, Harness, HarnessError, HarnessInfo, HarnessKind,
    ModelInfo, Turn, Usage,
};
use crate::config::{LimitsConfig, OpencodeConfig};
use crate::process::pool::{SharedPool, SharedWorker};
use crate::process::{resolve_executable, ChildGuard, CommandBuilder};

const AGENT: &str = "locum";

pub struct ServeProcess {
    guard: Mutex<Option<ChildGuard>>,
    base: String,
    password: String,
    client: reqwest::Client,
    alive: Arc<AtomicBool>,
    _workdir: tempfile::TempDir,
}

fn free_port() -> Result<u16, HarnessError> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| HarnessError::unavailable(format!("no free port: {e}")))?;
    Ok(l.local_addr()
        .map_err(|e| HarnessError::unavailable(e.to_string()))?
        .port())
}

fn random_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut b);
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, b)
}

fn config_content() -> String {
    json!({
        "agent": {
            AGENT: {
                "mode": "primary",
                "prompt": crate::prompt::DEFAULT_PREAMBLE,
                "tools": { "*": false },
                "permission": { "*": "deny" }
            }
        },
        "share": "disabled",
        "autoupdate": false
    })
    .to_string()
}

impl ServeProcess {
    async fn start(cfg: &OpencodeConfig) -> Result<Arc<Self>, HarnessError> {
        let exe = resolve_executable("opencode", cfg.executable.as_deref())?;
        let port = free_port()?;
        let password = random_token();
        let workdir = tempfile::Builder::new()
            .prefix("locum-opencode-")
            .tempdir()
            .map_err(|e| HarnessError::unavailable(format!("tempdir: {e}")))?;
        let mut cmd = CommandBuilder::new(&exe)
            .args(["serve", "--hostname", "127.0.0.1", "--port"])
            .arg(port.to_string())
            .args(["--log-level", "warn"])
            .env("OPENCODE_SERVER_PASSWORD", &password)
            .env("OPENCODE_CONFIG_CONTENT", config_content())
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env("OPENCODE_DISABLE_PROJECT_CONFIG", "1")
            .strip_provider_keys(cfg.strip_provider_keys)
            .cwd(workdir.path())
            .no_stdin();
        if cfg.isolate_config {
            let dir = crate::config::data_dir().join("opencode-config");
            std::fs::create_dir_all(&dir)
                .map_err(|e| HarnessError::unavailable(format!("create {}: {e}", dir.display())))?;
            cmd = cmd.env("OPENCODE_CONFIG_DIR", dir);
        }
        tracing::debug!(argv = %cmd.argv_for_log(), "spawning opencode serve");
        let child = cmd
            .build()
            .spawn()
            .map_err(|e| HarnessError::unavailable(format!("failed to spawn opencode: {e}")))?;
        let mut guard = ChildGuard::new(child, "opencode", 8192);
        // Drain stdout so the child never blocks on a full pipe.
        if let Some(out) = guard.take_stdout() {
            tokio::spawn(async move {
                let mut rd = tokio::io::BufReader::new(out);
                let mut s = String::new();
                while let Ok(n) = tokio::io::AsyncBufReadExt::read_line(&mut rd, &mut s).await {
                    if n == 0 {
                        break;
                    }
                    s.clear();
                }
            });
        }

        let client = reqwest::Client::builder()
            .no_proxy()
            .tcp_nodelay(true)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| HarnessError::unavailable(format!("http client: {e}")))?;
        let base = format!("http://127.0.0.1:{port}");
        let me = Arc::new(Self {
            guard: Mutex::new(Some(guard)),
            base,
            password,
            client,
            alive: Arc::new(AtomicBool::new(true)),
            _workdir: workdir,
        });

        // Readiness with backoff, up to 15 s.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut wait = Duration::from_millis(50);
        loop {
            if let Some(code) = me.guard.lock().as_mut().and_then(|g| g.try_wait()) {
                let tail = me
                    .guard
                    .lock()
                    .as_ref()
                    .map(|g| g.stderr.snapshot())
                    .unwrap_or_default();
                return Err(HarnessError::unavailable(format!(
                    "opencode serve exited with code {code}: {}",
                    tail.lines().last().unwrap_or("")
                )));
            }
            match me.get("/api/info").await {
                Ok(_) => break,
                Err(_) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(Duration::from_millis(500));
                }
                Err(e) => {
                    me.shutdown().await;
                    return Err(HarnessError::unavailable(format!(
                        "opencode serve not ready: {e}"
                    )));
                }
            }
        }
        Ok(me)
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{}", self.base, path))
            .basic_auth("opencode", Some(&self.password))
    }

    async fn get(&self, path: &str) -> Result<Value, HarnessError> {
        let r = self
            .req(reqwest::Method::GET, path)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| HarnessError::unavailable(format!("opencode GET {path}: {e}")))?;
        json_body(r, path).await
    }

    async fn post(
        &self,
        path: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<Value, HarnessError> {
        let r = self
            .req(reqwest::Method::POST, path)
            .timeout(timeout)
            .json(body)
            .send()
            .await
            .map_err(|e| HarnessError::unavailable(format!("opencode POST {path}: {e}")))?;
        json_body(r, path).await
    }

    async fn delete(&self, path: &str) {
        let _ = self
            .req(reqwest::Method::DELETE, path)
            .timeout(Duration::from_secs(10))
            .send()
            .await;
    }
}

async fn json_body(r: reqwest::Response, path: &str) -> Result<Value, HarnessError> {
    let status = r.status();
    let text = r.text().await.unwrap_or_default();
    if !status.is_success() {
        let msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v.pointer("/error/message")
                    .or_else(|| v.get("message"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .unwrap_or_else(|| text.chars().take(300).collect());
        let kind = if status.as_u16() == 401 {
            ErrorKind::Unavailable
        } else if status.as_u16() == 400 {
            ErrorKind::BadRequest
        } else {
            classify_message(&msg)
        };
        return Err(HarnessError::new(
            kind,
            format!("opencode {path} → {status}: {msg}"),
        ));
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text)
        .map_err(|e| HarnessError::failed(format!("opencode {path}: bad JSON: {e}")))
}

#[async_trait]
impl SharedWorker for ServeProcess {
    fn alive(&self) -> bool {
        if !self.alive.load(Ordering::SeqCst) {
            return false;
        }
        let exited = self
            .guard
            .lock()
            .as_mut()
            .and_then(|g| g.try_wait())
            .is_some();
        if exited {
            self.alive.store(false, Ordering::SeqCst);
        }
        !exited
    }

    async fn shutdown(&self) {
        self.alive.store(false, Ordering::SeqCst);
        let g = self.guard.lock().take();
        if let Some(mut g) = g {
            g.terminate(Duration::from_secs(2)).await;
        }
    }
}

/// Minimal SSE frame parser over a byte stream.
struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(8192),
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Vec<(Option<String>, String)> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            let Some(pos) = find_frame_end(&self.buf) else {
                break;
            };
            let frame: Vec<u8> = self.buf.drain(..pos.0).collect();
            self.buf.drain(..pos.1);
            let text = String::from_utf8_lossy(&frame);
            let mut event = None;
            let mut data = String::new();
            for line in text.lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    event = Some(v.trim().to_string());
                } else if let Some(v) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(v.strip_prefix(' ').unwrap_or(v));
                }
            }
            if !data.is_empty() {
                out.push((event, data));
            }
        }
        out
    }
}

fn find_frame_end(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if i + 3 < buf.len() && &buf[i..i + 4] == b"\r\n\r\n" {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

fn split_model(model: &str) -> Result<(String, String), HarnessError> {
    model
        .split_once('/')
        .map(|(p, m)| (p.to_string(), m.to_string()))
        .filter(|(p, m)| !p.is_empty() && !m.is_empty())
        .ok_or_else(|| {
            HarnessError::new(
                ErrorKind::BadRequest,
                format!("opencode models must be `provider/model`, got `{model}`"),
            )
        })
}

fn model_ref(turn: &Turn) -> Result<Value, HarnessError> {
    let (provider, id) = split_model(&turn.model)?;
    let mut m = json!({ "providerID": provider, "id": id });
    if let Some(e) = &turn.effort {
        m["variant"] = Value::String(e.clone());
    }
    Ok(m)
}

fn prompt_text(turn: &Turn) -> String {
    match &turn.system {
        Some(s) if !s.trim().is_empty() => format!("# System\n\n{s}\n\n# User\n\n{}", turn.input),
        _ => turn.input.clone(),
    }
}

fn tokens_to_usage(t: &Value) -> Usage {
    let g = |k: &str| t.get(k).and_then(Value::as_u64).unwrap_or(0);
    let cached = t
        .pointer("/cache/read")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        prompt_tokens: g("input") + cached,
        completion_tokens: g("output") + g("reasoning"),
        cached_prompt_tokens: cached,
        reasoning_tokens: g("reasoning"),
        estimated: false,
    }
}

pub struct OpencodeHarness {
    cfg: OpencodeConfig,
    limits: LimitsConfig,
    pool: Arc<SharedPool<ServeProcess>>,
}

impl OpencodeHarness {
    pub fn new(cfg: OpencodeConfig, limits: LimitsConfig) -> Self {
        let factory_cfg = cfg.clone();
        let pool = SharedPool::new(cfg.pool.clone(), move || {
            let cfg = factory_cfg.clone();
            Box::pin(async move { ServeProcess::start(&cfg).await }) as BoxFuture<'static, _>
        });
        Self { cfg, limits, pool }
    }

    pub async fn prewarm(&self) {
        self.pool.prewarm().await;
    }

    fn exe(&self) -> Result<PathBuf, HarnessError> {
        resolve_executable("opencode", self.cfg.executable.as_deref())
    }

    async fn run_oneshot(
        &self,
        turn: Turn,
        cancel: CancellationToken,
    ) -> Result<EventStream, HarnessError> {
        let lease = self.pool.acquire().await?;
        let body = json!({ "prompt": prompt_text(&turn), "model": model_ref(&turn)? });
        let timeout = self.limits.request_timeout;
        let prompt_chars = turn.system.as_deref().map_or(0, str::len) + turn.input.len();
        let scratch_keep = turn.scratch.clone();
        let s = stream! {
            let _scratch = scratch_keep;
            let lease = lease;
            let res = tokio::select! {
                _ = cancel.cancelled() => { yield Event::Done(FinishReason::Interrupted); return; }
                r = lease.post("/api/experimental/generate/text", &body, timeout) => r,
            };
            match res {
                Ok(v) => {
                    let text = v.pointer("/data/text").and_then(Value::as_str).unwrap_or("").to_string();
                    yield Event::Usage(Usage::estimate(prompt_chars, text.len()));
                    if !text.is_empty() { yield Event::TextDelta(text); }
                    yield Event::Done(FinishReason::Stop);
                }
                Err(e) => yield Event::Error(e),
            }
        };
        Ok(Box::pin(s))
    }

    async fn run_session(
        &self,
        turn: Turn,
        cancel: CancellationToken,
    ) -> Result<EventStream, HarnessError> {
        let lease = self.pool.acquire().await?;
        let server = lease.worker.clone();
        let model = model_ref(&turn)?;

        let session_id = match &turn.resume {
            Some(id) => id.clone(),
            None => {
                let res = server
                    .post(
                        "/api/session",
                        &json!({ "title": format!("locum {}", turn.request_id), "agent": AGENT }),
                        Duration::from_secs(30),
                    )
                    .await?;
                res.pointer("/data/id")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .ok_or_else(|| HarnessError::failed("opencode session create returned no id"))?
            }
        };

        // Subscribe before prompting so no event is missed.
        let events = server
            .req(reqwest::Method::GET, "/api/event")
            .header("accept", "text/event-stream")
            .send()
            .await
            .map_err(|e| HarnessError::unavailable(format!("opencode event stream: {e}")))?;
        if !events.status().is_success() {
            return Err(HarnessError::unavailable(format!(
                "opencode event stream → {}",
                events.status()
            )));
        }
        let mut bytes = events.bytes_stream();

        let text = prompt_text(&turn);
        let prompt_body = json!({ "text": text, "model": model, "agent": AGENT });
        let sid = session_id.clone();
        let server2 = server.clone();
        // Fire the prompt; the response only acknowledges enqueueing.
        let prompt_task = tokio::spawn(async move {
            server2
                .post(
                    &format!("/api/session/{sid}/prompt"),
                    &prompt_body,
                    Duration::from_secs(60),
                )
                .await
        });

        let want_reasoning = turn.want_reasoning;
        let keep = turn.resume.is_some();
        let sid = session_id.clone();
        let scratch_keep = turn.scratch.clone();
        let s = stream! {
            let _scratch = scratch_keep;
            let _lease = lease;
            let server = server;
            let mut prompt_task = Some(prompt_task);
            yield Event::Continuation(sid.clone());
            let mut parser = SseParser::new();
            let mut rec = Reconciler::default();
            let mut usage: Option<Usage> = None;
            let mut finished = false;
            loop {
                let chunk = tokio::select! {
                    _ = cancel.cancelled() => {
                        let _ = server.post(&format!("/api/session/{sid}/interrupt"), &Value::Null, Duration::from_secs(5)).await;
                        if !keep { server.delete(&format!("/api/session/{sid}")).await; }
                        yield Event::Done(FinishReason::Interrupted);
                        return;
                    }
                    r = async { prompt_task.as_mut().unwrap().await }, if prompt_task.is_some() => {
                        prompt_task = None;
                        match r {
                            Ok(Ok(_)) => {}
                            Ok(Err(e)) => { if !keep { server.delete(&format!("/api/session/{sid}")).await; } yield Event::Error(e); return; }
                            Err(e) => { yield Event::Error(HarnessError::failed(format!("prompt task: {e}"))); return; }
                        }
                        continue;
                    }
                    c = bytes.next() => c,
                };
                let chunk = match chunk {
                    Some(Ok(c)) => c,
                    Some(Err(e)) => { yield Event::Error(HarnessError::unavailable(format!("opencode event stream: {e}"))); return; }
                    None => { yield Event::Error(HarnessError::unavailable("opencode event stream closed")); return; }
                };
                for (_evt, data) in parser.push(&chunk) {
                    let Ok(v) = serde_json::from_str::<Value>(&data) else { continue };
                    let d = &v["data"];
                    if d.get("sessionID").and_then(Value::as_str) != Some(sid.as_str()) { continue; }
                    match v.get("type").and_then(Value::as_str).unwrap_or("") {
                        "session.text.delta" => {
                            if let Some(t) = d.get("delta").and_then(Value::as_str) {
                                rec.delta(t);
                                yield Event::TextDelta(t.to_string());
                            }
                        }
                        "session.reasoning.delta" if want_reasoning => {
                            if let Some(t) = d.get("delta").and_then(Value::as_str) {
                                yield Event::ReasoningDelta(t.to_string());
                            }
                        }
                        "session.step.ended" => {
                            if let Some(t) = d.get("tokens") { usage = Some(tokens_to_usage(t)); }
                        }
                        "session.usage.updated" => {
                            if let Some(t) = d.get("tokens") { usage = Some(tokens_to_usage(t)); }
                        }
                        "session.permission.requested" | "session.permission.asked" => {
                            let rid = d.get("requestID").or_else(|| d.get("id")).and_then(Value::as_str).unwrap_or("");
                            tracing::warn!(request = rid, "opencode asked for permission during a plain chat turn; rejecting");
                            if !rid.is_empty() {
                                let _ = server.post(&format!("/api/session/{sid}/permission/{rid}/reply"), &json!({ "decision": "reject" }), Duration::from_secs(5)).await;
                            }
                        }
                        "session.tool.called" | "session.tool.started" => {
                            tracing::warn!("opencode attempted a tool call during a plain chat turn");
                        }
                        "session.execution.succeeded" => { finished = true; }
                        "session.execution.failed" | "session.step.failed" | "session.error" => {
                            let msg = d.pointer("/error/data/message")
                                .or_else(|| d.pointer("/error/message"))
                                .or_else(|| d.get("message"))
                                .and_then(Value::as_str)
                                .map(String::from)
                                .unwrap_or_else(|| d.get("error").map(|e| e.to_string()).unwrap_or_else(|| "opencode execution failed".into()));
                            if !keep { server.delete(&format!("/api/session/{sid}")).await; }
                            if let Some(u) = usage.take() { yield Event::Usage(u); }
                            yield Event::Error(HarnessError::new(classify_message(&msg), msg));
                            return;
                        }
                        "session.idle" => {
                            let outcome = d.get("outcome").and_then(Value::as_str).unwrap_or("succeeded");
                            if outcome == "failed" {
                                if !keep { server.delete(&format!("/api/session/{sid}")).await; }
                                yield Event::Error(HarnessError::failed("opencode session ended with a failure"));
                                return;
                            }
                            finished = true;
                        }
                        _ => {}
                    }
                    if finished {
                        if !keep { server.delete(&format!("/api/session/{sid}")).await; }
                        if let Some(u) = usage.take() { yield Event::Usage(u); }
                        yield Event::Done(FinishReason::Stop);
                        return;
                    }
                }
            }
        };
        Ok(Box::pin(s))
    }
}

#[async_trait]
impl Harness for OpencodeHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Opencode
    }

    async fn probe(&self) -> Result<HarnessInfo, HarnessError> {
        let exe = self.exe()?;
        let out = tokio::time::timeout(
            Duration::from_secs(20),
            CommandBuilder::new(&exe)
                .arg("--version")
                .no_stdin()
                .build()
                .output(),
        )
        .await
        .map_err(|_| HarnessError::unavailable("opencode --version timed out"))?
        .map_err(|e| HarnessError::unavailable(format!("opencode --version: {e}")))?;
        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let mut info = HarnessInfo {
            executable: Some(exe),
            version: Some(version),
            authenticated: None,
            default_model: self.cfg.default_model.clone(),
            detail: None,
        };
        match self.pool.acquire().await {
            Ok(lease) => {
                let ok = lease
                    .get("/api/model")
                    .await
                    .map(|v| v["data"].as_array().map_or(0, Vec::len));
                match ok {
                    Ok(n) => {
                        info.authenticated = Some(n > 0);
                        info.detail = Some(format!("serve ok; {n} models visible"));
                    }
                    Err(e) => {
                        info.authenticated = Some(false);
                        info.detail = Some(format!("serve ok but model list failed: {e}"));
                    }
                }
            }
            Err(e) => {
                info.authenticated = Some(false);
                info.detail = Some(format!("serve failed to start: {e}"));
            }
        }
        Ok(info)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, HarnessError> {
        let lease = self.pool.acquire().await?;
        let v = lease.get("/api/model").await?;
        let mut out = Vec::new();
        if let Some(arr) = v["data"].as_array() {
            for m in arr {
                let (Some(p), Some(id)) = (
                    m.get("providerID").and_then(Value::as_str),
                    m.get("id").and_then(Value::as_str),
                ) else {
                    continue;
                };
                out.push(ModelInfo {
                    id: format!("{p}/{id}"),
                    default_effort: None,
                    efforts: m
                        .get("variants")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| {
                                    x.get("id").and_then(Value::as_str).map(String::from)
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                });
            }
        }
        Ok(out)
    }

    async fn run(
        &self,
        turn: Turn,
        cancel: CancellationToken,
    ) -> Result<EventStream, HarnessError> {
        if self.cfg.oneshot && !turn.stream_hint && turn.resume.is_none() && !turn.want_reasoning {
            self.run_oneshot(turn, cancel).await
        } else {
            self.run_session(turn, cancel).await
        }
    }

    async fn prewarm(&self) {
        OpencodeHarness::prewarm(self).await;
    }

    async fn forget(&self, continuation: &str) {
        if !self.pool.is_empty() {
            if let Ok(lease) = self.pool.acquire().await {
                lease.delete(&format!("/api/session/{continuation}")).await;
            }
        }
    }

    async fn shutdown(&self) {
        self.pool.shutdown().await;
    }
}
