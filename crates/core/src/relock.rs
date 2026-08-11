//! P7.2 — relock 防震荡与自适应周期（B1+D3，ARCH-3 共享 RelockGuard）。
//!
//! 三个组件：
//! - `RelockGuard`：所有 relock 入口（周期 B1 / 即时 D3）的统一冷却闸门——
//!   防 AMS↔threadctl 震荡（检测→修改→检测→修改死循环）。
//! - `AdaptiveRelock`：观察窗口 ≥30s 确认持续覆盖才缩短周期（60→10→3s），
//!   连续稳定才延长（10→60→300s）——防单次覆盖噪声触发周期抖动。
//! - `read_cpuset_owner` / `is_in_our_cpuset` / `sample_coverage`：D3 覆盖检测
//!   （/proc/<pid>/cpuset 归属）。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// 读 `/proc/<pid>/cpuset` 归属（如 "/threadctl/3-7"）。
pub fn read_cpuset_owner(pid: i32) -> Option<String> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/cpuset")).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// 进程是否仍在我们创建的 cpuset 下（D3 覆盖检测）。
/// `base` 即 `/dev/cpuset/threadctl`（topology::BASE_CPUSET）。
pub fn is_in_our_cpuset(pid: i32, base: &str) -> bool {
    read_cpuset_owner(pid).is_some_and(|owner| owner.starts_with(base))
}

/// D3：抽样全部被跟踪进程的 cpuset 归属，返回被覆盖比例（0.0~1.0）。
/// 由 daemon 周期调用（如每 5s）→ 喂给 `AdaptiveRelock::observe`。
pub fn sample_coverage(tracker: &crate::tracker::StateTracker, base: &str) -> f64 {
    let pids = tracker.pids();
    if pids.is_empty() {
        return 0.0;
    }
    let mut covered = 0usize;
    for pid in &pids {
        if !is_in_our_cpuset(*pid, base) {
            covered += 1;
        }
    }
    covered as f64 / pids.len() as f64
}

/// 统一 relock 冷却闸门（ARCH-3）：周期 relock（B1）与即时 relock（D3）
/// 都先 `try_lock`，cooldown 内不执行——防高频触发互相叠加。
#[derive(Debug, Clone)]
pub struct RelockGuard {
    last_at: Instant,
    cooldown_ms: u64,
}

impl RelockGuard {
    pub fn new() -> Self {
        Self {
            // 初始冷却 0：启动后第一次周期 relock 立即放行
            last_at: Instant::now() - Duration::from_secs(3600),
            cooldown_ms: 0,
        }
    }

    /// cooldown 内返回 false（拒绝）；否则记录 now 并返回 true（放行）。
    pub fn try_lock(&mut self, now: Instant) -> bool {
        if now.duration_since(self.last_at).as_millis() as u64 >= self.cooldown_ms {
            self.last_at = now;
            true
        } else {
            false
        }
    }

    pub fn set_cooldown(&mut self, ms: u64) {
        self.cooldown_ms = ms;
    }

    pub fn cooldown_ms(&self) -> u64 {
        self.cooldown_ms
    }

    /// 距下次放行的剩余冷却（调试/观测用）。
    pub fn remaining(&self, now: Instant) -> Duration {
        let elapsed = now.duration_since(self.last_at).as_millis() as u64;
        Duration::from_millis(self.cooldown_ms.saturating_sub(elapsed))
    }
}

impl Default for RelockGuard {
    fn default() -> Self {
        Self::new()
    }
}

// ── B1 自适应周期 ──────────────────────────────────────────────

/// 周期档位（rank 0=3s 下限 … rank 3=300s 上限）。
/// 缩短路径：60 → 10 → 3；延长路径：10 → 60 → 300（相邻档位邻接）。
pub const INTERVALS_SECS: [u64; 4] = [3, 10, 60, 300];
pub const DEFAULT_INTERVAL: u64 = 60;

/// 覆盖采样周期（daemon 调用 observe 的节奏）。
pub const SAMPLE_INTERVAL_SECS: u64 = 5;
/// 观察窗口 ≥30s（规划书要求）→ 6 个采样。
pub const WINDOW_SAMPLES: usize = 6;
/// 窗口内覆盖比例 ≥ 此值 → 缩短一级。
const COVER_RATIO_SHRINK: f64 = 0.5;
/// 连续无覆盖时长 ≥ 此值（秒）→ 延长一级。
const STABLE_SECS_EXTEND: u64 = 60;

/// B1 自适应 relock 周期状态机。
///
/// 防震荡约束（三审一致）：
/// - 窗口未满（<30s）不调整；
/// - 单次覆盖失败不调整——持续覆盖才缩短；
/// - 连续稳定 60s 才延长——避免覆盖解除后立即回弹。
#[derive(Debug, Clone)]
pub struct AdaptiveRelock {
    rank: usize,
    samples: VecDeque<bool>,
    stable_secs: u64,
}

impl AdaptiveRelock {
    pub fn new() -> Self {
        Self {
            rank: INTERVALS_SECS.iter().position(|&v| v == DEFAULT_INTERVAL).unwrap_or(2),
            samples: VecDeque::with_capacity(WINDOW_SAMPLES),
            stable_secs: 0,
        }
    }

    /// 从配置的初始周期起步（映射到最近档位；如用户 lock-interval=10）。
    pub fn from_initial(secs: u64) -> Self {
        let rank = INTERVALS_SECS
            .iter()
            .enumerate()
            .min_by_key(|(_, &v)| (v as i64 - secs as i64).abs())
            .map(|(i, _)| i)
            .unwrap_or(2);
        Self {
            rank,
            samples: VecDeque::with_capacity(WINDOW_SAMPLES),
            stable_secs: 0,
        }
    }

