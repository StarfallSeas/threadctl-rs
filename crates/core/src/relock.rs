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
///
/// **注意**：`/proc/<pid>/cpuset` 返回**挂载点相对路径**（如 `/threadctl/3-7`），
/// 不是完整路径 `/dev/cpuset/threadctl/3-7`——`starts_with(base)` 永远 false
/// 导致 coverage 恒 100%（P7.2.1 真机 bug，adaptive 被误拉到 3s 下限）。
/// 兼容两种形式：完整前缀或挂载相对前缀。
pub fn is_in_our_cpuset(pid: i32, base: &str) -> bool {
    read_cpuset_owner(pid).is_some_and(|owner| owner_is_ours(&owner, base))
}

/// 归属字符串匹配（单测用：不依赖 /proc）。
/// 路径边界：仅匹配子目录（`rel/` 前缀）——进程在 `/threadctl` 根本身
/// （非策略子目录）不算"我们的"（CLAUDE LOW-3）。
fn owner_is_ours(owner: &str, base: &str) -> bool {
    let rel = base.trim_start_matches("/dev/cpuset");
    owner.starts_with(&format!("{base}/")) || owner.starts_with(&format!("{rel}/"))
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

/// 覆盖采样周期（daemon 调用 observe 的节奏）。P7.2 后从 5s 缩到 2s——
/// D3 覆盖拉回延迟 5s→2s（每轮读跟踪进程 cpuset 微秒级，功耗可忽略）。
pub const SAMPLE_INTERVAL_SECS: u64 = 2;
/// 观察窗口 ≥30s（规划书要求）→ 2s 采样 × 15 个。
pub const WINDOW_SAMPLES: usize = 15;
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
    ///
    /// **CLAUDE BUG-H1 修复**：不能把 ratio 先转 bool 再喂 `observe`——
    /// 0 < ratio < 0.5（部分覆盖）转 false 后窗口内 covered_n==0，被误判
    /// "稳定"而延长周期到 300s（AMS 低强度持续覆盖时对抗强度反向降低）。
    /// 独立实现：稳定判定要求**当前采样 ratio == 0.0**；任何非零覆盖都重置
    /// 稳定计时（不缩也不延）。
    pub fn observe_ratio(&mut self, ratio: f64) {
        let majority = ratio >= COVER_RATIO_SHRINK;
        self.samples.push_back(majority);
        if self.samples.len() > WINDOW_SAMPLES {
            self.samples.pop_front();
        }
        if self.samples.len() < WINDOW_SAMPLES {
            return;
        }
        let covered_n = self.samples.iter().filter(|&&c| c).count();
        let window_ratio = covered_n as f64 / self.samples.len() as f64;

        if window_ratio >= COVER_RATIO_SHRINK {
            self.stable_secs = 0;
            self.rank = self.rank.saturating_sub(1);
        } else if ratio == 0.0 {
            // 关键：当前采样零覆盖才累计稳定（0<ratio<0.5 会走 else 重置）
            self.stable_secs += SAMPLE_INTERVAL_SECS;
            if self.stable_secs >= STABLE_SECS_EXTEND {
                self.stable_secs = 0;
                if self.rank < INTERVALS_SECS.len() - 1 {
                    self.rank += 1;
                }
            }
        } else {
            // 任何非零覆盖（即使 <50%）重置稳定计时——不延长周期
            self.stable_secs = 0;
        }
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
    fn owner_is_ours_mount_relative() {
        // P7.2.1 真机 bug：/proc/<pid>/cpuset 返回挂载相对路径
        assert!(owner_is_ours("/threadctl/3-7", "/dev/cpuset/threadctl"), "挂载相对路径必须命中");
        assert!(owner_is_ours("/threadctl/0-5", "/dev/cpuset/threadctl"));
    }

    #[test]
    fn owner_is_ours_full_path() {
        assert!(owner_is_ours("/dev/cpuset/threadctl/3-7", "/dev/cpuset/threadctl"), "完整路径兼容");
    }

    #[test]
    fn owner_is_ours_foreign() {
        assert!(!owner_is_ours("/top-app", "/dev/cpuset/threadctl"), "系统 cpuset 不算我们的");
        assert!(!owner_is_ours("/background", "/dev/cpuset/threadctl"));
        assert!(!owner_is_ours("/threadctl2/x", "/dev/cpuset/threadctl"), "前缀近似不误判");
        // CLAUDE LOW-3：根 cgroup 本身（非策略子目录）不算我们的
        assert!(!owner_is_ours("/threadctl", "/dev/cpuset/threadctl"), "根路径不算我们的");
        assert!(!owner_is_ours("/dev/cpuset/threadctl", "/dev/cpuset/threadctl"), "完整根路径同理");
    }

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
        // 窗口未满（<15 采样，30s）→ 持续覆盖也不调整
        for _ in 0..3 {
            a.observe(true);
        }
        assert_eq!(a.interval_secs(), 60, "窗口未满不缩短");
    }

    #[test]
    fn adaptive_sustained_coverage_shrinks_to_3s() {
        let mut a = AdaptiveRelock::new();
        // 窗口满（15 采样）+ 持续覆盖 → 60 → 10 → 3
        for _ in 0..15 {
            a.observe(true);
        }
        assert_eq!(a.interval_secs(), 10, "第一轮持续覆盖缩短到 10s");
        for _ in 0..15 {
            a.observe(true);
        }
        assert_eq!(a.interval_secs(), 3, "第二轮持续覆盖到 3s 下限");
        for _ in 0..15 {
            a.observe(true);
        }
        assert_eq!(a.interval_secs(), 3, "已在下限不再缩短");
    }

    #[test]
    fn adaptive_single_coverage_no_adjust() {
        let mut a = AdaptiveRelock::new();
        // 窗口内 1/15 覆盖（<50%）→ 不缩短；且重置稳定累计
        for _ in 0..14 {
            a.observe(false);
        }
        a.observe(true);
        assert_eq!(a.interval_secs(), 60, "单次覆盖不缩短");
    }

    #[test]
    fn adaptive_stable_extends_to_300s() {
        let mut a = AdaptiveRelock::new();
        // 先缩短到 3s（15+15+15 持续覆盖）
        for _ in 0..45 {
            a.observe(true);
        }
        assert_eq!(a.interval_secs(), 3);
        // 清空窗口（15 次 false 挤出旧 true）+ 稳定 60s（30 次 × 2s）→ 3 → 10
        for _ in 0..45 {
            a.observe(false);
        }
        assert_eq!(a.interval_secs(), 10, "清窗+稳定 60s 延长一级");
        for _ in 0..30 {
            a.observe(false);
        }
        assert_eq!(a.interval_secs(), 60, "再稳定 60s 回到 60");
        for _ in 0..30 {
            a.observe(false);
        }
        assert_eq!(a.interval_secs(), 300, "最终到 300s 上限");
        for _ in 0..30 {
            a.observe(false);
        }
        assert_eq!(a.interval_secs(), 300, "已在上限不再延长");
    }

    #[test]
    fn adaptive_ratio_api() {
        let mut a = AdaptiveRelock::new();
        for _ in 0..15 {
            a.observe_ratio(0.8); // 80% 覆盖
        }
        assert_eq!(a.interval_secs(), 10);
    }

    #[test]
    fn sub_threshold_coverage_does_not_extend() {
        // CLAUDE BUG-H1 回归：30% 持续覆盖——不缩短也不延长（稳定计时被
        // 非零覆盖重置；修复前 window_ratio=0 误判稳定延长到 300s）。
        let mut a = AdaptiveRelock::new();
        for _ in 0..60 {
            a.observe_ratio(0.3); // < 0.5 but > 0
        }
        assert_eq!(a.interval_secs(), 60, "sub-threshold coverage must not extend interval");
    }

    #[test]
    fn zero_ratio_extends_normally() {
        // ratio=0（真正稳定）→ 延长路径不受影响
        let mut a = AdaptiveRelock::from_initial(10);
        // 满窗口（15 次）+ 60s 稳定（30 次 × 2s）→ 10 → 60
        for _ in 0..45 {
            a.observe_ratio(0.0);
        }
        assert_eq!(a.interval_secs(), 60, "零覆盖稳定 60s 延长一级");
    }
}
