//! ProcSource — /proc event source (`EventSource` implementation).
//!
//! Discovery strategy (evolution of 既有实现 proc_mode):
//! - process count change → full scan (new whitelisted process discovery → Fork events)
//! - stable process count → incremental path: only check tracked processes
//!   (alive + thread delta → ThreadClone)
//! - vanished processes → Exit events
//!
//! Baseline state (applied tid sets) lives in the shared `StateTracker`, shared with the engine.

use std::collections::HashSet;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use threadctl_core::config::ConfigSnapshot;
use threadctl_core::event::{EventSource, ProcessEvent};
use threadctl_core::proc::{list_tids, read_cmdline};
use threadctl_core::tracker::StateTracker;

/// Zygote pending 退避间隔（ChatGPT P6.2 建议：100ms/300ms/1s 指数退避，
/// 比固定 200ms 更符合 Android Zygote 行为——fork 后 cmdline 延迟填充）。
const PENDING_BACKOFF_MS: [u64; 3] = [100, 300, 1000];
const PENDING_MAX_RETRIES: u8 = 3;
/// pending 上限：防止内核线程（cmdline 也为空）风暴占满队列。
const PENDING_MAX_PENDING: usize = 64;

/// 等待 cmdline 填充的进程（Zygote fork 空窗）。
struct PendingProcess {
    pid: i32,
    retry_count: u8,
    next_retry: Instant,
}

pub struct ProcSource {
    cfg: Option<Arc<ConfigSnapshot>>,
    tracker: Arc<Mutex<StateTracker>>,
    /// 上次 sysinfo 进程总数；变化触发全量扫描。
    last_proc_total: i32,
    /// 配置变更/降级后强制全量扫描。
    scan_all: bool,
    /// Zygote pending 队列（P6.2-3）。
    pending: Vec<PendingProcess>,
}

impl ProcSource {
    pub fn new(tracker: Arc<Mutex<StateTracker>>) -> Self {
        Self {
            cfg: None,
            tracker,
            last_proc_total: -1,
            scan_all: true,
            pending: Vec::new(),
        }
    }

    /// 处理 Zygote pending：指数退避重读 cmdline，成功则产 Fork 事件。
    /// 重试耗尽或进程已死 → 丢弃（全扫兜底）。
    fn flush_pending(&mut self, rules: &threadctl_core::ruleset::RuleSet, events: &mut Vec<ProcessEvent>) {
        if self.pending.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut kept: Vec<PendingProcess> = Vec::with_capacity(self.pending.len());
        for p in std::mem::take(&mut self.pending) {
            if now < p.next_retry {
                kept.push(p);
                continue;
            }
            if !threadctl_core::proc::is_alive(p.pid) {
                continue; // 进程已死，丢弃
            }
            match read_cmdline(p.pid) {
                Some(pkg) if !pkg.is_empty() => {
                    if rules.is_interested(&pkg) {
                        events.push(ProcessEvent::fork(p.pid, p.pid).with_pkg(pkg));
                    }
                    // 成功（无论是否白名单）→ 出队
                }
                _ => {
                    if p.retry_count < PENDING_MAX_RETRIES {
                        let delay = PENDING_BACKOFF_MS[p.retry_count as usize];
                        kept.push(PendingProcess {
                            pid: p.pid,
                            retry_count: p.retry_count + 1,
                            next_retry: now + Duration::from_millis(delay),
                        });
                    }
                    // 重试耗尽 → 丢弃（全扫兜底）
                }
            }
        }
        self.pending = kept;
    }

