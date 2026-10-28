//! Warm pools for long-lived harness server processes (one process serves many turns).

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use tokio::task::JoinHandle;

use crate::config::PoolConfig;
use crate::harness::HarnessError;

#[async_trait]
pub trait SharedWorker: Send + Sync + 'static {
    fn alive(&self) -> bool;
    async fn shutdown(&self);
}

struct Slot<W> {
    worker: Arc<W>,
    inflight: usize,
    turns: usize,
    last_used: Instant,
    retiring: bool,
}

type Factory<W> = dyn Fn() -> BoxFuture<'static, Result<Arc<W>, HarnessError>> + Send + Sync;

pub struct SharedPool<W: SharedWorker> {
    cfg: PoolConfig,
    slots: Mutex<Vec<Slot<W>>>,
    factory: Box<Factory<W>>,
    spawn_lock: tokio::sync::Mutex<()>,
    reaper: Mutex<Option<JoinHandle<()>>>,
}

pub struct Lease<W: SharedWorker> {
    pool: Arc<SharedPool<W>>,
    pub worker: Arc<W>,
    pub spawned: bool,
}

impl<W: SharedWorker> std::ops::Deref for Lease<W> {
    type Target = W;
    fn deref(&self) -> &W {
        &self.worker
    }
}

impl<W: SharedWorker> Drop for Lease<W> {
    fn drop(&mut self) {
        self.pool.release(&self.worker);
    }
}

impl<W: SharedWorker> SharedPool<W> {
    pub fn new(
        cfg: PoolConfig,
        factory: impl Fn() -> BoxFuture<'static, Result<Arc<W>, HarnessError>> + Send + Sync + 'static,
    ) -> Arc<Self> {
        let pool = Arc::new(Self {
            cfg,
            slots: Mutex::new(Vec::new()),
            factory: Box::new(factory),
            spawn_lock: tokio::sync::Mutex::new(()),
            reaper: Mutex::new(None),
        });
        pool.start_reaper();
        pool
    }

    fn start_reaper(self: &Arc<Self>) {
        let ttl = self.cfg.idle_ttl;
        if ttl.is_zero() {
            return;
        }
        let weak = Arc::downgrade(self);
        let handle = tokio::spawn(async move {
            let period = (ttl / 4).max(std::time::Duration::from_secs(1));
            let mut tick = tokio::time::interval(period);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(pool) = weak.upgrade() else { break };
                pool.reap().await;
            }
        });
        *self.reaper.lock() = Some(handle);
    }

    async fn reap(&self) {
        let now = Instant::now();
        let victims: Vec<Arc<W>> = {
            let mut slots = self.slots.lock();
            let mut victims = Vec::new();
            let mut i = 0;
            while i < slots.len() {
                let s = &slots[i];
                let idle = s.inflight == 0 && now.duration_since(s.last_used) > self.cfg.idle_ttl;
                let dead = !s.worker.alive();
                let excess = slots.len().saturating_sub(victims.len()) > self.cfg.min_idle;
                if dead || (idle && excess) || (s.retiring && s.inflight == 0) {
                    victims.push(slots.remove(i).worker);
                } else {
                    i += 1;
                }
            }
            victims
        };
        for v in victims {
            v.shutdown().await;
        }
    }

    pub async fn prewarm(self: &Arc<Self>) {
        if !self.cfg.prewarm {
            return;
        }
        let want = self.cfg.min_idle.max(1).min(self.cfg.max_workers.max(1));
        for _ in 0..want {
            if let Err(e) = self.spawn_slot().await {
                tracing::warn!(error = %e, "pool prewarm failed");
                break;
            }
        }
    }

    async fn spawn_slot(self: &Arc<Self>) -> Result<Arc<W>, HarnessError> {
        let worker = (self.factory)().await?;
        self.slots.lock().push(Slot {
            worker: worker.clone(),
            inflight: 0,
            turns: 0,
            last_used: Instant::now(),
            retiring: false,
        });
        Ok(worker)
    }

    fn try_pick(&self) -> Option<Arc<W>> {
        let mut slots = self.slots.lock();
        slots.retain(|s| s.worker.alive());
        let max_turns = self.cfg.max_turns_per_worker;
        let best = slots
            .iter_mut()
            .filter(|s| !s.retiring && (max_turns == 0 || s.turns < max_turns))
            .min_by_key(|s| s.inflight)?;
        if best.inflight > 0 && slots.len() < self.cfg.max_workers.max(1) {
            // Prefer spawning another process when every existing one is busy and we have room.
            return None;
        }
        let best = slots
            .iter_mut()
            .filter(|s| !s.retiring && (max_turns == 0 || s.turns < max_turns))
            .min_by_key(|s| s.inflight)?;
        best.inflight += 1;
        best.last_used = Instant::now();
        Some(best.worker.clone())
    }

    fn pick_any(&self) -> Option<Arc<W>> {
        let mut slots = self.slots.lock();
        let max_turns = self.cfg.max_turns_per_worker;
        let best = slots
            .iter_mut()
            .filter(|s| s.worker.alive() && !s.retiring && (max_turns == 0 || s.turns < max_turns))
            .min_by_key(|s| s.inflight)?;
        best.inflight += 1;
        best.last_used = Instant::now();
        Some(best.worker.clone())
    }

    pub async fn acquire(self: &Arc<Self>) -> Result<Lease<W>, HarnessError> {
        if let Some(w) = self.try_pick() {
            return Ok(Lease {
                pool: self.clone(),
                worker: w,
                spawned: false,
            });
        }
        let _g = self.spawn_lock.lock().await;
        // Someone else may have spawned while we waited.
        if let Some(w) = self.try_pick() {
            return Ok(Lease {
                pool: self.clone(),
                worker: w,
                spawned: false,
            });
        }
        let count = self.slots.lock().len();
        if count >= self.cfg.max_workers.max(1) {
            // At capacity: multiplex onto the least loaded process.
            if let Some(w) = self.pick_any() {
                return Ok(Lease {
                    pool: self.clone(),
                    worker: w,
                    spawned: false,
                });
            }
        }
        let worker = self.spawn_slot().await?;
        {
            let mut slots = self.slots.lock();
            if let Some(s) = slots.iter_mut().find(|s| Arc::ptr_eq(&s.worker, &worker)) {
                s.inflight += 1;
            }
        }
        Ok(Lease {
            pool: self.clone(),
            worker,
            spawned: true,
        })
    }

    fn release(&self, worker: &Arc<W>) {
        let retire = {
            let mut slots = self.slots.lock();
            let Some(idx) = slots.iter().position(|s| Arc::ptr_eq(&s.worker, worker)) else {
                return;
            };
            let s = &mut slots[idx];
            s.inflight = s.inflight.saturating_sub(1);
            s.turns += 1;
            s.last_used = Instant::now();
            if self.cfg.max_turns_per_worker > 0 && s.turns >= self.cfg.max_turns_per_worker {
                s.retiring = true;
            }
            if !s.worker.alive() || (s.retiring && s.inflight == 0) {
                Some(slots.remove(idx).worker)
            } else {
                None
            }
        };
        if let Some(w) = retire {
            tokio::spawn(async move { w.shutdown().await });
        }
    }

    pub fn len(&self) -> usize {
        self.slots.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub async fn shutdown(&self) {
        if let Some(h) = self.reaper.lock().take() {
            h.abort();
        }
        let all: Vec<Arc<W>> = self.slots.lock().drain(..).map(|s| s.worker).collect();
        for w in all {
            w.shutdown().await;
        }
    }
}
