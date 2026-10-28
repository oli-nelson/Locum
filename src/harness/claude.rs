//! Claude Code adapter: `claude --print --output-format stream-json`.

use std::path::PathBuf;
use std::sync::Arc;

use async_stream::stream;
use async_trait::async_trait;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::util::{classify_message, Reconciler};
use super::{
    ErrorKind, Event, EventStream, FinishReason, Harness, HarnessError, HarnessInfo, HarnessKind,
    ModelInfo, Turn, Usage,
};
use crate::config::{ClaudeConfig, LimitsConfig};
use crate::process::{resolve_executable, ChildGuard, CommandBuilder, LineReader};

pub struct ClaudeHarness {
    cfg: ClaudeConfig,
    limits: LimitsConfig,
    slots: Arc<Semaphore>,
}

impl ClaudeHarness {
    pub fn new(cfg: ClaudeConfig, limits: LimitsConfig) -> Self {
        let slots = Arc::new(Semaphore::new(cfg.pool.max_workers.max(1)));
        Self { cfg, limits, slots }
    }

    fn exe(&self) -> Result<PathBuf, HarnessError> {
        resolve_executable("claude", self.cfg.executable.as_deref())
    }

    fn base_command(&self, exe: &std::path::Path, cwd: &std::path::Path) -> CommandBuilder {
        CommandBuilder::new(exe)
            .args([
                "--print",
                "--verbose",
                "--output-format",
                "stream-json",
                "--include-partial-messages",
                "--input-format",
                "text",
                "--safe-mode",
                "--tools",
                "",
                "--strict-mcp-config",
                "--mcp-config",
                r#"{"mcpServers":{}}"#,
                "--disable-slash-commands",
                "--permission-prompts",
                "none",
                "--setting-sources",
                "",
            ])
            .env_remove("CLAUDECODE")
            .env_remove("CLAUDE_CODE_ENTRYPOINT")
            .strip_provider_keys(self.cfg.strip_provider_keys)
            .cwd(cwd)
    }
}