    pub fn interval_secs(&self) -> u64 {
        INTERVALS_SECS[self.rank]
    }

    /// 记录一次覆盖采样（covered=true 表示发现进程被移出我们的 cpuset）。
    /// 每 `SAMPLE_INTERVAL_SECS` 调用一次。
    pub fn observe(&mut self, covered: bool) {
        self.samples.push_back(covered);
        if self.samples.len() > WINDOW_SAMPLES {
            self.samples.pop_front();
        }
        // 窗口未满（<30s）不调整（防启动期噪声）
        if self.samples.len() < WINDOW_SAMPLES {
            return;
        }
        let covered_n = self.samples.iter().filter(|&&c| c).count();
        let ratio = covered_n as f64 / self.samples.len() as f64;

        if ratio >= COVER_RATIO_SHRINK {
            // 持续覆盖 → 缩短一级（60→10→3，rank 下限 0）
            self.stable_secs = 0;
            self.rank = self.rank.saturating_sub(1);
        } else if covered_n == 0 {
            // 窗口内零覆盖 → 累计稳定时长，≥60s 延长一级（10→60→300）
            self.stable_secs += SAMPLE_INTERVAL_SECS;
            if self.stable_secs >= STABLE_SECS_EXTEND {
                self.stable_secs = 0;
                if self.rank < INTERVALS_SECS.len() - 1 {
                    self.rank += 1;
                }
            }
        } else {
            // 部分覆盖但未达阈值：不调整，重置稳定累计
            self.stable_secs = 0;
        }
    }

    /// 观测（覆盖比例 0.0~1.0）——daemon 采样后用比例判断。
    /// 便于直接喂 `sample_coverage` 的输出。
    pub fn observe_ratio(&mut self, ratio: f64) {
        self.observe(ratio >= COVER_RATIO_SHRINK);
    }
}

impl Default for AdaptiveRelock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── RelockGuard ──

    #[test]
    fn guard_first_call_passes() {
        let mut g = RelockGuard::new();
        assert!(g.try_lock(Instant::now()), "初始无冷却应放行");
    }

    #[test]
    fn guard_cooldown_blocks_then_expires() {
        let mut g = RelockGuard::new();
        g.set_cooldown(1000);
        let t0 = Instant::now();
        assert!(g.try_lock(t0));
        assert!(!g.try_lock(t0 + Duration::from_millis(500)), "冷却内拒绝");
        assert!(g.try_lock(t0 + Duration::from_millis(1000)), "冷却到期放行");
    }

    #[test]
    fn guard_cooldown_zero_always_passes() {
        let mut g = RelockGuard::new();
        g.set_cooldown(0);
        let t0 = Instant::now();
        assert!(g.try_lock(t0));
        assert!(g.try_lock(t0 + Duration::from_millis(1)), "cooldown=0 恒放行");
    }

    // ── AdaptiveRelock ──

    #[test]
    fn adaptive_default_60s() {
        assert_eq!(AdaptiveRelock::new().interval_secs(), 60);
    }

    #[test]
    fn adaptive_window_not_full_no_adjust() {
        let mut a = AdaptiveRelock::new();
        // 窗口未满（<6 采样）→ 持续覆盖也不调整
        for _ in 0..3 {
            a.observe(true);
        }
        assert_eq!(a.interval_secs(), 60, "窗口未满不缩短");
    }

    #[test]
    fn adaptive_sustained_coverage_shrinks_to_3s() {
        let mut a = AdaptiveRelock::new();
        // 窗口满 + 持续覆盖 → 60 → 10 → 3
        for _ in 0..6 {
            a.observe(true);
        }
        assert_eq!(a.interval_secs(), 10, "第一轮持续覆盖缩短到 10s");
        for _ in 0..6 {
            a.observe(true);
        }
        assert_eq!(a.interval_secs(), 3, "第二轮持续覆盖到 3s 下限");
        for _ in 0..6 {
            a.observe(true);
        }
        assert_eq!(a.interval_secs(), 3, "已在下限不再缩短");
    }

    #[test]
    fn adaptive_single_coverage_no_adjust() {
        let mut a = AdaptiveRelock::new();
        // 窗口内 1/6 覆盖（<50%）→ 不缩短；且重置稳定累计
        for _ in 0..5 {
            a.observe(false);
        }
        a.observe(true);
        assert_eq!(a.interval_secs(), 60, "单次覆盖不缩短");
    }

    #[test]
    fn adaptive_stable_extends_to_300s() {
        let mut a = AdaptiveRelock::new();
        // 先缩短到 3s
        for _ in 0..12 {
            a.observe(true);
        }
        assert_eq!(a.interval_secs(), 3);
        // 清空窗口（6 次 false 把旧 true 挤出）+ 稳定 60s（12 次）→ 3 → 10
        // 注意：混合窗口（新旧 true 并存）会重置 stable_secs——设计行为
        for _ in 0..18 {
            a.observe(false);
        }
        assert_eq!(a.interval_secs(), 10, "清窗+稳定 60s 延长一级");
        for _ in 0..12 {
            a.observe(false);
        }
        assert_eq!(a.interval_secs(), 60, "再稳定 60s 回到 60");
        for _ in 0..12 {
            a.observe(false);
        }
        assert_eq!(a.interval_secs(), 300, "最终到 300s 上限");
        for _ in 0..12 {
            a.observe(false);
        }
        assert_eq!(a.interval_secs(), 300, "已在上限不再延长");
    }

    #[test]
    fn adaptive_ratio_api() {
        let mut a = AdaptiveRelock::new();
        for _ in 0..6 {
            a.observe_ratio(0.8); // 80% 覆盖
        }
        assert_eq!(a.interval_secs(), 10);
    }
}
