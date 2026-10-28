use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::harness::HarnessKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub limits: LimitsConfig,
    pub models: ModelsConfig,
    pub compat: CompatConfig,
    pub prompt: PromptConfig,
    pub harness: HarnessesConfig,
    pub session_cache: SessionCacheConfig,
    pub log: LogConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub unix: Option<PathBuf>,
    pub cors_origins: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct AuthConfig {
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    pub max_inflight: usize,
    #[serde(with = "humantime_serde")]
    pub queue_timeout: Duration,
    #[serde(with = "humantime_serde")]
    pub request_timeout: Duration,
    #[serde(with = "humantime_serde")]
    pub first_token_timeout: Duration,
    pub max_body_bytes: usize,
    pub max_prompt_chars: usize,
    pub max_line_bytes: usize,
    pub max_completion_bytes: usize,
    #[serde(with = "humantime_serde")]
    pub micro_batch: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelsConfig {
    pub default: String,
    pub discover: bool,
    pub fallback_to_default: bool,
    #[serde(with = "humantime_serde")]
    pub discover_ttl: Duration,
    pub aliases: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompatConfig {
    pub ignore_tools: bool,
    pub reasoning_content: bool,
    pub stop_sequences: bool,
    pub max_tokens_truncation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromptConfig {
    pub preamble: bool,
    pub preamble_text: Option<String>,
    pub transcript_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct HarnessesConfig {
    pub codex: CodexConfig,
    pub claude: ClaudeConfig,
    pub opencode: OpencodeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PoolConfig {
    pub min_idle: usize,
    pub max_workers: usize,
    #[serde(with = "humantime_serde")]
    pub idle_ttl: Duration,
    pub max_turns_per_worker: usize,
    pub prewarm: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodexMode {
    AppServer,
    Exec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodexConfig {
    pub enabled: bool,
    pub executable: Option<PathBuf>,
    pub mode: CodexMode,
    pub default_model: String,
    pub default_effort: Option<String>,
    /// Run app-server with a private `CODEX_HOME` that only contains a link to the user's
    /// `auth.json`, so user MCP servers, hooks and plugins are not loaded.
    pub isolate_home: bool,
    pub strip_provider_keys: bool,
    pub reasoning_summary: bool,
    pub pool: PoolConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClaudeConfig {
    pub enabled: bool,
    pub executable: Option<PathBuf>,
    pub default_model: String,
    pub default_effort: Option<String>,
    pub models: Vec<String>,
    pub warm: bool,
    pub max_budget_usd: Option<f64>,
    pub strip_provider_keys: bool,
    pub pool: PoolConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpencodeConfig {
    pub enabled: bool,
    pub executable: Option<PathBuf>,
    pub default_model: String,
    pub default_effort: Option<String>,
    pub oneshot: bool,
    /// Run `opencode serve` with an empty `OPENCODE_CONFIG_DIR` so user MCP servers and
    /// plugins are not loaded. Auth is stored separately and keeps working.
    pub isolate_config: bool,
    pub strip_provider_keys: bool,
    pub pool: PoolConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionCacheConfig {
    pub enabled: bool,
    #[serde(with = "humantime_serde")]
    pub ttl: Duration,
    pub max_entries: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    pub level: String,
    pub json: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:11435".parse().unwrap(),
            unix: None,
            cors_origins: vec!["*".into()],
        }
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_inflight: 8,
            queue_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(300),
            first_token_timeout: Duration::from_secs(90),
            max_body_bytes: 4 * 1024 * 1024,
            max_prompt_chars: 400_000,
            max_line_bytes: 1024 * 1024,
            max_completion_bytes: 8 * 1024 * 1024,
            micro_batch: Duration::from_millis(5),
        }
    }
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            default: "codex".into(),
            discover: true,
            fallback_to_default: false,
            discover_ttl: Duration::from_secs(600),
            aliases: BTreeMap::new(),
        }
    }
}

impl Default for CompatConfig {
    fn default() -> Self {
        Self {
            ignore_tools: false,
            reasoning_content: false,
            stop_sequences: true,
            max_tokens_truncation: true,
        }
    }
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            preamble: true,
            preamble_text: None,
            transcript_template: None,
        }
    }
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            min_idle: 0,
            max_workers: 2,
            idle_ttl: Duration::from_secs(120),
            max_turns_per_worker: 200,
            prewarm: false,
        }
    }
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            executable: None,
            mode: CodexMode::AppServer,
            default_model: "gpt-6-luna".into(),
            default_effort: Some("medium".into()),
            isolate_home: false,
            strip_provider_keys: false,
            reasoning_summary: true,
            pool: PoolConfig {
                max_workers: 2,
                ..PoolConfig::default()
            },
        }
    }
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            executable: None,
            default_model: "sonnet".into(),
            default_effort: None,
            models: vec![
                "opus".into(),
                "sonnet".into(),
                "haiku".into(),
                "fable".into(),
            ],
            warm: false,
            max_budget_usd: None,
            strip_provider_keys: false,
            pool: PoolConfig {
                max_workers: 4,
                ..PoolConfig::default()
            },
        }
    }
}

