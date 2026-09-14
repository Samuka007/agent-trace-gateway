//! 节奏治理（D8 双时钟 + 单档位）：每账号并发上限 + 人类节律（突发/沉寂）
//! + 需求压力档位。
//!
//! 模型（docs/atcd-design.md §13）：
//! - 节律时钟（养号）：天然休整 → 突发（可连发 B 次）→ 燃尽沉寂；
//!   人类是「一段专注里连点几下，然后离开」，不是恒定速率。
//! - 需求时钟（排队）：请求在 `wait_turn` 等待；档位 `nurture_level` 决定
//!   沉寂余量的放弃比例（0=等满沉寂，1=立即放弃）。
//! - 地板不可让步：`min_turn_gap` 与 `burst_min_gap` 永远执行——亚秒级
//!   规律性是最强的机器信号，档位只调节歇息余量。
//!
//! 等待计入用户体验，所以地板默认值保守（毫秒~秒级），参数随真实流量迭代。

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

pub struct PacingConfig {
    /// 休整间隔地板（人类地板，档位不可让步）。
    pub min_turn_gap: Duration,
    /// 休整抖动上限（人的间隔从不精确）。
    pub jitter: Duration,
    /// 单账号同时在飞的请求数——"一个人同时挂着的终端"。
    pub concurrency_per_account: usize,
    /// D8 档位：养号纪律保留比例。0 = 养号绝对优先（等满沉寂），
    /// 1 = 需求绝对优先（沉寂立即放弃），中间线性。
    pub nurture_level: f64,
    /// D8 突发额度：一次天然休整后的连发上限（"专注操作里的连点"）。
    pub burst_turns: u32,
    /// D8 突发内间隔地板（不可让步）。
    pub burst_min_gap: Duration,
    /// D8 燃尽后沉寂基数。
    pub quiet: Duration,
    /// D8 沉寂抖动上限。
    pub quiet_jitter: Duration,
}

impl Default for PacingConfig {
    fn default() -> Self {
        Self {
            min_turn_gap: Duration::from_millis(1200),
            jitter: Duration::from_millis(900),
            concurrency_per_account: 2,
            nurture_level: 0.5,
            burst_turns: 3,
            burst_min_gap: Duration::from_millis(250),
            quiet: Duration::from_millis(8000),
            quiet_jitter: Duration::from_millis(3000),
        }
    }
}

#[derive(Default)]
struct Inner {
    semaphores: HashMap<String, Arc<Semaphore>>,
    last_turn: HashMap<String, std::time::Instant>,
    /// 剩余突发额度。
    burst_left: HashMap<String, u32>,
    /// 沉寂截止时刻。
    quiet_until: HashMap<String, std::time::Instant>,
}

/// 本次放行属于哪类（测试可观测；生产不依赖）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grant {
    /// 天然休整（且重臂突发额度）。
    Rested,
    /// 突发额度内连发。
    Burst,
    /// 沉寂期按档位提前放行。
    Decayed,
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
    if period_ns == 0 {
        return 0;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    now % period_ns
}

