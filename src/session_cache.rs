//! Optional transcript-prefix → harness continuation cache (phase 2).
//!
//! Key: blake3(harness, model, system, messages[..n-1]). A hit lets the proxy send only the final
//! user message to a persisted harness session instead of re-rendering the whole transcript.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lru::LruCache;
use parking_lot::Mutex;

use crate::config::SessionCacheConfig;
use crate::harness::{HarnessKind, Registry};
use crate::openai::{Content, ContentPart, Message};

#[derive(Debug, Clone)]
pub struct Entry {
    pub harness: HarnessKind,
    pub continuation: String,
    /// Assistant reply we produced for this prefix; must match the client's next-to-last message.
    pub reply: String,
    pub at: Instant,
}

pub struct SessionCache {
    cfg: SessionCacheConfig,
    map: Mutex<LruCache<[u8; 32], Entry>>,
    registry: Arc<Registry>,
}

fn message_text(m: &Message) -> String {
    match &m.content {
        None => String::new(),
        Some(Content::Text(s)) => s.clone(),
        Some(Content::Parts(p)) => p
            .iter()
            .filter_map(|x| match x {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

pub fn key(harness: HarnessKind, model: &str, messages: &[Message]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(harness.as_str().as_bytes());
    h.update(b"\0");
    h.update(model.as_bytes());
    h.update(b"\0");
    for m in messages {
        h.update(m.role.as_bytes());
        h.update(b"\x01");
        h.update(message_text(m).as_bytes());
        h.update(b"\x02");
    }
    *h.finalize().as_bytes()
}

pub struct Hit {
    pub continuation: String,
}

impl SessionCache {
    pub fn new(cfg: SessionCacheConfig, registry: Arc<Registry>) -> Arc<Self> {
        let cache = Arc::new(Self {
            map: Mutex::new(LruCache::new(
                NonZeroUsize::new(cfg.max_entries.max(1)).unwrap(),
            )),
            cfg,
            registry,
        });
        let weak = Arc::downgrade(&cache);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                let Some(c) = weak.upgrade() else { break };
                c.expire().await;
            }
        });
        cache
    }

    /// Look for a stored continuation matching `messages[..n-1]` whose reply equals the
    /// client's last assistant message.
    pub fn lookup(&self, harness: HarnessKind, model: &str, messages: &[Message]) -> Option<Hit> {
        if messages.len() < 3 {
            return None;
        }
        let n = messages.len();
        let prev_assistant = &messages[n - 2];
        if prev_assistant.role != "assistant" {
            return None;
        }
        let k = key(harness, model, &messages[..n - 2]);
        let mut map = self.map.lock();
        let e = map.get(&k)?;
        if e.at.elapsed() > self.cfg.ttl || e.harness != harness {
            map.pop(&k);
            return None;
        }
        if e.reply.trim() != message_text(prev_assistant).trim() {
            return None;
        }
        let hit = Hit {
            continuation: e.continuation.clone(),
        };
        map.pop(&k);
        Some(hit)
    }

    /// Record the continuation after a successful turn so the next request can resume it.
    pub fn store(
        &self,
        harness: HarnessKind,
        model: &str,
        messages: &[Message],
        continuation: String,
        reply: String,
    ) {
        let k = key(harness, model, messages);
        let evicted = self.map.lock().push(
            k,
            Entry {
                harness,
                continuation,
                reply,
                at: Instant::now(),
            },
        );
        if let Some((_, old)) = evicted {
            self.forget(old);
        }
    }

    fn forget(&self, e: Entry) {
        if let Some(h) = self.registry.get(e.harness) {
            tokio::spawn(async move { h.forget(&e.continuation).await });
        }
    }

    async fn expire(&self) {
        let ttl = self.cfg.ttl;
        let expired: Vec<Entry> = {
            let mut map = self.map.lock();
            let keys: Vec<[u8; 32]> = map
                .iter()
                .filter(|(_, e)| e.at.elapsed() > ttl)
                .map(|(k, _)| *k)
                .collect();
            keys.into_iter().filter_map(|k| map.pop(&k)).collect()
        };
        for e in expired {
            self.forget(e);
        }
    }

    pub async fn shutdown(&self) {
        let all: Vec<Entry> = {
            let mut map = self.map.lock();
            let keys: Vec<[u8; 32]> = map.iter().map(|(k, _)| *k).collect();
            keys.into_iter().filter_map(|k| map.pop(&k)).collect()
        };
        for e in all {
            if let Some(h) = self.registry.get(e.harness) {
                h.forget(&e.continuation).await;
            }
        }
    }
}
