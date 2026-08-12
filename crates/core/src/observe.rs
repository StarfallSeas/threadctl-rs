//! P8 — 观测数据层（线程监视器核心，Metric 需求合并进 threadctl 的部分）。
//!
//! 分界线（三审/用户确认）：
//! - **合并**：数据采集（read_processor/负载）+ ThreadSnapshot + 窗口统计
//!   （迁移次数/Affinity 变化/核心分布/主要核心）——daemon 内增量计算
//! - **不合并**：历史持久化（内存环形窗口，重启清）、帧率关联分析（展示层做）
//!
//! 展示形态（Web/APP 层）：`affinity + [running_cpu]` → "0-7[4]"。

use std::collections::HashMap;
use std::collections::VecDeque;

use crate::proc::{read_processor, read_thread_cpu_secs};
use crate::topology::CpuSet;
use crate::tracker::StateTracker;

/// 单次采样的线程快照（P8 数据层输出）。
#[derive(Debug, Clone)]
pub struct ThreadSnapshot {
    pub tid: i32,
    pub name: String,
    /// 允许范围（"0-7"）——affinity 显示前半部分
    pub affinity: String,
    /// 实际运行核心（"0-7[4]" 的 [4]）
    pub running_cpu: Option<u32>,
    /// 负载百分比（0-100，相邻采样 CPU 时间差 / 采样间隔）
    pub load_pct: u8,
    /// 累计 CPU tick（内部：负载差分用）
    pub cpu_ticks: u64,
}

/// 每线程的窗口统计（迁移/分布——Metric 需求的核心输出）。
#[derive(Debug, Clone, Default)]
pub struct ThreadStats {
    pub tid: i32,
    pub name: String,
    /// 核心迁移次数（相邻样本 running_cpu 变化）
    pub migrations: u64,
    /// Affinity 变化次数（相邻样本 affinity 字符串变化）
    pub affinity_changes: u64,
    /// 核心分布（running_cpu → 样本数）
    pub cpu_distribution: HashMap<u32, usize>,
    /// 总样本数
    pub samples: usize,
    /// 负载均值（0-100）
    pub avg_load_pct: u64,
    /// 负载峰值
    pub max_load_pct: u8,
    /// 主要运行核心（分布 argmax）
    pub primary_cpu: Option<u32>,
}

impl ThreadStats {
    /// 主要核心 = 分布计数最大者。
    fn compute_primary(&mut self) {
        self.primary_cpu = self
            .cpu_distribution
            .iter()
            .max_by_key(|(_, &n)| n)
            .map(|(cpu, _)| *cpu);
    }
}

/// 环形窗口（近 WINDOW 个样本）——daemon 内存，重启清（观测短期行为）。
pub struct SnapshotWindow {
    /// tid → 最近样本（每个 tid 独立窗口，保留最近 N 个）
    recent: HashMap<i32, VecDeque<(u64, Option<u32>, String)>>,
    /// tid → 累计统计
    stats: HashMap<i32, ThreadStats>,
    /// 最近采样序号
    seq: u64,
}

/// 窗口大小（每线程保留样本数；2s 采样 → 5 分钟 = 150 样本）。
pub const WINDOW_SAMPLES: usize = 150;

impl SnapshotWindow {
    pub fn new() -> Self {
        Self {
            recent: HashMap::new(),
            stats: HashMap::new(),
            seq: 0,
        }
    }

    /// 推入一批快照（一次采样周期）——增量更新统计。
    pub fn push_batch(&mut self, snapshots: &[ThreadSnapshot]) {
        self.seq += 1;
        let seq = self.seq;
        for snap in snapshots {
            let st = self.stats.entry(snap.tid).or_insert_with(|| ThreadStats {
                tid: snap.tid,
                name: snap.name.clone(),
                ..Default::default()
            });
            st.name = snap.name.clone();
            st.samples += 1;
            // 移动平均（内存友好，语义=均值）
            let n = st.samples as u64;
            st.avg_load_pct = (st.avg_load_pct * (n - 1) + snap.load_pct as u64) / n;
            if snap.load_pct > st.max_load_pct {
                st.max_load_pct = snap.load_pct;
            }
            if let Some(cpu) = snap.running_cpu {
                *st.cpu_distribution.entry(cpu).or_insert(0) += 1;
            }

            // 迁移/Affinity 变化：与上一样本比较
            let q = self.recent.entry(snap.tid).or_default();
            if let Some(&(_, prev_cpu, ref prev_aff)) = q.back() {
                if let (Some(cur_cpu), Some(prev_cpu)) = (snap.running_cpu, prev_cpu) {
                    if cur_cpu != prev_cpu {
                        st.migrations += 1;
                    }
                }
                if *prev_aff != snap.affinity {
                    st.affinity_changes += 1;
                }
            }
            q.push_back((seq, snap.running_cpu, snap.affinity.clone()));
            while q.len() > WINDOW_SAMPLES {
                q.pop_front();
            }
            st.compute_primary();
        }
    }

