//! StateTracker — process/thread state tracking (Q5 thread-name cache + Q3 cpuset refcounts).
//!
//! Replaces scattered process_cache / global 60s cache cleanup:
//! - Thread-name cache is **per-process** (TTL 60s, timers naturally stagger, no global
//!   cleanup spike); exec invalidates it actively, process exit clears it
//! - cpuset dir refcounts: `dir_name → refcount`, zero triggers `remove_cpuset_dir`

use std::collections::{HashMap, HashSet};

use crate::proc::{read_start_time, read_thread_name};
use crate::topology::{remove_cpuset_dir, BASE_CPUSET};

/// 线程名缓存 TTL（秒）。
const THREAD_NAME_CACHE_TTL_SECS: i64 = 60;

/// 每进程线程名缓存（Q5 定案）。
#[derive(Default)]
pub struct ThreadNameCache {
    names: HashMap<i32, String>,
    last_full_refresh: i64,
}

impl ThreadNameCache {
    pub fn new(now: i64) -> Self {
        Self { names: HashMap::new(), last_full_refresh: now }
    }

    /// 取或读线程名；TTL 到期清空本缓存（每进程粒度）。
    pub fn get_or_read(&mut self, tid: i32, now: i64) -> &str {
        if now - self.last_full_refresh >= THREAD_NAME_CACHE_TTL_SECS {
            self.names.clear();
            self.last_full_refresh = now;
        }
        self.names
            .entry(tid)
            .or_insert_with(|| read_thread_name(tid).unwrap_or_default())
    }

    /// exec 主动失效（Q5：避免旧线程名污染）。
    pub fn clear(&mut self) {
        self.names.clear();
        self.last_full_refresh = 0;
    }

    /// 进程线程集收缩时清掉失效条目。
    pub fn retain(&mut self, tids: &HashSet<i32>) {
        self.names.retain(|&tid, _| tids.contains(&tid));
    }
}

/// 单个被跟踪进程的状态。
pub struct ProcessState {
    pub pid: i32,
    pub pkg: String,
    /// 进程启动时间（/proc/pid/stat starttime），用于 PID 复用检测。
    pub start_time: u64,
    /// 是否已完成首轮全线程扫描。
    pub initial_scan_done: bool,
    /// 最近一次全线程扫描时间（单调秒）。
    pub last_scan_time: i64,
    /// 已应用的线程集合（增量检测新线程用）。
    pub applied_tids: HashSet<i32>,
    /// 本进程贡献过引用的 cpuset 目录。
    pub applied_dirs: HashSet<String>,
    pub tid_names: ThreadNameCache,
}

impl ProcessState {
    fn new(pid: i32, start_time: u64, pkg: String, now: i64) -> Self {
        Self {
            pid,
            pkg,
            start_time,
            initial_scan_done: false,
            last_scan_time: 0,
            applied_tids: HashSet::new(),
            applied_dirs: HashSet::new(),
            tid_names: ThreadNameCache::new(now),
        }
    }
}

/// 全局跟踪器。
#[derive(Default)]
pub struct StateTracker {
    procs: HashMap<i32, ProcessState>,
    /// cpuset 目录名 → 引用计数。
    cpuset_refs: HashMap<String, u32>,
}