impl Default for OpencodeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            executable: None,
            default_model: "github-copilot/claude-sonnet-5".into(),
            default_effort: None,
            oneshot: true,
            isolate_config: true,
            strip_provider_keys: false,
            pool: PoolConfig {
                max_workers: 1,
                idle_ttl: Duration::from_secs(300),
                ..PoolConfig::default()
            },
        }
    }
}

impl Default for SessionCacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ttl: Duration::from_secs(600),
            max_entries: 256,
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            json: false,
        }
    }
}

pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("locum").join("config.toml"))
}

pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::cache_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("locum")
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut cfg = match path {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .with_context(|| format!("reading config {}", p.display()))?;
                toml::from_str(&text).with_context(|| format!("parsing config {}", p.display()))?
            }
            None => match default_config_path() {
                Some(p) if p.exists() => {
                    let text = std::fs::read_to_string(&p)
                        .with_context(|| format!("reading config {}", p.display()))?;
                    toml::from_str(&text)
                        .with_context(|| format!("parsing config {}", p.display()))?
                }
                _ => Config::default(),
            },
        };
        cfg.apply_env()?;
        Ok(cfg)
    }

    fn apply_env(&mut self) -> Result<()> {
        if let Ok(v) = std::env::var("LOCUM_BIND") {
            self.server.bind = v.parse().context("LOCUM_BIND")?;
        }
        if let Ok(v) = std::env::var("LOCUM_KEYS") {
            self.auth.keys = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Ok(v) = std::env::var("LOCUM_DEFAULT_MODEL") {
            self.models.default = v;
        }
        if let Ok(v) = std::env::var("LOCUM_LOG") {
            self.log.level = v;
        }
        if let Ok(v) = std::env::var("LOCUM_CODEX_BIN") {
            self.harness.codex.executable = Some(v.into());
        }
        if let Ok(v) = std::env::var("LOCUM_CLAUDE_BIN") {
            self.harness.claude.executable = Some(v.into());
        }
        if let Ok(v) = std::env::var("LOCUM_OPENCODE_BIN") {
            self.harness.opencode.executable = Some(v.into());
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        let ip = self.server.bind.ip();
        if !ip.is_loopback() && self.auth.keys.is_empty() {
            anyhow::bail!(
                "refusing to bind to non-loopback address {} without [auth] keys configured",
                self.server.bind
            );
        }
        if self.limits.max_inflight == 0 {
            anyhow::bail!("limits.max_inflight must be > 0");
        }
        Ok(())
    }

    pub fn harness_enabled(&self, kind: HarnessKind) -> bool {
        match kind {
            HarnessKind::Codex => self.harness.codex.enabled,
            HarnessKind::Claude => self.harness.claude.enabled,
            HarnessKind::Opencode => self.harness.opencode.enabled,
            HarnessKind::Fake => true,
        }
    }

    pub fn harness_default_model(&self, kind: HarnessKind) -> &str {
        match kind {
            HarnessKind::Codex => &self.harness.codex.default_model,
            HarnessKind::Claude => &self.harness.claude.default_model,
            HarnessKind::Opencode => &self.harness.opencode.default_model,
            HarnessKind::Fake => "fake",
        }
    }

    pub fn harness_default_effort(&self, kind: HarnessKind) -> Option<&str> {
        match kind {
            HarnessKind::Codex => self.harness.codex.default_effort.as_deref(),
            HarnessKind::Claude => self.harness.claude.default_effort.as_deref(),
            HarnessKind::Opencode => self.harness.opencode.default_effort.as_deref(),
            HarnessKind::Fake => None,
        }
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }
}