impl PacingGate {
    pub fn new(cfg: PacingConfig) -> Self {
        Self {
            cfg,
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn config(&self) -> &PacingConfig {
        &self.cfg
    }

    fn semaphore(&self, account_id: &str) -> Arc<Semaphore> {
        let mut inner = self.inner.lock();
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

    /// D8 判定 + 状态迁移（锁内完成，突发计数不会双花）。
    fn decide_and_commit(&self, account_id: &str, now: std::time::Instant) -> (Duration, Grant) {
        let mut inner = self.inner.lock();
        let last = inner.last_turn.get(account_id).copied();
        let quiet_until = inner.quiet_until.get(account_id).copied();
        let burst_left = inner
            .burst_left
            .get(account_id)
            .copied()
            .unwrap_or(self.cfg.burst_turns);
        let jitter = Duration::from_nanos(jitter_ns(self.cfg.jitter.as_nanos()) as u64);

        let rest_ok = last.is_none_or(|l| now.duration_since(l) >= self.cfg.min_turn_gap + jitter);
        let quiet_ok = quiet_until.is_none_or(|q| now >= q);
        if rest_ok && quiet_ok {
            // 天然休整：放行并重臂（用户休息后回来，又能快速连发）。
            inner
                .burst_left
                .insert(account_id.to_string(), self.cfg.burst_turns);
            return (Duration::ZERO, Grant::Rested);
        }

        let burst_floor_ok = last.is_none_or(|l| now.duration_since(l) >= self.cfg.burst_min_gap);
        if burst_left > 0 && burst_floor_ok {
            let left = burst_left - 1;
            inner.burst_left.insert(account_id.to_string(), left);
            if left == 0 {
                // 燃尽 → 进入沉寂。
                let qj = Duration::from_nanos(jitter_ns(self.cfg.quiet_jitter.as_nanos()) as u64);
                inner
                    .quiet_until
                    .insert(account_id.to_string(), now + self.cfg.quiet + qj);
            }
            return (Duration::ZERO, Grant::Burst);
        }

        // 沉寂：地板（gap 余量）全额等待，档位只萎缩「超出地板的沉寂余量」。
        // level=0 → 等满 quiet；level=1 → 只剩地板（见 D8 §13.3-1）。
        let gap_target = last.map(|l| l + self.cfg.min_turn_gap + jitter);
        let level = self.cfg.nurture_level.clamp(0.0, 1.0);
        let gap_remaining = gap_target
            .map(|t| t.saturating_duration_since(now))
            .unwrap_or(Duration::ZERO);
        let extra = match (gap_target, quiet_until) {
            (Some(g), Some(q)) => q.saturating_duration_since(g),
            (None, Some(q)) => q.saturating_duration_since(now),
            _ => Duration::ZERO,
        };
        let wait = gap_remaining + extra.mul_f64(1.0 - level);
        (wait, Grant::Decayed)
    }

    /// 回合等待：按 D8 状态机判定，睡满差值后记录本回合时刻。
    pub async fn wait_turn(&self, account_id: &str) {
        let (wait, _grant) = self.decide_and_commit(account_id, std::time::Instant::now());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        self.record_turn(account_id);
    }

    /// 记录一次回合完成时刻（内存态；持久化由调用方选择性落库）。
    pub fn record_turn(&self, account_id: &str) {
        let mut inner = self.inner.lock();
        inner
            .last_turn
            .insert(account_id.to_string(), std::time::Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(min_gap_ms: u64, burst: u32, quiet_ms: u64, level: f64) -> PacingConfig {
        PacingConfig {
            min_turn_gap: Duration::from_millis(min_gap_ms),
            jitter: Duration::ZERO,
            concurrency_per_account: 2,
            nurture_level: level,
            burst_turns: burst,
            burst_min_gap: Duration::from_millis(10),
            quiet: Duration::from_millis(quiet_ms),
            quiet_jitter: Duration::ZERO,
        }
    }

    fn burst_left(gate: &PacingGate, id: &str) -> u32 {
        gate.inner.lock().burst_left.get(id).copied().unwrap_or(0)
    }

    #[tokio::test]
    async fn first_turn_waits_zero() {
        let gate = PacingGate::new(cfg(50, 3, 200, 0.0));
        let t0 = std::time::Instant::now();
        gate.wait_turn("a").await;
        assert!(t0.elapsed() < Duration::from_millis(40), "首回合不应等待");
        assert_eq!(burst_left(&gate, "a"), 3, "首回合应重臂");
    }

    #[tokio::test]
    async fn second_turn_within_gap_waits() {
        let gate = PacingGate::new(cfg(120, 0, 0, 0.0)); // burst=0 关突发，档位 0
        gate.wait_turn("a").await;
        let t0 = std::time::Instant::now();
        gate.wait_turn("a").await;
        assert!(
            t0.elapsed() >= Duration::from_millis(100),
            "间隔内第二回合应等待"
        );
    }

    #[tokio::test]
    async fn gate_bounds_concurrency() {
        let gate = Arc::new(PacingGate::new(cfg(0, 0, 0, 1.0)));
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

    /// D8：突发连发耗尽后进入沉寂；档位 0 时等满沉寂。
    #[tokio::test]
    async fn burst_then_quiet_at_level_zero() {
        let quiet_ms = 400u64;
        let gate = PacingGate::new(cfg(1000, 2, quiet_ms, 0.0));
        gate.wait_turn("a").await; // Rested（首回合，重臂 2）

        let t0 = std::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(15)).await;
        gate.wait_turn("a").await; // Burst 1
        assert!(
            t0.elapsed() < Duration::from_millis(100),
            "突发内应快速放行"
        );
        assert_eq!(burst_left(&gate, "a"), 1);

        tokio::time::sleep(Duration::from_millis(15)).await;
        gate.wait_turn("a").await; // Burst 2 → 燃尽入沉寂
        assert_eq!(burst_left(&gate, "a"), 0);

        let t1 = std::time::Instant::now();
        gate.wait_turn("a").await; // 沉寂：档位 0 → 等满（地板 1000ms 主导，quiet 被吸收）
        assert!(
            t1.elapsed() >= Duration::from_millis(700),
            "档位 0 应等满地板的沉寂（实测 {:?}）",
            t1.elapsed()
        );
    }

    /// D8：档位 1 时沉寂立即放弃（只剩地板）。
    #[tokio::test]
    async fn level_one_abandons_quiet() {
        let gate = PacingGate::new(cfg(1000, 2, 5000, 1.0));
        gate.wait_turn("a").await;
        tokio::time::sleep(Duration::from_millis(15)).await;
        gate.wait_turn("a").await;
        tokio::time::sleep(Duration::from_millis(15)).await;
        gate.wait_turn("a").await; // 燃尽，quiet=5000ms

        let t0 = std::time::Instant::now();
        gate.wait_turn("a").await; // 档位 1 → 沉寂立即放弃，但地板保留
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_millis(700),
            "档位 1 仍应保留地板（实测 {:?}）",
            elapsed
        );
        assert!(
            elapsed < Duration::from_millis(2000),
            "档位 1 应放弃 5000ms 沉寂（实测 {:?}）",
            elapsed
        );
    }

    /// D8：档位 0.5 时半放弃沉寂余量（地板全额保留）。
    #[tokio::test]
    async fn half_level_halves_extra() {
        let gate = PacingGate::new(cfg(100, 1, 1000, 0.5));
        gate.wait_turn("a").await; // 重臂 1
        tokio::time::sleep(Duration::from_millis(15)).await;
        gate.wait_turn("a").await; // 燃尽，quiet=1000ms

        let t0 = std::time::Instant::now();
        gate.wait_turn("a").await;
        let elapsed = t0.elapsed();
        // 期望 ≈ 地板 85ms + 沉寂余量 900×0.5=450ms ≈ 535ms
        assert!(
            elapsed >= Duration::from_millis(350) && elapsed < Duration::from_millis(750),
            "档位 0.5 应半放弃沉寂余量（实测 {:?}）",
            elapsed
        );
    }

    /// D8：天然休整后突发额度重臂。
    #[tokio::test]
    async fn rest_rearms_burst() {
        let gate = PacingGate::new(cfg(50, 2, 100, 1.0));
        gate.wait_turn("a").await; // 重臂
        tokio::time::sleep(Duration::from_millis(15)).await;
        gate.wait_turn("a").await; // Burst → 1
        tokio::time::sleep(Duration::from_millis(15)).await;
        gate.wait_turn("a").await; // Burst → 0，入沉寂
        assert_eq!(burst_left(&gate, "a"), 0);

        // 等过 沉寂+间隔 地板 → 下一次天然休整应重臂
        tokio::time::sleep(Duration::from_millis(150)).await;
        gate.wait_turn("a").await;
        assert_eq!(burst_left(&gate, "a"), 2, "休整后应重臂");
    }
}
