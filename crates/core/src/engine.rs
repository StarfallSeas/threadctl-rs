//! Event engine — the convergence point of event → rule matching → policy execution.
//!
//! `handle_events` is the P2 pipeline throat: every event from any source
//! (proc/eBPF) is resolved here. Pure logic + unit-testable (apply side effects
//! are concentrated in `policy::apply_thread`).
//!
//! Borrowing discipline: `refresh_process_rules` uses short borrows internally
//! (`mem::replace` to take the cache out); the outer scope must not hold a long
//! borrow from `tracker.enter` across function calls.

use std::collections::HashSet;

use crate::caps::can_rt_sched;
use crate::config::ConfigSnapshot;
use crate::event::{EventKind, ProcessEvent};
use crate::policy::{self, ApplyOutcome};
use crate::proc::{list_tids, read_cmdline};
use crate::topology::CpuTopology;
use crate::tracker::{StateTracker, ThreadNameCache};

/// 线程级刷新间隔：Fork/ThreadClone 后全线程扫描的最小间隔。
const THREAD_SCAN_TTL_SECS: i64 = 2;

/// 处理一批事件，返回成功 apply 次数（telemetry 用）。
pub fn handle_events(
    tracker: &mut StateTracker,
    events: &[ProcessEvent],
    cfg: &ConfigSnapshot,
    topo: &CpuTopology,
    now: i64,
) -> usize {
    let rt_allowed = can_rt_sched();
    let mut applied = 0;
    for ev in events {
        applied += handle_event(tracker, ev, cfg, topo, now, rt_allowed);
    }
    applied
}

fn handle_event(
    tracker: &mut StateTracker,
    ev: &ProcessEvent,
    cfg: &ConfigSnapshot,
    topo: &CpuTopology,
    now: i64,
    rt_allowed: bool,
) -> usize {
    match ev.kind {
        EventKind::Exit => {
            tracker.remove(ev.pid);
            0
        }
        EventKind::CpuMigrate => 0, // P5
        EventKind::Fork | EventKind::ThreadClone | EventKind::Exec => {
            // 事件源可能已附 pkg（proc），否则回退读 /proc（eBPF）。
            let pkg = ev
                .pkg
                .clone()
                .or_else(|| read_cmdline(ev.pid))
                .unwrap_or_default();
            if pkg.is_empty() {
                return 0;
            }
            if !cfg.rules.is_interested(&pkg) {
                // 不再感兴趣（配置变更后残留）→ 移除跟踪。
                if tracker.contains(ev.pid) {
                    tracker.remove(ev.pid);
                }
                return 0;
            }

            match ev.kind {
                EventKind::Exec => {
                    // exec 替换映像：清缓存 + 强制全线程重扫。
                    {
                        let state = tracker.get_mut(ev.pid);
                        if let Some(s) = state {
                            s.tid_names.clear();
                            s.initial_scan_done = false;
                            s.last_scan_time = 0;
                            s.applied_tids.clear();
                        }
                    }
                    let (n, dirs) =
                        refresh_process_rules(tracker, ev.pid, &pkg, cfg, now, rt_allowed, topo);
                    tracker.register_dirs(ev.pid, &dirs);
                    mark_scanned(tracker, ev.pid, now);
                    n
                }
                EventKind::Fork => {
                    let (n, dirs) =
                        refresh_process_rules(tracker, ev.pid, &pkg, cfg, now, rt_allowed, topo);
                    tracker.register_dirs(ev.pid, &dirs);
                    mark_scanned(tracker, ev.pid, now);
                    n
                }
                _ => {
                    // ThreadClone：TTL 内增量应用单个 tid，到期全线程重扫。
                    let need_full = tracker.get(ev.pid).map_or(true, |s| {
                        !s.initial_scan_done || now - s.last_scan_time >= THREAD_SCAN_TTL_SECS
                    });
                    if need_full {
                        let (n, dirs) = refresh_process_rules(
                            tracker, ev.pid, &pkg, cfg, now, rt_allowed, topo,
                        );
                        tracker.register_dirs(ev.pid, &dirs);
                        mark_scanned(tracker, ev.pid, now);
                        n
                    } else {
                        apply_single_tid(tracker, ev.pid, &pkg, ev.tid, cfg, topo, now, rt_allowed)
                    }
                }
            }
        }
    }
}

/// 标记进程已完成首轮全线程扫描。
fn mark_scanned(tracker: &mut StateTracker, pid: i32, now: i64) {
    if let Some(s) = tracker.get_mut(pid) {
        s.initial_scan_done = true;
        s.last_scan_time = now;
    }
}

