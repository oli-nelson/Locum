//! Codex CLI adapter: `codex app-server` (JSON-RPC over stdio, token deltas) with `codex exec` fallback.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use async_trait::async_trait;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::util::{classify_message, Reconciler};
use super::{
    ErrorKind, Event, EventStream, FinishReason, Harness, HarnessError, HarnessInfo, HarnessKind,
    ModelInfo, Turn, Usage,
};
use crate::config::{CodexConfig, CodexMode, LimitsConfig};
use crate::process::pool::{SharedPool, SharedWorker};
use crate::process::{resolve_executable, ChildGuard, CommandBuilder, LineReader};

const NOISY_NOTIFICATIONS: &[&str] = &[
    "turn/diff/updated",
    "turn/plan/updated",
    "item/commandExecution/outputDelta",
    "item/fileChange/outputDelta",
    "item/plan/delta",
    "thread/readState/changed",
    "account/rateLimits/updated",
    "fs/changed",
];

// ---------------------------------------------------------------------------
// JSON-RPC app-server process
// ---------------------------------------------------------------------------

pub struct AppServer {
    guard: Mutex<Option<ChildGuard>>,
    stdin: tokio::sync::Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, HarnessError>>>>,
    subs: Mutex<HashMap<String, mpsc::Sender<Value>>>,
    alive: Arc<AtomicBool>,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl AppServer {
    async fn start(cfg: &CodexConfig, limits: &LimitsConfig) -> Result<Arc<Self>, HarnessError> {
        let exe = resolve_executable("codex", cfg.executable.as_deref())?;
        let mut cmd = CommandBuilder::new(&exe)
            .args([
                "app-server",
                "-c",
                r#"sandbox_mode="read-only""#,
                "-c",
                r#"approval_policy="never""#,
            ])
            .strip_provider_keys(cfg.strip_provider_keys)
            .cwd(std::env::temp_dir());
        if !cfg.reasoning_summary {
            cmd = cmd.args(["-c", r#"model_reasoning_summary="none""#]);
        }
        if cfg.isolate_home {
            cmd = cmd.env("CODEX_HOME", isolated_codex_home()?);
        }
        tracing::debug!(argv = %cmd.argv_for_log(), "spawning codex app-server");
        let child = cmd.build().spawn().map_err(|e| {
            HarnessError::unavailable(format!("failed to spawn codex app-server: {e}"))
        })?;
        let mut guard = ChildGuard::new(child, "codex", 8192);
        let stdin = guard
            .take_stdin()
            .ok_or_else(|| HarnessError::failed("no stdin"))?;
        let stdout = guard
            .take_stdout()
            .ok_or_else(|| HarnessError::failed("no stdout"))?;

        let server = Arc::new(Self {
            guard: Mutex::new(Some(guard)),
            stdin: tokio::sync::Mutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            subs: Mutex::new(HashMap::new()),
            alive: Arc::new(AtomicBool::new(true)),
            reader: Mutex::new(None),
        });

        let weak = Arc::downgrade(&server);
        let alive = server.alive.clone();
        let max_line = limits.max_line_bytes;
        let handle = tokio::spawn(async move {
            let mut lines = LineReader::new(stdout, max_line);
            loop {
                let line = match lines.next_line().await {
                    Ok(Some(l)) => l,
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "codex app-server read error");
                        break;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                let Some(server) = weak.upgrade() else { break };
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                server.dispatch(v);
            }
            alive.store(false, Ordering::SeqCst);
            if let Some(server) = weak.upgrade() {
                server.fail_all("codex app-server exited");
            }
        });
        *server.reader.lock() = Some(handle);

        let init = server
            .request(
                "initialize",
                json!({
                    "clientInfo": { "name": "locum", "title": "locum", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": { "optOutNotificationMethods": NOISY_NOTIFICATIONS }
                }),
                Duration::from_secs(30),
            )
            .await?;
        tracing::debug!(user_agent = ?init.get("userAgent"), "codex app-server initialised");
        server.notify("initialized", json!({})).await?;
        Ok(server)
    }

    fn dispatch(self: &Arc<Self>, v: Value) {
        let id = v.get("id").and_then(Value::as_i64);
        let method = v.get("method").and_then(Value::as_str).map(String::from);
        match (id, method) {
            (Some(id), None) => {
                if let Some(tx) = self.pending.lock().remove(&id) {
                    let res = if let Some(err) = v.get("error") {
                        let msg = err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("rpc error")
                            .to_string();
                        Err(HarnessError::new(classify_message(&msg), msg))
                    } else {
                        Ok(v.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = tx.send(res);
                }
            }
            (Some(id), Some(method)) => {
                // Server → client request (approvals, user input). Decline everything.
                let response = match method.as_str() {
                    "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                        json!({ "id": id, "result": { "decision": "decline" } })
                    }
                    "execCommandApproval" | "applyPatchApproval" => {
                        json!({ "id": id, "result": { "decision": "denied" } })
                    }
                    "item/tool/requestUserInput" => {
                        json!({ "id": id, "result": { "answers": {} } })
                    }
                    "item/tool/call" => {
                        json!({ "id": id, "result": { "success": false, "contentItems": [] } })
                    }
                    _ => {
                        json!({ "id": id, "error": { "code": -32601, "message": "locum does not support this request" } })
                    }
                };
                tracing::warn!(method, "codex requested approval/input; declined");
                let me = self.clone();
                tokio::spawn(async move {
                    let _ = me.write_raw(response).await;
                });
            }
            (None, Some(method)) => {
                let params = v.get("params").cloned().unwrap_or(Value::Null);
                let thread_id = params.get("threadId").and_then(Value::as_str);
                if let Some(tid) = thread_id {
                    let sub = self.subs.lock().get(tid).cloned();
                    if let Some(tx) = sub {
                        match tx.try_send(json!({ "method": method, "params": params })) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                tracing::warn!(
                                    tid,
                                    "codex notification channel full; dropping event"
                                );
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                // The turn's stream is gone; stop routing to it.
                                let mut subs = self.subs.lock();
                                if subs.get(tid).is_some_and(|t| t.same_channel(&tx)) {
                                    subs.remove(tid);
                                }
                                tracing::debug!(
                                    tid,
                                    "codex notification for a finished turn; unsubscribed"
                                );
                            }
                        }
                    }
                } else if method == "error" {
                    tracing::warn!(params = %params, "codex app-server global error");
                }
            }
            _ => {}
        }
    }

    fn fail_all(&self, msg: &str) {
        for (_, tx) in self.pending.lock().drain() {
            let _ = tx.send(Err(HarnessError::unavailable(msg)));
        }
        self.subs.lock().clear();
    }

    async fn write_raw(&self, v: Value) -> Result<(), HarnessError> {
        let mut line = serde_json::to_vec(&v)?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(&line)
            .await
            .map_err(|e| HarnessError::unavailable(format!("codex stdin: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| HarnessError::unavailable(format!("codex stdin: {e}")))?;
        Ok(())
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), HarnessError> {
        self.write_raw(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, HarnessError> {
        if !self.alive() {
            return Err(HarnessError::unavailable("codex app-server is not running"));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        self.write_raw(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(HarnessError::unavailable(
                "codex app-server dropped the request",
            )),
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(HarnessError::timeout(format!("codex `{method}` timed out")))
            }
        }
    }

    fn subscribe(&self, thread_id: &str) -> mpsc::Receiver<Value> {
        let (tx, rx) = mpsc::channel(4096);
        self.subs.lock().insert(thread_id.to_string(), tx);
        rx
    }

    fn unsubscribe(&self, thread_id: &str) {
        self.subs.lock().remove(thread_id);
    }
}

#[async_trait]
impl SharedWorker for AppServer {
    fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    async fn shutdown(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Some(h) = self.reader.lock().take() {
            h.abort();
        }
        let guard = self.guard.lock().take();
        if let Some(mut g) = guard {
            g.terminate(Duration::from_secs(2)).await;
        }
        self.fail_all("codex app-server shut down");
    }
}

fn isolated_codex_home() -> Result<PathBuf, HarnessError> {
    let real_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".codex")))
        .ok_or_else(|| HarnessError::unavailable("cannot determine CODEX_HOME"))?;
    let dir = crate::config::data_dir().join("codex-home");
    std::fs::create_dir_all(&dir)
        .map_err(|e| HarnessError::unavailable(format!("create {}: {e}", dir.display())))?;
    #[cfg(unix)]
    {
        let link = dir.join("auth.json");
        let target = real_home.join("auth.json");
        if !link.exists() && target.exists() {
            std::os::unix::fs::symlink(&target, &link)
                .map_err(|e| HarnessError::unavailable(format!("link auth.json: {e}")))?;
        }
    }
    #[cfg(not(unix))]
    let _ = real_home;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Interrupts and unsubscribes an app-server turn whose stream is dropped before the
/// turn finishes, so codex stops generating and its notifications stop being routed.
struct ActiveTurn {
    server: Arc<AppServer>,
    thread_id: String,
    turn_id: String,
    finished: bool,
}

impl Drop for ActiveTurn {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.server.unsubscribe(&self.thread_id);
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let server = self.server.clone();
        let params = json!({ "threadId": self.thread_id, "turnId": self.turn_id });
        rt.spawn(async move {
            let _ = server
                .request("turn/interrupt", params, Duration::from_secs(5))
                .await;
        });
    }
}

pub struct CodexHarness {
    cfg: CodexConfig,
    limits: LimitsConfig,
    pool: Arc<SharedPool<AppServer>>,
    app_server_broken: AtomicBool,
}

impl CodexHarness {
    pub fn new(cfg: CodexConfig, limits: LimitsConfig) -> Self {
        let factory_cfg = cfg.clone();
        let factory_limits = limits.clone();
        let pool = SharedPool::new(cfg.pool.clone(), move || {
            let cfg = factory_cfg.clone();
            let limits = factory_limits.clone();
            Box::pin(async move { AppServer::start(&cfg, &limits).await }) as BoxFuture<'static, _>
        });
        Self {
            cfg,
            limits,
            pool,
            app_server_broken: AtomicBool::new(false),
        }
    }

    pub async fn prewarm(&self) {
        if self.cfg.mode == CodexMode::AppServer {
            self.pool.prewarm().await;
        }
    }

    fn use_app_server(&self) -> bool {
        self.cfg.mode == CodexMode::AppServer && !self.app_server_broken.load(Ordering::SeqCst)
    }

    fn exe(&self) -> Result<PathBuf, HarnessError> {
        resolve_executable("codex", self.cfg.executable.as_deref())
    }

    async fn run_app_server(
        &self,
        turn: Turn,
        cancel: CancellationToken,
    ) -> Result<EventStream, HarnessError> {
        let lease = match self.pool.acquire().await {
            Ok(l) => l,
            Err(e) if e.kind == ErrorKind::Unavailable && self.cfg.mode == CodexMode::AppServer => {
                tracing::warn!(error = %e, "codex app-server failed to start; falling back to `codex exec`");
                self.app_server_broken.store(true, Ordering::SeqCst);
                return self.run_exec(turn, cancel).await;
            }
            Err(e) => return Err(e),
        };
        let server = lease.worker.clone();
        let scratch = turn.scratch.path().to_string_lossy().into_owned();

        let thread_id = match &turn.resume {
            Some(tid) => {
                server
                    .request(
                        "thread/resume",
                        json!({ "threadId": tid }),
                        Duration::from_secs(30),
                    )
                    .await?;
                tid.clone()
            }
            None => {
                let mut params = json!({
                    "model": turn.model,
                    "cwd": scratch,
                    "approvalPolicy": "never",
                    "sandbox": "read-only",
                    "ephemeral": true,
                    "serviceName": "locum",
                });
                if let Some(system) = &turn.system {
                    params["developerInstructions"] = Value::String(system.clone());
                }
                let res = server
                    .request("thread/start", params, Duration::from_secs(60))
                    .await?;
                res.pointer("/thread/id")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .ok_or_else(|| HarnessError::failed("thread/start returned no thread id"))?
            }
        };

        let mut rx = server.subscribe(&thread_id);

        let mut input = vec![json!({ "type": "text", "text": turn.input })];
        for img in &turn.images {
            if let Some(p) = &img.path {
                input.push(json!({ "type": "localImage", "path": p.to_string_lossy(), "detail": img.detail }));
            } else if let Some(u) = &img.url {
                input.push(json!({ "type": "image", "url": u, "detail": img.detail }));
            }
        }
        let mut params = json!({ "threadId": thread_id, "input": input, "model": turn.model });
        if let Some(e) = &turn.effort {
            params["effort"] = Value::String(e.clone());
        }
        if let Some(schema) = &turn.response_schema {
            params["outputSchema"] = schema.clone();
        }
        let res = match server
            .request("turn/start", params, Duration::from_secs(60))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                server.unsubscribe(&thread_id);
                return Err(e);
            }
        };
        let turn_id = res
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_default();

        let want_reasoning = turn.want_reasoning;
        let tid = thread_id.clone();
        let scratch_keep = turn.scratch.clone();
        let s = stream! {
            let _scratch = scratch_keep;
            let _lease = lease;
            let mut active = ActiveTurn { server: server.clone(), thread_id: tid.clone(), turn_id: turn_id.clone(), finished: false };
            let server = server;
            yield Event::Continuation(tid.clone());
            let mut rec = Reconciler::default();
            let mut usage: Option<Usage> = None;
            let mut agent_item_id: Option<String> = None;
            loop {
                let msg = tokio::select! {
                    _ = cancel.cancelled() => {
                        active.finished = true;
                        let _ = server.request("turn/interrupt", json!({ "threadId": tid, "turnId": turn_id }), Duration::from_secs(5)).await;
                        server.unsubscribe(&tid);
                        yield Event::Done(FinishReason::Interrupted);
                        return;
                    }
                    m = rx.recv() => m,
                };
                let Some(msg) = msg else {
                    active.finished = true;
                    server.unsubscribe(&tid);
                    yield Event::Error(HarnessError::unavailable("codex app-server connection closed mid-turn"));
                    return;
                };
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                let p = &msg["params"];
                if !turn_id.is_empty() {
                    if let Some(t) = p.get("turnId").and_then(Value::as_str) {
                        if t != turn_id { continue; }
                    }
                }
                match method {
                    "item/agentMessage/delta" => {
                        if let Some(d) = p.get("delta").and_then(Value::as_str) {
                            let item = p.get("itemId").and_then(Value::as_str).map(String::from);
                            if agent_item_id.is_none() { agent_item_id = item.clone(); }
                            if agent_item_id == item || item.is_none() {
                                rec.delta(d);
                                yield Event::TextDelta(d.to_string());
                            } else {
                                // A later agent message (e.g. after a tool attempt): treat as continuation.
                                agent_item_id = item;
                                let sep = "\n\n";
                                rec.delta(sep); rec.delta(d);
                                yield Event::TextDelta(format!("{sep}{d}"));
                            }
                        }
                    }
                    "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" if want_reasoning => {
                        if let Some(d) = p.get("delta").and_then(Value::as_str) {
                            yield Event::ReasoningDelta(d.to_string());
                        }
                    }
                    "item/started" => {
                        let t = p.pointer("/item/type").and_then(Value::as_str).unwrap_or("");
                        if matches!(t, "commandExecution" | "fileChange" | "mcpToolCall" | "webSearch" | "collabAgentToolCall") {
                            tracing::warn!(item_type = t, "codex attempted a tool action during a plain chat turn");
                        }
                    }
                    "item/completed" => {
                        let item = &p["item"];
                        if item.get("type").and_then(Value::as_str) == Some("agentMessage") {
                            if let Some(text) = item.get("text").and_then(Value::as_str) {
                                let this_id = item.get("id").and_then(Value::as_str).map(String::from);
                                if agent_item_id.is_none() || agent_item_id == this_id {
                                    for e in rec.finalize(text, "codex") { yield e; }
                                }
                            }
                        }
                    }
                    "thread/tokenUsage/updated" => {
                        if let Some(u) = p.pointer("/tokenUsage/last") {
                            let g = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                            usage = Some(Usage {
                                prompt_tokens: g("inputTokens"),
                                completion_tokens: g("outputTokens"),
                                cached_prompt_tokens: g("cachedInputTokens"),
                                reasoning_tokens: g("reasoningOutputTokens"),
                                estimated: false,
                            });
                        }
                    }
                    "turn/completed" => {
                        let status = p.pointer("/turn/status").and_then(Value::as_str).unwrap_or("completed");
                        active.finished = true;
                        server.unsubscribe(&tid);
                        if let Some(u) = usage.take() { yield Event::Usage(u); }
                        match status {
                            "failed" => {
                                let err = &p["turn"]["error"];
                                let msg = err.get("message").and_then(Value::as_str).unwrap_or("codex turn failed").to_string();
                                let info = err.get("codexErrorInfo").map(|v| v.to_string()).unwrap_or_default().to_ascii_lowercase();
                                let kind = if info.contains("contextwindowexceeded") { ErrorKind::ContextLength }
                                    else if info.contains("ratelimit") || info.contains("usagelimit") || info.contains("overloaded") { ErrorKind::RateLimited }
                                    else if info.contains("unauthorized") { ErrorKind::Unavailable }
                                    else if info.contains("badrequest") || info.contains("modelnotfound") { ErrorKind::BadRequest }
                                    else { classify_message(&msg) };
                                yield Event::Error(HarnessError::new(kind, msg));
                            }
                            "interrupted" => yield Event::Done(FinishReason::Interrupted),
                            _ => yield Event::Done(FinishReason::Stop),
                        }
                        return;
                    }
                    "error" => {
                        let msg = p.get("message").and_then(Value::as_str)
                            .or_else(|| p.pointer("/error/message").and_then(Value::as_str))
                            .unwrap_or("codex error").to_string();
                        let will_retry = p.get("willRetry").and_then(Value::as_bool).unwrap_or(false);
                        if will_retry { tracing::debug!(msg, "codex transient error (retrying)"); continue; }
                        active.finished = true;
                        server.unsubscribe(&tid);
                        yield Event::Error(HarnessError::new(classify_message(&msg), msg));
                        return;
                    }
                    _ => {}
                }
            }
        };
        Ok(Box::pin(s))
    }

    async fn run_exec(
        &self,
        turn: Turn,
        cancel: CancellationToken,
    ) -> Result<EventStream, HarnessError> {
        let exe = self.exe()?;
        let scratch = turn.scratch.path().to_path_buf();
        let mut cmd = CommandBuilder::new(&exe)
            .args([
                "exec",
                "--json",
                "--ephemeral",
                "--ignore-user-config",
                "--ignore-rules",
                "--skip-git-repo-check",
                "--sandbox",
                "read-only",
                "--color",
                "never",
                "-m",
            ])
            .arg(&turn.model)
            .strip_provider_keys(self.cfg.strip_provider_keys)
            .cwd(&scratch);
        if let Some(e) = &turn.effort {
            cmd = cmd
                .arg("-c")
                .arg(format!("model_reasoning_effort={}", toml_str(e)));
        }
        if let Some(schema) = &turn.response_schema {
            let path = scratch.join("output-schema.json");
            tokio::fs::write(&path, serde_json::to_vec(schema)?).await?;
            cmd = cmd.arg("--output-schema").arg(path);
        }
        for img in &turn.images {
            if let Some(p) = &img.path {
                cmd = cmd.arg("-i").arg(p);
            }
        }
        cmd = cmd.arg("-C").arg(&scratch).arg("-");

        let mut prompt = String::new();
        if let Some(system) = &turn.system {
            prompt.push_str("# System\n\n");
            prompt.push_str(system);
            prompt.push_str("\n\n# User\n\n");
        }
        prompt.push_str(&turn.input);

        tracing::debug!(argv = %cmd.argv_for_log(), "spawning codex exec");
        let child = cmd
            .build()
            .spawn()
            .map_err(|e| HarnessError::unavailable(format!("failed to spawn codex: {e}")))?;
        let mut guard = ChildGuard::new(child, "codex", 8192);
        let mut stdin = guard
            .take_stdin()
            .ok_or_else(|| HarnessError::failed("no stdin"))?;
        let stdout = guard
            .take_stdout()
            .ok_or_else(|| HarnessError::failed("no stdout"))?;
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });

        let max_line = self.limits.max_line_bytes;
        let scratch_keep = turn.scratch.clone();
        let s = stream! {
            let _scratch = scratch_keep;
            let mut guard = guard;
            let mut lines = LineReader::new(stdout, max_line);
            let mut rec = Reconciler::default();
            let mut usage: Option<Usage> = None;
            let mut done = false;
            loop {
                let line = tokio::select! {
                    _ = cancel.cancelled() => { guard.kill_now(); yield Event::Done(FinishReason::Interrupted); return; }
                    l = lines.next_line() => l,
                };
                let line = match line { Ok(Some(l)) => l, Ok(None) => break, Err(e) => { yield Event::Error(e); return; } };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                match v.get("type").and_then(Value::as_str) {
                    Some("thread.started") => {
                        if let Some(t) = v.get("thread_id").and_then(Value::as_str) { yield Event::Continuation(t.to_string()); }
                    }
                    Some("item.completed") => {
                        let item = &v["item"];
                        if item.get("type").and_then(Value::as_str) == Some("agent_message") {
                            if let Some(t) = item.get("text").and_then(Value::as_str) {
                                let piece = if rec.has_text() { format!("\n\n{t}") } else { t.to_string() };
                                rec.delta(&piece);
                                yield Event::TextDelta(piece);
                            }
                        }
                    }
                    Some("turn.completed") => {
                        if let Some(u) = v.get("usage") {
                            let g = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                            usage = Some(Usage { prompt_tokens: g("input_tokens"), completion_tokens: g("output_tokens"), cached_prompt_tokens: g("cached_input_tokens"), reasoning_tokens: g("reasoning_output_tokens"), estimated: false });
                        }
                        done = true;
                    }
                    Some("turn.failed") | Some("error") => {
                        let msg = v.get("message").and_then(Value::as_str)
                            .or_else(|| v.pointer("/error/message").and_then(Value::as_str))
                            .unwrap_or("codex exec failed").to_string();
                        yield Event::Error(HarnessError::new(classify_message(&msg), msg));
                        return;
                    }
                    _ => {}
                }
            }
            let code = guard.wait_timeout(Duration::from_secs(5)).await;
            if let Some(u) = usage.take() { yield Event::Usage(u); }
            if done || rec.has_text() {
                yield Event::Done(FinishReason::Stop);
            } else {
                let tail = guard.stderr.snapshot();
                let msg = tail.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("codex exec produced no output").to_string();
                tracing::warn!(exit = ?code, "codex exec failed: {msg}");
                yield Event::Error(HarnessError::new(classify_message(&msg), msg));
            }
        };
        Ok(Box::pin(s))
    }
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[async_trait]
impl Harness for CodexHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Codex
    }