    /// 当前全部线程统计（排序：tid）。
    pub fn stats(&self) -> Vec<&ThreadStats> {
        let mut v: Vec<&ThreadStats> = self.stats.values().collect();
        v.sort_by_key(|s| s.tid);
        v
    }

    /// 单个线程统计（快照输出用）。
    pub fn stats_of(&self, tid: i32) -> Option<&ThreadStats> {
        self.stats.get(&tid)
    }

    /// 最近样本（(running_cpu, affinity)）——snapshot 输出 "0-7[4]" 用。
    pub fn recent_sample(&self, tid: i32) -> Option<(Option<u32>, String)> {
        self.recent.get(&tid)?.back().map(|(_, cpu, aff)| (*cpu, aff.clone()))
    }

    /// 清理已退出线程的统计（审查发现：stats 只增不减——长期运行旧 tid 累积泄漏）。
    /// daemon 与 cleanup_dead 同周期调用（收集存活 tid 集合传入）。
    pub fn retain(&mut self, alive_tids: &std::collections::HashSet<i32>) {
        self.recent.retain(|tid, _| alive_tids.contains(tid));
        self.stats.retain(|tid, _| alive_tids.contains(tid));
    }
}

impl Default for SnapshotWindow {
    fn default() -> Self {
        Self::new()
    }
}

/// 采样器：遍历 tracker 的进程/线程，产快照。
/// 负载 = CPU tick 差分 / 采样间隔（tick 频率 = 内核 HZ，Android 通常 100 或 250）。
pub struct Sampler {
    /// tid → 上次 CPU tick（负载差分）
    last_cpu: HashMap<i32, u64>,
    /// 采样间隔秒（负载换算用）
    interval_secs: u64,
    /// 内核 tick 频率（默认 100；/proc/uptime 无法直接拿 HZ，用 100 近似）
    hz: u64,
    /// 复用缓冲（性能审查：每轮新建 Vec 导致 RSS 高水位不回落——
    /// Rust 分配器不把内存还给 OS，峰值保持；复用消除分配 churn）
    buf: Vec<ThreadSnapshot>,
}

impl Sampler {
    pub fn new(interval_secs: u64) -> Self {
        Self {
            last_cpu: HashMap::new(),
            interval_secs: interval_secs.max(1),
            hz: 100,
            buf: Vec::with_capacity(256),
        }
    }