impl StateTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, pid: i32) -> Option<&ProcessState> {
        self.procs.get(&pid)
    }

    pub fn get_mut(&mut self, pid: i32) -> Option<&mut ProcessState> {
        self.procs.get_mut(&pid)
    }

    pub fn contains(&self, pid: i32) -> bool {
        self.procs.contains_key(&pid)
    }

    pub fn pids(&self) -> Vec<i32> {
        self.procs.keys().copied().collect()
    }

    pub fn len(&self) -> usize {
        self.procs.len()
    }

    /// 进入/更新进程状态（不存在则创建）。
    ///
    /// H4 修复：已跟踪且 pkg 相同 → 零 I/O 快速返回（不再每轮读 /proc/<pid>/stat）。
    /// pkg 不同时读 start_time 做 PID 复用检测，并顺带更新 pkg（exec 后包名变化）。
    pub fn enter(&mut self, pid: i32, pkg: String, now: i64) -> &mut ProcessState {
        // 审查重构：先只读判断分支，再操作——避免持可变引用时替换
        //（原实现三处 get_mut().unwrap()，借用/维护均脆弱）。
        // 0=fast 返回现有；1=exec 更新 pkg；2=PID 复用需替换；None=新进程。
        let action = self.procs.get(&pid).map(|ex| {
            if ex.pkg == pkg || ex.start_time == 0 {
                0u8
            } else {
                let st = read_start_time(pid).unwrap_or(0);
                if st == 0 || st == ex.start_time {
                    1u8
                } else {
                    2u8
                }
            }
        });
        match action {
            Some(0) => self.procs.get_mut(&pid).expect("存在"),
            Some(1) => {
                let ex = self.procs.get_mut(&pid).expect("存在");
                ex.pkg = pkg;
                ex
            }
            Some(2) => {
                // PID 复用：旧进程已退出——走统一释放路径（cpuset 引用）
                let (_, old) = self.procs.remove_entry(&pid).expect("存在");
                self.release_state(&old);
                let st = read_start_time(pid).unwrap_or(0);
                self.procs.insert(pid, ProcessState::new(pid, st, pkg, now));
                self.procs.get_mut(&pid).expect("刚插入")
            }
            None => {
                let st = read_start_time(pid).unwrap_or(0);
                self.procs.insert(pid, ProcessState::new(pid, st, pkg, now));
                self.procs.get_mut(&pid).expect("刚插入")
            }
            Some(_) => unreachable!("enter 分支常量仅 0/1/2"),
        }
    }

    /// 线程退出（P7.1 IMPL-4）：从 applied_tids 移除单个 tid——消除 TID
    /// 复用窗口（旧线程退出后新线程复用同一 TID 会被 applied_tids 误判已应用）。
    /// 进程级退出仍走 remove()。
    pub fn remove_tid(&mut self, pid: i32, tid: i32) {
        if let Some(state) = self.procs.get_mut(&pid) {
            state.applied_tids.remove(&tid);
        }
    }

    /// 移除进程：释放其全部 cpuset 引用，归零目录 rmdir 回收。
    pub fn remove(&mut self, pid: i32) -> bool {
        let Some(state) = self.procs.remove(&pid) else {
            return false;
        };
        self.release_state(&state);
        true
    }

    /// cpuset 引用释放（remove 与 PID 复用替换共用）。
    fn release_state(&mut self, state: &ProcessState) {
        for dir in &state.applied_dirs {
            if dir.is_empty() {
                continue;
            }
            if let Some(n) = self.cpuset_refs.get_mut(dir) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    self.cpuset_refs.remove(dir);
                    let path = format!("{BASE_CPUSET}/{dir}");
                    if !remove_cpuset_dir(&path) {
                        eprintln!("{}", crate::i18n::t(format!("警告: cpuset 目录回收失败: {path}").as_str(),
                    format!("warning: cpuset dir reclaim failed: {path}").as_str()));
                    } else {
                        // Claude Q3: rmdir 后同步清除 ensure 缓存，否则下次
                        // ensure 被缓存跳过 → cpuset tasks 写入失败
                        // CLAUDE LOW-2：缓存归 backend，直接调（policy.rs 转发层已删）
                        crate::backend::forget_cpuset_dir(dir);
                        crate::debug_log!("tracker", "cpuset dir reclaimed: {path}");
                    }
                }
            }
        }
    }

    /// 登记进程新用到的 cpuset 目录（幂等：同目录只计一次）。
    pub fn register_dirs(&mut self, pid: i32, dirs: &[String]) {
        let Some(state) = self.procs.get_mut(&pid) else {
            return;
        };
        for d in dirs {
            if d.is_empty() {
                continue;
            }
            if state.applied_dirs.insert(d.clone()) {
                *self.cpuset_refs.entry(d.clone()).or_insert(0) += 1;
            }
        }
    }

    /// 记录已应用线程（增量检测基准）。
    pub fn record_applied_tids(&mut self, pid: i32, tids: &HashSet<i32>) {
        if let Some(state) = self.procs.get_mut(&pid) {
            state.applied_tids.extend(tids.iter().copied());
        }
    }

    /// 当前各 cpuset 目录引用数（IPC/调试用）。
    pub fn cpuset_refs(&self) -> &HashMap<String, u32> {
        &self.cpuset_refs
    }

    /// 清理不再被规则关注的进程（配置变更后调用）。
    pub fn retain_interested(&mut self, pkg_set: &HashSet<String>) {
        let stale: Vec<i32> = self
            .procs
            .iter()
            .filter(|(_, p)| !pkg_set.contains(&p.pkg))
            .map(|(&pid, _)| pid)
            .collect();
        for pid in stale {
            self.remove(pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_tid_clears_single_tid() {
        // P7.1 IMPL-4（CLAUDE LOW-4）：线程退出 → applied_tids 移除单个 tid，
        // 消除 TID 复用窗口（旧线程退出后新线程复用同一 TID 不再被误判已应用）。
        let mut tracker = StateTracker::new();
        tracker.enter(1, "com.a".into(), 100);
        tracker.record_applied_tids(1, &[101, 102, 103].into_iter().collect());

        // 线程 102 退出：仅移除该 tid
        tracker.remove_tid(1, 102);
        let tids: HashSet<i32> = tracker.get(1).expect("进程仍在").applied_tids.clone();
        assert!(tids.contains(&101));
        assert!(!tids.contains(&102), "线程 102 已退出，应从 applied_tids 移除");
        assert!(tids.contains(&103));

        // 不存在的 tid：无副作用
        tracker.remove_tid(1, 999);
        let tids: HashSet<i32> = tracker.get(1).expect("进程仍在").applied_tids.clone();
        assert_eq!(tids.len(), 2);

        // 不存在的进程：无副作用
        tracker.remove_tid(999, 101);
    }

    #[test]
    fn remove_tid_nonexistent_pid_is_noop() {
        // CLAUDE NEW-L2：EXIT 乱序/未知 pid → remove_tid 静默跳过，不 panic
        let mut tracker = StateTracker::new();
        tracker.remove_tid(999, 100);
        assert!(tracker.get(999).is_none());
    }

    #[test]
    fn remove_tid_nonexistent_tid_is_noop() {
        // CLAUDE NEW-L2：tid 不在 applied_tids → 集合不变
        let mut tracker = StateTracker::new();
        tracker.enter(1, "com.a".into(), 100);
        tracker.record_applied_tids(1, &[101].into_iter().collect());
        tracker.remove_tid(1, 999);
        let tids: HashSet<i32> = tracker.get(1).expect("进程仍在").applied_tids.clone();
        assert_eq!(tids.len(), 1);
        assert!(tids.contains(&101));
    }

    #[test]
    fn remove_releases_cpuset_refs() {
        let mut tracker = StateTracker::new();
        tracker.enter(1, "com.a".into(), 100);
        tracker.register_dirs(1, &["0-3".into()]);
        tracker.register_dirs(1, &["0-3".into()]); // 幂等
        assert_eq!(tracker.cpuset_refs().get("0-3"), Some(&1));

        tracker.enter(2, "com.b".into(), 100);
        tracker.register_dirs(2, &["0-3".into()]);
        assert_eq!(tracker.cpuset_refs().get("0-3"), Some(&2));

        // 移除一个进程：引用降到 1，目录不回收
        tracker.remove(1);
        assert_eq!(tracker.cpuset_refs().get("0-3"), Some(&1));

        // 移除最后一个：目录引用归零，从 map 移除（rmdir 在 termux 测试环境会失败，忽略）
        tracker.remove(2);
        assert!(tracker.cpuset_refs().get("0-3").is_none());
    }

    #[test]
    fn retain_interested_drops_unwatched() {
        let mut tracker = StateTracker::new();
        tracker.enter(1, "com.a".into(), 0);
        tracker.enter(2, "com.b".into(), 0);
        let mut keep = HashSet::new();
        keep.insert("com.a".to_string());
        tracker.retain_interested(&keep);
        assert!(tracker.contains(1));
        assert!(!tracker.contains(2));
    }

    #[test]
    fn thread_name_cache_clears_on_exec() {
        let mut cache = ThreadNameCache::new(100);
        cache.names.insert(1, "x".into());
        cache.clear();
        assert!(cache.names.is_empty());
    }
}