fn parse_usage(u: &Value) -> Usage {
    let g = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let cached = g("cache_read_input_tokens");
    Usage {
        prompt_tokens: g("input_tokens") + cached + g("cache_creation_input_tokens"),
        completion_tokens: g("output_tokens"),
        cached_prompt_tokens: cached,
        reasoning_tokens: u
            .pointer("/output_tokens_details/thinking_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        estimated: false,
    }
}

fn message_text(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

#[async_trait]
impl Harness for ClaudeHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Claude
    }

    async fn probe(&self) -> Result<HarnessInfo, HarnessError> {
        let exe = self.exe()?;
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            CommandBuilder::new(&exe)
                .arg("--version")
                .no_stdin()
                .build()
                .output(),
        )
        .await
        .map_err(|_| HarnessError::unavailable("claude --version timed out"))?
        .map_err(|e| HarnessError::unavailable(format!("claude --version: {e}")))?;
        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(HarnessInfo {
            executable: Some(exe),
            version: Some(version),
            authenticated: None,
            default_model: self.cfg.default_model.clone(),
            detail: Some("auth is only verified by a live canary (`locum doctor --live`)".into()),
        })
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, HarnessError> {
        Ok(self
            .cfg
            .models
            .iter()
            .map(|m| ModelInfo {
                id: m.clone(),
                default_effort: None,
                efforts: ["low", "medium", "high", "xhigh", "max"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            })
            .collect())
    }

    async fn run(
        &self,
        turn: Turn,
        cancel: CancellationToken,
    ) -> Result<EventStream, HarnessError> {
        let permit = tokio::time::timeout(
            self.limits.queue_timeout,
            self.slots.clone().acquire_owned(),
        )
        .await
        .map_err(|_| HarnessError::new(ErrorKind::RateLimited, "claude pool saturated"))?
        .map_err(|_| HarnessError::failed("claude pool closed"))?;

        let exe = self.exe()?;
        let scratch = turn.scratch.path().to_path_buf();
        let mut cmd = self.base_command(&exe, &scratch);

        if let Some(system) = &turn.system {
            let path = scratch.join("system-prompt.txt");
            tokio::fs::write(&path, system).await?;
            cmd = cmd.arg("--system-prompt-file").arg(path);
        }
        cmd = cmd.arg("--model").arg(&turn.model);
        if let Some(e) = &turn.effort {
            cmd = cmd.arg("--effort").arg(e);
        }
        if let Some(schema) = &turn.response_schema {
            cmd = cmd.arg("--json-schema").arg(serde_json::to_string(schema)?);
        }
        if let Some(b) = self.cfg.max_budget_usd {
            cmd = cmd.arg("--max-budget-usd").arg(format!("{b}"));
        }
        match &turn.resume {
            Some(sid) => cmd = cmd.arg("--resume").arg(sid),
            None if self.cfg.warm => {}
            None => cmd = cmd.arg("--no-session-persistence"),
        }

        tracing::debug!(argv = %cmd.argv_for_log(), "spawning claude");
        let child = cmd
            .build()
            .spawn()
            .map_err(|e| HarnessError::unavailable(format!("failed to spawn claude: {e}")))?;
        let mut guard = ChildGuard::new(child, "claude", 8192);
        let mut stdin = guard
            .take_stdin()
            .ok_or_else(|| HarnessError::failed("no stdin"))?;
        let stdout = guard
            .take_stdout()
            .ok_or_else(|| HarnessError::failed("no stdout"))?;
        let input = turn.input.clone();
        tokio::spawn(async move {
            let _ = stdin.write_all(input.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });

        let max_line = self.limits.max_line_bytes;
        let want_reasoning = turn.want_reasoning;
        let scratch_keep = turn.scratch.clone();
        let s = stream! {
            let _scratch = scratch_keep;
            let _permit = permit;
            let mut guard = guard;
            let mut lines = LineReader::new(stdout, max_line);
            let mut rec = Reconciler::default();
            let mut usage: Option<Usage> = None;
            let mut done = false;

            loop {
                let line = tokio::select! {
                    _ = cancel.cancelled() => {
                        guard.kill_now();
                        yield Event::Done(FinishReason::Interrupted);
                        return;
                    }
                    l = lines.next_line() => l,
                };
                let line = match line {
                    Ok(Some(l)) => l,
                    Ok(None) => break,
                    Err(e) => { yield Event::Error(e); return; }
                };
                if line.trim().is_empty() { continue; }
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => { tracing::debug!(line = %line.chars().take(200).collect::<String>(), "claude: non-JSON line"); continue; }
                };
                match v.get("type").and_then(Value::as_str) {
                    Some("system") => {
                        if v.get("subtype").and_then(Value::as_str) == Some("init") {
                            if let Some(m) = v.get("model").and_then(Value::as_str) { yield Event::ResolvedModel(m.to_string()); }
                            if let Some(sid) = v.get("session_id").and_then(Value::as_str) { yield Event::Continuation(sid.to_string()); }
                        }
                    }
                    Some("stream_event") => {
                        let ev = v.get("event").cloned().unwrap_or(Value::Null);
                        if ev.get("type").and_then(Value::as_str) == Some("content_block_delta") {
                            let d = ev.get("delta").cloned().unwrap_or(Value::Null);
                            match d.get("type").and_then(Value::as_str) {
                                Some("text_delta") => {
                                    if let Some(t) = d.get("text").and_then(Value::as_str) {
                                        rec.delta(t);
                                        yield Event::TextDelta(t.to_string());
                                    }
                                }
                                Some("thinking_delta") if want_reasoning => {
                                    if let Some(t) = d.get("thinking").and_then(Value::as_str) {
                                        yield Event::ReasoningDelta(t.to_string());
                                    }
                                }
                                _ => {}
                            }
                        } else if ev.get("type").and_then(Value::as_str) == Some("message_delta") {
                            if let Some(u) = ev.get("usage") { usage = Some(parse_usage(u)); }
                        }
                    }
                    Some("assistant") => {
                        let msg = v.get("message").cloned().unwrap_or(Value::Null);
                        let synthetic = msg.get("model").and_then(Value::as_str) == Some("<synthetic>");
                        if !synthetic {
                            let text = message_text(&msg);
                            for e in rec.finalize(&text, "claude") { yield e; }
                            if let Some(u) = msg.get("usage") { usage = Some(parse_usage(u)); }
                        }
                    }
                    Some("result") => {
                        let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false)
                            || v.get("subtype").and_then(Value::as_str).is_some_and(|s| s.starts_with("error"));
                        if let Some(u) = v.get("usage") { usage = Some(parse_usage(u)); }
                        if is_error {
                            let msg = v.get("result").and_then(Value::as_str)
                                .map(String::from)
                                .or_else(|| v.get("errors").map(|e| e.to_string()))
                                .unwrap_or_else(|| "claude reported an error".into());
                            yield Event::Error(HarnessError::new(classify_message(&msg), msg));
                            return;
                        }
                        if let Some(t) = v.get("result").and_then(Value::as_str) {
                            for e in rec.finalize(t, "claude") { yield e; }
                        }
                        if let Some(u) = usage.take() { yield Event::Usage(u); }
                        done = true;
                        yield Event::Done(FinishReason::Stop);
                        break;
                    }
                    _ => {}
                }
            }

            if !done {
                let code = guard.wait_timeout(std::time::Duration::from_secs(2)).await;
                let tail = guard.stderr.snapshot();
                tracing::warn!(exit = ?code, stderr = %tail.chars().rev().take(2000).collect::<String>().chars().rev().collect::<String>(), "claude exited without a result");
                if rec.has_text() {
                    yield Event::Done(FinishReason::Stop);
                } else {
                    let msg = if tail.trim().is_empty() {
                        format!("claude exited (code {code:?}) without producing a result")
                    } else {
                        tail.lines().last().unwrap_or("claude failed").to_string()
                    };
                    yield Event::Error(HarnessError::new(classify_message(&msg), msg));
                }
            } else {
                // Non-zero exit after a successful result is logged, not surfaced.
                if let Some(code) = guard.wait_timeout(std::time::Duration::from_secs(5)).await {
                    if code != 0 { tracing::debug!(code, "claude non-zero exit after result"); }
                }
            }
        };
        Ok(Box::pin(s))
    }
}