    /// 单轮收集：产出发现的事件。
    fn collect(&mut self) -> Vec<ProcessEvent> {
        // clone Arc：避免 &self.cfg 借用与 &mut self（flush_pending）冲突
        let Some(cfg) = self.cfg.clone() else {
            return Vec::new();
        };
        let rules = &cfg.rules;
        let mut events = Vec::new();

        // P6.2-3：先处理 Zygote pending（指数退避重读 cmdline）
        self.flush_pending(rules, &mut events);

        // Android 专项审查 🔴A：Bionic 的 sysinfo.procs 返回**任务数（含线程）**
        // （SM8550 实测 1000+），线程创建/退出是常态 → procs 每轮必变 → 每轮全扫。
        // 改用 /proc 目录项计数 = 真实进程数。
        let proc_total = count_processes();
        // H3 修复（Claude Q4）：仅"真实进程数净增加超过阈值"触发全扫。
        // 线程增删走增量路径 + ThreadClone/Exit 事件，不触发全扫。
        const FULL_SCAN_THRESHOLD: i32 = 5;
        let need_full = self.scan_all
            || (proc_total > self.last_proc_total + FULL_SCAN_THRESHOLD
                && self.last_proc_total >= 0)
            || proc_total == -1;
        self.last_proc_total = proc_total;
        self.scan_all = false;

        let mut current_pids: HashSet<i32> = HashSet::new();

        if need_full {
            let Ok(dir) = fs::read_dir("/proc") else {
                return events;
            };
            for entry in dir.flatten() {
                let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                    continue;
                };
                current_pids.insert(pid);
                // P6.2-3：Zygote fork 空窗——fork 后 cmdline 短暂为空。
                // 空/读失败 → 入 pending 队列（指数退避重试），不直接丢弃。
                let Some(pkg) = read_cmdline(pid) else {
                    // BUG-M4 修复 (Claude)：全扫发现非跟踪 pid 且 cmdline 空时，
                    // 必须检查是否已在 pending 队列（flush_pending 保留项 + 重试）
                    if self.pending.len() < PENDING_MAX_PENDING
                        && !self.tracker.lock().unwrap_or_else(|e| e.into_inner()).contains(pid)
                        && !self.pending.iter().any(|p| p.pid == pid)
                    {
                        self.pending.push(PendingProcess {
                            pid,
                            retry_count: 0,
                            next_retry: Instant::now(),
                        });
                    }
                    continue;
                };
                if pkg.is_empty() {
                    if self.pending.len() < PENDING_MAX_PENDING
                        && !self.tracker.lock().unwrap_or_else(|e| e.into_inner()).contains(pid)
                        && !self.pending.iter().any(|p| p.pid == pid)
                    {
                        self.pending.push(PendingProcess {
                            pid,
                            retry_count: 0,
                            next_retry: Instant::now(),
                        });
                    }
                    continue;
                }
                if !rules.is_interested(&pkg) {
                    continue;
                }
                let tracker = self.tracker.lock().unwrap_or_else(|e| e.into_inner());
                let is_new = !tracker.contains(pid);
                let new_tids = if is_new {
                    Vec::new()
                } else {
                    new_tids_for(&tracker, pid)
                };
                drop(tracker);

                if is_new {
                    events.push(ProcessEvent::fork(pid, pid).with_pkg(pkg));
                } else {
                    for tid in new_tids {
                        events.push(ProcessEvent::thread_clone(pid, tid).with_pkg(pkg.clone()));
                    }
                }
            }
        } else {
            // 增量路径：只检查已跟踪进程的线程增量。
            let pids = self.tracker.lock().unwrap_or_else(|e| e.into_inner()).pids();
            for pid in pids {
                if !threadctl_core::proc::is_alive(pid) {
                    continue; // 死进程，下方 Exit 检测处理
                }
                current_pids.insert(pid);
                let Some(pkg) = read_cmdline(pid) else { continue };
                if !rules.is_interested(&pkg) {
                    continue;
                }
                let tracker = self.tracker.lock().unwrap_or_else(|e| e.into_inner());
                let new_tids = new_tids_for(&tracker, pid);
                drop(tracker);
                for tid in new_tids {
                    events.push(ProcessEvent::thread_clone(pid, tid).with_pkg(pkg.clone()));
                }
            }
        }

        // Exit 检测：被跟踪但已不在当前进程表中的。
        let tracked = self.tracker.lock().unwrap_or_else(|e| e.into_inner()).pids();
        for pid in tracked {
            if !current_pids.contains(&pid) {
                events.push(ProcessEvent::exit(pid, pid));
            }
        }

        events
    }
}

/// 真实进程数：/proc 下纯数字目录项计数（Android 专项 🔴A——
/// Bionic sysinfo.procs 是线程数，不可用于进程增量判断）。
fn count_processes() -> i32 {
    fs::read_dir("/proc")
        .map(|d| {
            d.flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .chars()
                        .all(|c| c.is_ascii_digit())
                })
                .count() as i32
        })
        .unwrap_or(-1)
}

/// 计算进程的新增线程（当前 tids − 已应用 tids）。
/// 进程尚未扫描过（applied_tids 为空）时不产事件，由 Fork 事件全扫接管。
fn new_tids_for(tracker: &StateTracker, pid: i32) -> Vec<i32> {
    let Some(state) = tracker.get(pid) else {
        return Vec::new();
    };
    if state.applied_tids.is_empty() || !state.initial_scan_done {
        return Vec::new();
    }
    let tids_now: HashSet<i32> = list_tids(pid).into_iter().collect();
    tids_now.difference(&state.applied_tids).copied().collect()
}

impl EventSource for ProcSource {
    fn poll(&mut self, deadline: Instant) -> Vec<ProcessEvent> {
        let events = self.collect();
        if events.is_empty() {
            // 空轮：休眠至 deadline，维持轮询节奏。
            let now = Instant::now();
            if now < deadline {
                std::thread::sleep(deadline - now);
            }
        }
        events
    }

    fn on_config_changed(&mut self, cfg: &Arc<ConfigSnapshot>) {
        self.cfg = Some(cfg.clone());
        self.scan_all = true;
        self.last_proc_total = -1;
    }

    fn shutdown(&mut self) {}
}
