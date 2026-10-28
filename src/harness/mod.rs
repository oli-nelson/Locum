//! Harness abstraction: every adapter turns a [`Turn`] into a stream of [`Event`]s.

pub mod claude;
pub mod codex;
pub mod fake;
pub mod opencode;
pub mod util;

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HarnessKind {
    Codex,
    Claude,
    Opencode,
    Fake,
}

impl HarnessKind {
    pub const ALL: [HarnessKind; 3] = [
        HarnessKind::Codex,
        HarnessKind::Claude,
        HarnessKind::Opencode,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            HarnessKind::Codex => "codex",
            HarnessKind::Claude => "claude",
            HarnessKind::Opencode => "opencode",
            HarnessKind::Fake => "fake",
        }
    }

    pub fn owned_by(&self) -> &'static str {
        match self {
            HarnessKind::Codex => "codex",
            HarnessKind::Claude => "claude-code",
            HarnessKind::Opencode => "opencode",
            HarnessKind::Fake => "locum",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "codex" => Some(HarnessKind::Codex),
            "claude" | "claude-code" => Some(HarnessKind::Claude),
            "opencode" => Some(HarnessKind::Opencode),
            "fake" => Some(HarnessKind::Fake),
            _ => None,
        }
    }
}

impl fmt::Display for HarnessKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct ImageInput {
    /// Local file written into the scratch dir (data URIs) …
    pub path: Option<PathBuf>,
    /// … or a remote URL passed through.
    pub url: Option<String>,
    pub detail: Option<String>,
}

/// A single, self-contained model call.
#[derive(Debug, Clone)]
pub struct Turn {
    pub request_id: String,
    pub model: String,
    pub effort: Option<String>,
    pub system: Option<String>,
    pub input: String,
    pub images: Vec<ImageInput>,
    pub response_schema: Option<Value>,
    pub want_reasoning: bool,
    /// Whether the client asked for a streamed response (lets adapters pick a one-shot path).
    pub stream_hint: bool,
    /// Per-request empty working directory. Dropped (removed) with the request.
    pub scratch: Arc<tempfile::TempDir>,
    /// Harness continuation id from the session cache, if any.
    pub resume: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_prompt_tokens: u64,
    pub reasoning_tokens: u64,
    pub estimated: bool,
}

impl Usage {
    pub fn estimate(prompt_chars: usize, completion_chars: usize) -> Self {
        Self {
            prompt_tokens: (prompt_chars as u64).div_ceil(4),
            completion_tokens: (completion_chars as u64).div_ceil(4),
            cached_prompt_tokens: 0,
            reasoning_tokens: 0,
            estimated: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Binary missing, not authenticated, failed to start.
    Unavailable,
    /// Harness ran but the turn failed.
    Failed,
    Timeout,
    ContextLength,
    Interrupted,
    RateLimited,
    BadRequest,
}

#[derive(Debug, Clone)]
pub struct HarnessError {
    pub kind: ErrorKind,
    pub message: String,
}

impl HarnessError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unavailable, message)
    }
    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Failed, message)
    }
    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Timeout, message)
    }
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for HarnessError {}

impl From<std::io::Error> for HarnessError {
    fn from(e: std::io::Error) -> Self {
        HarnessError::failed(e.to_string())
    }
}

impl From<serde_json::Error> for HarnessError {
    fn from(e: serde_json::Error) -> Self {
        HarnessError::failed(format!("protocol parse error: {e}"))
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    TextDelta(String),
    ReasoningDelta(String),
    Usage(Usage),
    /// Harness-resolved model name, when reported.
    ResolvedModel(String),
    /// Continuation handle (thread/session id) usable for follow-up turns.
    Continuation(String),
    Done(FinishReason),
    Error(HarnessError),
}

pub type EventStream = Pin<Box<dyn Stream<Item = Event> + Send + 'static>>;

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub default_effort: Option<String>,
    pub efforts: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct HarnessInfo {
    pub executable: Option<PathBuf>,
    pub version: Option<String>,
    pub authenticated: Option<bool>,
    pub default_model: String,
    pub detail: Option<String>,
}

#[async_trait]
pub trait Harness: Send + Sync {
    fn kind(&self) -> HarnessKind;
    async fn probe(&self) -> Result<HarnessInfo, HarnessError>;
    async fn list_models(&self) -> Result<Vec<ModelInfo>, HarnessError>;
    async fn run(&self, turn: Turn, cancel: CancellationToken)
        -> Result<EventStream, HarnessError>;
    /// Drop any persisted continuation created with `resume` support.
    async fn forget(&self, _continuation: &str) {}
    /// Spawn warm workers ahead of the first request when the pool is configured to.
    async fn prewarm(&self) {}
    async fn shutdown(&self) {}
}

pub struct Registry {
    harnesses: HashMap<HarnessKind, Arc<dyn Harness>>,
}

impl Registry {
    pub fn from_config(cfg: &Config) -> Self {
        let mut harnesses: HashMap<HarnessKind, Arc<dyn Harness>> = HashMap::new();
        if cfg.harness.codex.enabled {
            harnesses.insert(
                HarnessKind::Codex,
                Arc::new(codex::CodexHarness::new(
                    cfg.harness.codex.clone(),
                    cfg.limits.clone(),
                )),
            );
        }
        if cfg.harness.claude.enabled {
            harnesses.insert(
                HarnessKind::Claude,
                Arc::new(claude::ClaudeHarness::new(
                    cfg.harness.claude.clone(),
                    cfg.limits.clone(),
                )),
            );
        }
        if cfg.harness.opencode.enabled {
            harnesses.insert(
                HarnessKind::Opencode,
                Arc::new(opencode::OpencodeHarness::new(
                    cfg.harness.opencode.clone(),
                    cfg.limits.clone(),
                )),
            );
        }
        if cfg!(feature = "fake") || std::env::var_os("LOCUM_FAKE_HARNESS").is_some() {
            harnesses.insert(HarnessKind::Fake, Arc::new(fake::FakeHarness));
        }
        Self { harnesses }
    }

    pub fn with(harnesses: Vec<Arc<dyn Harness>>) -> Self {
        Self {
            harnesses: harnesses.into_iter().map(|h| (h.kind(), h)).collect(),
        }
    }

    pub fn get(&self, kind: HarnessKind) -> Option<Arc<dyn Harness>> {
        self.harnesses.get(&kind).cloned()
    }

    pub fn kinds(&self) -> Vec<HarnessKind> {
        let mut v: Vec<_> = self.harnesses.keys().copied().collect();
        v.sort();
        v
    }

    pub fn iter(&self) -> impl Iterator<Item = (&HarnessKind, &Arc<dyn Harness>)> {
        self.harnesses.iter()
    }

    pub async fn shutdown(&self) {
        for h in self.harnesses.values() {
            h.shutdown().await;
        }
    }
}
