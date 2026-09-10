//! 节奏治理：每账号并发上限 + 相邻回合最小间隔。
//!
//! 请求进入账号队列时记下时刻，发出前若距上一回合不足
//! `min_gap + jitter` 就等到够了再发。等待计入用户体验，所以
//! gap 的默认值保守（毫秒级），参数随真实流量迭代。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Semaphore;

pub struct PacingConfig {
    /// 相邻回合的基础间隔。
    pub min_turn_gap: Duration,
    /// 附加抖动上限（人的间隔从不精确）。
    pub jitter: Duration,
    /// 单账号同时在飞的请求数——"一个人同时挂着的终端"。
    pub concurrency_per_account: usize,
}

impl Default for PacingConfig {
    fn default() -> Self {
        Self {
            min_turn_gap: Duration::from_millis(1200),
            jitter: Duration::from_millis(900),
            concurrency_per_account: 2,
        }
    }
}

#[derive(Default)]
struct Inner {
    semaphores: HashMap<String, Arc<Semaphore>>,
    last_turn: HashMap<String, std::time::Instant>,
}

pub struct PacingGate {
    cfg: PacingConfig,
    inner: Mutex<Inner>,
}

/// 持有即占用一个并发名额，drop 时释放。
pub struct PacingSlot {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

/// 廉价抖动源：纳秒时钟对周期取模。非加密用途足够，原型不引 rand。
fn jitter_ns(period_ns: u128) -> u128 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    now.as_nanos() % period_ns.max(1)
}

impl PacingGate {
    pub fn new(cfg: PacingConfig) -> Self {
        Self { cfg, inner: Mutex::new(Inner::default()) }
    }

    pub fn config(&self) -> &PacingConfig {
        &self.cfg
    }

    fn semaphore(&self, account_id: &str) -> Arc<Semaphore> {
        let mut inner = self.inner.lock().unwrap();
        inner
            .semaphores
            .entry(account_id.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.cfg.concurrency_per_account)))
            .clone()
    }

    /// 获取账号并发名额；超出上限时在此排队（这是设计好的排队点）。
    pub async fn slot(&self, account_id: &str) -> PacingSlot {
        let sem = self.semaphore(account_id);
        let permit = sem.acquire_owned().await.expect("semaphore closed");
        PacingSlot { _permit: permit }
    }

    /// 回合间隔等待：距该账号上一回合不足 gap 时睡满差值，并记下本回合时刻。
    pub async fn wait_turn(&self, account_id: &str) {
        let wait = {
            let inner = self.inner.lock().unwrap();
            let last = inner.last_turn.get(account_id).copied();
            let base = self.cfg.min_turn_gap;
            let jitter = Duration::from_nanos(jitter_ns(self.cfg.jitter.as_nanos()) as u64);
            match last {
                Some(last) => {
                    let elapsed = last.elapsed();
                    if elapsed >= base {
                        Duration::ZERO
                    } else {
                        base - elapsed + jitter
                    }
                }
                None => Duration::ZERO,
            }
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        self.record_turn(account_id);
    }

    /// 记录一次回合时刻（内存态；持久化由调用方选择性落库）。
    pub fn record_turn(&self, account_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.last_turn.insert(account_id.to_string(), std::time::Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_turn_waits_zero() {
        let gate = PacingGate::new(PacingConfig {
            min_turn_gap: Duration::from_millis(50),
            jitter: Duration::ZERO,
            concurrency_per_account: 2,
        });
        let t0 = std::time::Instant::now();
        gate.wait_turn("a").await;
        assert!(t0.elapsed() < Duration::from_millis(40), "首回合不应等待");
    }

    #[tokio::test]
    async fn second_turn_within_gap_waits() {
        let gate = PacingGate::new(PacingConfig {
            min_turn_gap: Duration::from_millis(120),
            jitter: Duration::ZERO,
            concurrency_per_account: 2,
        });
        gate.wait_turn("a").await;
        let t0 = std::time::Instant::now();
        gate.wait_turn("a").await;
        assert!(t0.elapsed() >= Duration::from_millis(100), "间隔内第二回合应等待");
    }

    #[tokio::test]
    async fn gate_bounds_concurrency() {
        let gate = Arc::new(PacingGate::new(PacingConfig {
            min_turn_gap: Duration::ZERO,
            jitter: Duration::ZERO,
            concurrency_per_account: 2,
        }));
        let s1 = gate.slot("a").await;
        let s2 = gate.slot("a").await;
        let g = gate.clone();
        let third = tokio::spawn(async move {
            let _s3 = g.slot("a").await;
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!third.is_finished(), "第三个名额应仍在排队");
        drop(s1);
        drop(s2);
        third.await.unwrap();
    }
}
