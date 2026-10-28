//! Model-string resolution and the discovered-model catalog.

use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

use crate::config::Config;
use crate::harness::{HarnessKind, Registry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub harness: HarnessKind,
    pub model: String,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteError {
    Unknown(String),
    Disabled(HarnessKind),
}

fn split_effort(s: &str) -> (&str, Option<String>) {
    match s.rsplit_once('#') {
        Some((m, e)) if !e.is_empty() && !m.is_empty() => (m, Some(e.to_string())),
        _ => (s, None),
    }
}

fn resolve_inner(cfg: &Config, requested: &str, depth: u8) -> Result<Resolved, RouteError> {
    if depth > 4 {
        return Err(RouteError::Unknown(requested.to_string()));
    }
    if let Some(target) = cfg.models.aliases.get(requested) {
        return resolve_inner(cfg, target, depth + 1);
    }
    let (body, effort) = split_effort(requested);
    if body == "default" {
        return resolve_inner(cfg, &cfg.models.default, depth + 1).map(|mut r| {
            if effort.is_some() {
                r.effort = effort;
            }
            r
        });
    }
    if let Some(kind) = HarnessKind::parse(body) {
        if !cfg.harness_enabled(kind) {
            return Err(RouteError::Disabled(kind));
        }
        let (m, e2) = split_effort(cfg.harness_default_model(kind));
        return Ok(Resolved {
            harness: kind,
            model: m.to_string(),
            effort: effort
                .or(e2)
                .or_else(|| cfg.harness_default_effort(kind).map(String::from)),
        });
    }
    if let Some((prefix, rest)) = body.split_once('/') {
        if let Some(kind) = HarnessKind::parse(prefix) {
            if !cfg.harness_enabled(kind) {
                return Err(RouteError::Disabled(kind));
            }
            if rest.is_empty() {
                return Err(RouteError::Unknown(requested.to_string()));
            }
            return Ok(Resolved {
                harness: kind,
                model: rest.to_string(),
                effort: effort.or_else(|| cfg.harness_default_effort(kind).map(String::from)),
            });
        }
    }
    if cfg.models.fallback_to_default {
        return resolve_inner(cfg, &cfg.models.default, depth + 1);
    }
    Err(RouteError::Unknown(requested.to_string()))
}

pub fn resolve(cfg: &Config, requested: &str) -> Result<Resolved, RouteError> {
    resolve_inner(cfg, requested.trim(), 0)
}

#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub id: String,
    pub owned_by: &'static str,
}

pub struct Catalog {
    cfg: Arc<Config>,
    registry: Arc<Registry>,
    cache: Mutex<Option<(Instant, Vec<CatalogEntry>)>>,
    created: u64,
}

impl Catalog {
    pub fn new(cfg: Arc<Config>, registry: Arc<Registry>) -> Self {
        Self {
            cfg,
            registry,
            cache: Mutex::new(None),
            created: crate::openai::now_unix(),
        }
    }

    pub fn created(&self) -> u64 {
        self.created
    }

    pub fn static_entries(&self) -> Vec<CatalogEntry> {
        let mut out = Vec::new();
        out.push(CatalogEntry {
            id: "default".into(),
            owned_by: "locum",
        });
        for alias in self.cfg.models.aliases.keys() {
            out.push(CatalogEntry {
                id: alias.clone(),
                owned_by: "locum",
            });
        }
        for kind in self.registry.kinds() {
            out.push(CatalogEntry {
                id: kind.as_str().into(),
                owned_by: kind.owned_by(),
            });
            let (m, _) = split_effort(self.cfg.harness_default_model(kind));
            out.push(CatalogEntry {
                id: format!("{}/{}", kind.as_str(), m),
                owned_by: kind.owned_by(),
            });
            if kind == HarnessKind::Claude {
                for m in &self.cfg.harness.claude.models {
                    out.push(CatalogEntry {
                        id: format!("claude/{m}"),
                        owned_by: kind.owned_by(),
                    });
                }
            }
        }
        dedup(out)
    }

    pub async fn entries(&self) -> Vec<CatalogEntry> {
        let mut out = self.static_entries();
        if !self.cfg.models.discover {
            return out;
        }
        if let Some((at, cached)) = self.cache.lock().clone() {
            if at.elapsed() < self.cfg.models.discover_ttl {
                out.extend(cached);
                return dedup(out);
            }
        }
        let mut discovered = Vec::new();
        for (kind, h) in self.registry.iter() {
            if *kind == HarnessKind::Claude || *kind == HarnessKind::Fake {
                continue;
            }
            match tokio::time::timeout(std::time::Duration::from_secs(20), h.list_models()).await {
                Ok(Ok(models)) => {
                    for m in models {
                        discovered.push(CatalogEntry {
                            id: format!("{}/{}", kind.as_str(), m.id),
                            owned_by: kind.owned_by(),
                        });
                    }
                }
                Ok(Err(e)) => {
                    tracing::debug!(harness = %kind, error = %e, "model discovery failed")
                }
                Err(_) => tracing::debug!(harness = %kind, "model discovery timed out"),
            }
        }
        *self.cache.lock() = Some((Instant::now(), discovered.clone()));
        out.extend(discovered);
        dedup(out)
    }

    pub fn invalidate(&self) {
        *self.cache.lock() = None;
    }
}

fn dedup(mut v: Vec<CatalogEntry>) -> Vec<CatalogEntry> {
    let mut seen = std::collections::HashSet::new();
    v.retain(|e| seen.insert(e.id.clone()));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        let mut c = Config::default();
        c.models
            .aliases
            .insert("gpt-4o".into(), "codex/gpt-6-luna#high".into());
        c.models
            .aliases
            .insert("claude".into(), "claude/sonnet".into());
        c
    }

    #[test]
    fn prefixed_with_effort() {
        let r = resolve(&cfg(), "opencode/github-copilot/claude-sonnet-5#high").unwrap();
        assert_eq!(r.harness, HarnessKind::Opencode);
        assert_eq!(r.model, "github-copilot/claude-sonnet-5");
        assert_eq!(r.effort.as_deref(), Some("high"));
    }

    #[test]
    fn alias_and_bare() {
        let r = resolve(&cfg(), "gpt-4o").unwrap();
        assert_eq!(r.harness, HarnessKind::Codex);
        assert_eq!(r.effort.as_deref(), Some("high"));
        let r = resolve(&cfg(), "claude").unwrap();
        assert_eq!(r.model, "sonnet");
        let r = resolve(&cfg(), "codex").unwrap();
        assert_eq!(r.model, "gpt-6-luna");
        assert_eq!(r.effort.as_deref(), Some("medium"));
        let r = resolve(&cfg(), "default").unwrap();
        assert_eq!(r.harness, HarnessKind::Codex);
    }

    #[test]
    fn unknown() {
        assert!(matches!(
            resolve(&cfg(), "gpt-3.5"),
            Err(RouteError::Unknown(_))
        ));
        let mut c = cfg();
        c.models.fallback_to_default = true;
        assert!(resolve(&c, "gpt-3.5").is_ok());
    }
}