/// 全线程扫描：枚举 /proc/<pid>/task，匹配规则并应用。
/// 返回 (apply 成功数, 用到的 cpuset 目录)。
fn refresh_process_rules(
    tracker: &mut StateTracker,
    pid: i32,
    pkg: &str,
    cfg: &ConfigSnapshot,
    now: i64,
    rt_allowed: bool,
    topo: &CpuTopology,
) -> (usize, Vec<String>) {
    let tids = list_tids(pid);
    let has_thread_rules = cfg.rules.has_thread_rules(pkg);

    // 取出线程名缓存（短借用，避免跨循环持有 tracker 借用）。
    let mut tid_names = {
        let state = tracker.enter(pid, pkg.to_string(), now);
        std::mem::replace(&mut state.tid_names, ThreadNameCache::new(now))
    };

    let mut applied = 0;
    let mut dirs: Vec<String> = Vec::new();
    let tid_set: HashSet<i32> = tids.iter().copied().collect();

    for tid in &tids {
        let tname = if has_thread_rules {
            tid_names.get_or_read(*tid, now).to_string()
        } else {
            String::new()
        };
        if let Some(policy) = cfg.rules.resolve(pkg, &tname) {
            let outcome = policy::apply_thread(*tid, pkg, &policy, topo, rt_allowed);
            // Claude 审查 Bug 3：SkippedNoCpus（占位规则只应用 sched）不计数 applied
            match outcome {
                ApplyOutcome::Exited => continue, // 线程已退出，下次全扫收敛
                ApplyOutcome::SkippedNoCpus => {} // sched 已应用，affinity 跳过，不计数
                _ => {
                    applied += 1;
                    if !dirs.contains(&policy.cpuset_dir) {
                        dirs.push(policy.cpuset_dir.clone());
                    }
                }
            }
        }
    }

    // 归还缓存并收缩。
    {
        let state = tracker.enter(pid, pkg.to_string(), now);
        state.tid_names = tid_names;
        state.tid_names.retain(&tid_set);
        state.applied_tids = tid_set;
    }

    (applied, dirs)
}

/// 单 tid 增量应用（ThreadClone 事件，TTL 内）。
fn apply_single_tid(
    tracker: &mut StateTracker,
    pid: i32,
    pkg: &str,
    tid: i32,
    cfg: &ConfigSnapshot,
    topo: &CpuTopology,
    now: i64,
    rt_allowed: bool,
) -> usize {
    let policy = {
        let state = tracker.enter(pid, pkg.to_string(), now);
        if state.applied_tids.contains(&tid) {
            return 0; // 已应用过
        }
        let tname = if cfg.rules.has_thread_rules(pkg) {
            state.tid_names.get_or_read(tid, now).to_string()
        } else {
            String::new()
        };
        match cfg.rules.resolve(pkg, &tname) {
            Some(p) => p,
            None => {
                state.applied_tids.insert(tid);
                return 0;
            }
        }
    };

    let outcome = policy::apply_thread(tid, pkg, &policy, topo, rt_allowed);
    // Claude 审查 Bug 3：SkippedNoCpus 不计数（只应用 sched）
    if outcome == ApplyOutcome::Exited || outcome == ApplyOutcome::SkippedNoCpus {
        return 0;
    }

    let dir = policy.cpuset_dir.clone();
    {
        let state = tracker.get_mut(pid);
        if let Some(s) = state {
            s.applied_tids.insert(tid);
        }
    }
    tracker.register_dirs(pid, &[dir]);
    1
}

/// 周期重锁定（relock）：对抗 Android AMS/cgroup 覆盖。
/// 遍历全部被跟踪进程做全线程刷新；getaffinity 短路保证空转开销小。
///
/// Claude 审查 2.2：跳过后台/缓存进程（oom_adj > 500）——
/// 后台应用不需要重新绑性能核，让 Linux scheduler 自然决定更省电；
/// 且减少无效 setaffinity/cpuset 写入。前台/感知服务照常 relock。
pub fn relock_all(
    tracker: &mut StateTracker,
    cfg: &ConfigSnapshot,
    topo: &CpuTopology,
    now: i64,
) -> usize {
    use crate::decision::TaskIntent;
    use crate::proc::read_oom_adj;

    let rt_allowed = can_rt_sched();
    let pids = tracker.pids();
    let mut applied = 0;
    for pid in pids {
        let pkg = match tracker.get(pid) {
            Some(s) => s.pkg.clone(),
            None => continue,
        };
        if unsafe { libc::kill(pid, 0) } != 0 {
            tracker.remove(pid);
            continue;
        }
        // 后台/缓存进程跳过 relock（省电 + 减少 syscall）
        let intent = TaskIntent::from_oom_adj(read_oom_adj(pid));
        if matches!(intent, TaskIntent::Background | TaskIntent::Frozen) {
            continue;
        }
        let (n, dirs) = refresh_process_rules(tracker, pid, &pkg, cfg, now, rt_allowed, topo);
        tracker.register_dirs(pid, &dirs);
        applied += n;
    }
    applied
}

/// 死进程清理：kill(pid,0) 失败的从跟踪中移除（含 cpuset 引用释放）。
pub fn cleanup_dead(tracker: &mut StateTracker) -> usize {
    let pids = tracker.pids();
    let mut removed = 0;
    for pid in pids {
        if unsafe { libc::kill(pid, 0) } != 0 && tracker.remove(pid) {
            removed += 1;
        }
    }
    removed
}

/// 当前被跟踪进程（IPC dump / 调试用）。
pub fn tracked_summary(tracker: &StateTracker) -> Vec<(i32, String, usize)> {
    tracker
        .pids()
        .into_iter()
        .filter_map(|pid| tracker.get(pid).map(|p| (pid, p.pkg.clone(), p.applied_tids.len())))
        .collect()
}