    async fn probe(&self) -> Result<HarnessInfo, HarnessError> {
        let exe = self.exe()?;
        let out = tokio::time::timeout(
            Duration::from_secs(15),
            CommandBuilder::new(&exe)
                .arg("--version")
                .no_stdin()
                .build()
                .output(),
        )
        .await
        .map_err(|_| HarnessError::unavailable("codex --version timed out"))?
        .map_err(|e| HarnessError::unavailable(format!("codex --version: {e}")))?;
        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let mut info = HarnessInfo {
            executable: Some(exe),
            version: Some(version),
            authenticated: None,
            default_model: self.cfg.default_model.clone(),
            detail: Some(format!("mode: {:?}", self.cfg.mode)),
        };
        if self.use_app_server() {
            match self.pool.acquire().await {
                Ok(lease) => {
                    let acct = lease
                        .request(
                            "account/read",
                            json!({ "refreshToken": false }),
                            Duration::from_secs(15),
                        )
                        .await;
                    info.authenticated = Some(
                        matches!(&acct, Ok(v) if !v.get("account").is_none_or(Value::is_null)),
                    );
                    if let Ok(v) = &acct {
                        if let Some(t) = v.pointer("/account/type").and_then(Value::as_str) {
                            info.detail = Some(format!("app-server ok; account: {t}"));
                        }
                    }
                }
                Err(e) => {
                    info.detail = Some(format!("app-server failed to start: {e}"));
                    info.authenticated = Some(false);
                }
            }
        }
        Ok(info)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, HarnessError> {
        if !self.use_app_server() {
            return Ok(vec![ModelInfo {
                id: self.cfg.default_model.clone(),
                default_effort: self.cfg.default_effort.clone(),
                efforts: vec![],
            }]);
        }
        let lease = self.pool.acquire().await?;
        let res = lease
            .request(
                "model/list",
                json!({ "includeHidden": false, "limit": 200 }),
                Duration::from_secs(30),
            )
            .await?;
        let mut out = Vec::new();
        if let Some(arr) = res.get("data").and_then(Value::as_array) {
            for m in arr {
                let Some(id) = m.get("id").and_then(Value::as_str) else {
                    continue;
                };
                out.push(ModelInfo {
                    id: id.to_string(),
                    default_effort: m
                        .get("defaultReasoningEffort")
                        .and_then(Value::as_str)
                        .map(String::from),
                    efforts: m
                        .get("supportedReasoningEfforts")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|e| {
                                    e.as_str().map(String::from).or_else(|| {
                                        e.get("reasoningEffort")
                                            .and_then(Value::as_str)
                                            .map(String::from)
                                    })
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
        if self.use_app_server() {
            self.run_app_server(turn, cancel).await
        } else {
            self.run_exec(turn, cancel).await
        }
    }

    async fn prewarm(&self) {
        CodexHarness::prewarm(self).await;
    }

    async fn forget(&self, continuation: &str) {
        if self.use_app_server() && !self.pool.is_empty() {
            if let Ok(lease) = self.pool.acquire().await {
                let _ = lease
                    .request(
                        "thread/delete",
                        json!({ "threadId": continuation }),
                        Duration::from_secs(10),
                    )
                    .await;
            }
        }
    }

    async fn shutdown(&self) {
        self.pool.shutdown().await;
    }
}