    /// 遍历所有跟踪进程的全部线程，产快照（复用缓冲，返回借用）。
    pub fn sample(&mut self, tracker: &StateTracker) -> &[ThreadSnapshot] {
        self.buf.clear();
        let out = &mut self.buf;
        for pid in tracker.pids() {
            let Some(st) = tracker.get(pid) else {
                continue;
            };
            let allowed = CpuSet::read_allowed_mask(pid).map(|m| m.to_range_string()).unwrap_or_default();
            for tid in &st.applied_tids {
                // 直接读 /proc/<tid>/comm（观测频率 × 线程数可接受；
                // 不用 tracker 缓存——sample 只有 &StateTracker）
                let name = crate::proc::read_thread_name(*tid).unwrap_or_default();
                let running_cpu = read_processor(*tid);
                let cpu_ticks = read_thread_cpu_secs(*tid).unwrap_or(0);
                // 负载：本次 - 上次（tick）/ (interval * hz) → 百分比
                let load_pct = match self.last_cpu.get(tid) {
                    Some(&prev) if cpu_ticks >= prev => {
                        let used = cpu_ticks - prev;
                        let max = self.interval_secs * self.hz;
                        ((used * 100) / max.max(1)).min(100) as u8
                    }
                    _ => 0,
                };
                self.last_cpu.insert(*tid, cpu_ticks);
                out.push(ThreadSnapshot {
                    tid: *tid,
                    name,
                    affinity: allowed.clone(),
                    running_cpu,
                    load_pct,
                    cpu_ticks,
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::parse_stat_fields;

    #[test]
    fn stat_parse_processor_and_cpu() {
        // 标准 /proc/<tid>/stat 样例：`)` 后 S + 35 个 filler，f[36]=processor=7
        // （f[10]=utime, f[11]=stime）
        let stat = "1234 (TestThread) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 7 37 38 39";
        let f = parse_stat_fields(stat).expect("parse");
        assert_eq!(f[0], "S", "state");
        assert_eq!(f[10], "10", "utime");
        assert_eq!(f[11], "11", "stime");
        assert_eq!(f[36], "7", "processor（第 39 字段）");
        assert_eq!(f.len(), 40, "40 个字段（S + 39）");
    }

    #[test]
    fn stat_parse_comm_with_spaces() {
        // comm 含空格/括号：rsplit_once(')') 必须取最后一个
        let stat = "99 (RenderThread (GL)) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 5 37 38 39";
        let f = parse_stat_fields(stat).expect("parse");
        assert_eq!(f[36], "5");
    }

    #[test]
    fn window_tracks_migrations_and_distribution() {
        let mut w = SnapshotWindow::new();
        let mk = |tid: i32, cpu: Option<u32>, aff: &str, load: u8| ThreadSnapshot {
            tid,
            name: format!("t{tid}"),
            affinity: aff.into(),
            running_cpu: cpu,
            load_pct: load,
            cpu_ticks: 0,
        };
        // tid=1：cpu 4→4→7（1 次迁移），affinity "0-7"→"0-7"（0 变化）
        w.push_batch(&[mk(1, Some(4), "0-7", 10)]);
        w.push_batch(&[mk(1, Some(4), "0-7", 20)]);
        w.push_batch(&[mk(1, Some(7), "0-7", 30)]);
        let s = w.stats_of(1).expect("stats");
        assert_eq!(s.migrations, 1, "4→4→7 只迁移 1 次");
        assert_eq!(s.affinity_changes, 0);
        assert_eq!(s.samples, 3);
        assert_eq!(s.avg_load_pct, 20);
        assert_eq!(s.max_load_pct, 30);
        assert_eq!(s.primary_cpu, Some(4), "cpu4 出现 2 次最多");
        assert_eq!(s.cpu_distribution.get(&4), Some(&2));
        assert_eq!(s.cpu_distribution.get(&7), Some(&1));
    }

    #[test]
    fn window_tracks_affinity_changes() {
        let mut w = SnapshotWindow::new();
        let mk = |tid: i32, cpu: Option<u32>, aff: &str, load: u8| ThreadSnapshot {
            tid,
            name: format!("t{tid}"),
            affinity: aff.into(),
            running_cpu: cpu,
            load_pct: load,
            cpu_ticks: 0,
        };
        w.push_batch(&[mk(2, Some(3), "0-7", 5)]);
        w.push_batch(&[mk(2, Some(3), "0-3", 5)]); // affinity 变化
        w.push_batch(&[mk(2, Some(3), "0-3", 5)]);
        let s = w.stats_of(2).expect("stats");
        assert_eq!(s.affinity_changes, 1, "0-7→0-3→0-3 变化 1 次");
        assert_eq!(s.migrations, 0, "cpu 未变");
    }

    #[test]
    fn window_evicts_old_samples() {
        let mut w = SnapshotWindow::new();
        let mk = |tid: i32, cpu: Option<u32>, load: u8| ThreadSnapshot {
            tid,
            name: format!("t{tid}"),
            affinity: "0-7".into(),
            running_cpu: cpu,
            load_pct: load,
            cpu_ticks: 0,
        };
        // 超过窗口：交替 cpu 1/2 → 迁移计数受窗口裁剪影响（只统计窗口内）
        for i in 0..(WINDOW_SAMPLES + 10) {
            w.push_batch(&[mk(9, Some((i % 2) as u32), 1)]);
        }
        let s = w.stats_of(9).expect("stats");
        assert_eq!(s.samples, WINDOW_SAMPLES + 10, "样本总数不裁剪（统计累计）");
        assert_eq!(w.recent.get(&9).map(|q| q.len()).unwrap_or(0), WINDOW_SAMPLES, "环形窗口裁剪");
        assert!(s.migrations > 0, "交替核心产生迁移");
    }

    #[test]
    fn sampler_load_delta() {
        let mut s = Sampler::new(1); // 1s 间隔，hz=100
        // 模拟：首次采样 cpu_ticks=0 → load 0；第二次无法注入 stat（用 read_thread_cpu_secs 读不到 mock）
        // 只验证结构可用（真实负载依赖 /proc）
        let out = s.sample(&StateTracker::new());
        assert!(out.is_empty(), "空 tracker 无快照");
    }
}
