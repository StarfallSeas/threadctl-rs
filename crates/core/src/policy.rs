//! Policy actions — affinity + scheduling policy.

use std::collections::HashSet;
use std::fs;
use std::io::Write as _;
use std::sync::{LazyLock, Mutex};

use crate::audit::{self, AuditEntry};
use crate::topology::{create_cpuset_dir, CpuSet, CpuTopology, BASE_CPUSET};

/// 已确保存在的 cpuset 子目录缓存。
static ENSURED_CPUSET_DIRS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// EINVAL 去重：同一 tid 只告警一次（内核线程/受限线程的预期行为）。
static WARNED_EINVAL_TIDS: LazyLock<Mutex<HashSet<i32>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// cgroup 限制去重：同一 tid 只诊断一次。
static WARNED_BLOCKED_TIDS: LazyLock<Mutex<HashSet<i32>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// EPERM 去重（M2 修复）。
static WARNED_EPERM_TIDS: LazyLock<Mutex<HashSet<i32>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// cpuset 移入失败去重（问题 2 诊断：no_intersection 的根因排查）。
static WARNED_CPUSET_TIDS: LazyLock<Mutex<HashSet<i32>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// 每 tid 只告警一次的通用 helper（L4 修复：pid 空间上限防无限增长）。
fn warn_once(set: &LazyLock<Mutex<HashSet<i32>>>, tid: i32) -> bool {
    let mut guard = set.lock().unwrap_or_else(|e| e.into_inner());
    // pid 空间一般 ≤ 32768；超限清空，防止长期运行内存无限增长
    if guard.len() > 32_768 {
        guard.clear();
    }
    guard.insert(tid)
}

/// getaffinity 短路命中计数（L6 修复：区分"已符合"与"实际应用"）。
static SHORT_CIRCUIT_TOTAL: LazyLock<std::sync::atomic::AtomicU64> =
    LazyLock::new(|| std::sync::atomic::AtomicU64::new(0));

/// 读取短路命中次数。
pub fn short_circuit_total() -> u64 {
    SHORT_CIRCUIT_TOTAL.load(std::sync::atomic::Ordering::Relaxed)
}

fn ensure_cpuset_dir(topo: &CpuTopology, dir_name: &str) {
    if let Ok(mut guard) = ENSURED_CPUSET_DIRS.lock() {
        if !guard.insert(dir_name.to_string()) {
            return; // 已确保过
        }
    }
    let path = format!("{BASE_CPUSET}/{dir_name}");
    // EEXIST 由 create_cpuset_dir 内部静默处理。
    create_cpuset_dir(&path, dir_name, &topo.mems_str);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedPolicy {
    Other,
    Fifo,
    Rr,
    Batch,
    Idle,
}

impl SchedPolicy {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "other" | "normal" | "" => Some(Self::Other),
            "fifo" => Some(Self::Fifo),
            "rr" => Some(Self::Rr),
            "batch" => Some(Self::Batch),
            "idle" => Some(Self::Idle),
            _ => None,
        }
    }

    /// 是否为实时策略（需要 CAP_SYS_NICE）。
    pub fn is_rt(self) -> bool {
        matches!(self, Self::Fifo | Self::Rr)
    }

    fn to_libc(self) -> i32 {
        match self {
            // SCHED_OTHER 未在 libc crate 导出（glibc 中为 0）
            Self::Other => 0,
            Self::Fifo => libc::SCHED_FIFO,
            Self::Rr => libc::SCHED_RR,
            Self::Batch => libc::SCHED_BATCH,
            Self::Idle => libc::SCHED_IDLE,
        }
    }
}

/// 一条规则解析后的完整动作。
#[derive(Clone, Debug)]
pub struct Policy {
    pub cpus: CpuSet,
    /// 命中的 cpuset 子目录名（如 "0-3"），空串表示无 cpuset 通道。
    pub cpuset_dir: String,
    pub sched: Option<SchedPolicy>,
    pub sched_prio: Option<i32>,
    pub nice: Option<i32>,
}

/// 对单个线程应用亲和性 + cpuset + 调度策略的结果（Measure 环节输入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// 成功应用（或已符合目标零开销返回）
    Applied,
    /// 线程已退出（ESRCH），调用方应触发重扫
    Exited,
    /// 目标 CPU 被 cgroup 允许集全部排除
    BlockedByCgroup,
    /// 目标被缩减但部分生效
    Downgraded,
    /// EPERM（无 CAP_SYS_NICE 或目标受限）
    BlockedByPerm,
    /// 意外 EINVAL
    EINVAL,
    /// 其他 setaffinity 错误
    Failed,
    /// sched-only 占位规则（空掩码），亲和性部分跳过
    SkippedNoCpus,
}

/// 对单个线程应用亲和性 + cpuset + 调度策略。
///
/// `rt_allowed`：Q6 —— 无 CAP_SYS_NICE 时跳过 fifo/rr 调度（亲和性不受影响）。
/// 每次应用写入 audit 环形缓冲（Observe→Decide→Act→Measure→Adjust 闭环）。
pub fn apply_thread(
    tid: i32,
    pkg: &str,
    policy: &Policy,
    topo: &CpuTopology,
    rt_allowed: bool,
) -> ApplyOutcome {
    let outcome = apply_affinity(tid, pkg, policy, topo);

    if let Some(pol) = policy.sched {
        if pol.is_rt() && !rt_allowed {
            eprintln!("warning: tid={tid} needs RT scheduling but lacks CAP_SYS_NICE; sched skipped");
            return outcome;
        }
        apply_sched(tid, pol, policy.sched_prio, policy.nice);
    }

    outcome
}

/// 亲和性：cpuset 移入 → getaffinity → online∩allowed 交集 → setaffinity。
///
/// 顺序关键：先写 cpuset tasks 把线程移入我们的 cgroup（消除 Android foreground
/// 等系统 cpuset 限制），再读 Cpus_allowed 交集，最后 setaffinity。
fn apply_affinity(tid: i32, pkg: &str, policy: &Policy, topo: &CpuTopology) -> ApplyOutcome {
    let cpus = &policy.cpus;
    let cpuset_dir = &policy.cpuset_dir;
    let requested = cpus.to_range_string();

    // ── ⓪ sched-only 占位规则：空掩码 → 亲和性跳过（不产生误导日志）──
    if cpus.count() == 0 {
        return ApplyOutcome::SkippedNoCpus;
    }

    // ── ① 移入 cpuset（先放松 Android cgroup 限制）──
    if topo.cpuset_enabled && topo.base_cpuset_fd != -1 {
        if !cpuset_dir.is_empty() {
            ensure_cpuset_dir(topo, cpuset_dir);
        }
        let tasks = if cpuset_dir.is_empty() {
            format!("{BASE_CPUSET}/tasks")
        } else {
            format!("{BASE_CPUSET}/{cpuset_dir}/tasks")
        };
        if let Err(e) = fs::OpenOptions::new()
            .append(true)
            .open(&tasks)
            .and_then(|mut f| f.write_all(format!("{tid}\n").as_bytes()))
        {
            // Diagnostic visibility: join failure is the #1 cause of no_intersection.
            // Full control relies on the cpuset channel; failures must be discoverable.
            if warn_once(&WARNED_CPUSET_TIDS, tid) {
                eprintln!(
                    "warning: cpuset join failed tid={tid} ({tasks}): {e} \u{2014} affinity may still be restricted by Android cgroup"
                );
            }
            audit::record(AuditEntry {
                timestamp: 0,
                tid, pkg: pkg.to_string(),
                requested_cpus: requested.clone(),
                effective_cpus: String::new(),
                success: false,
                reason: "cpuset_write_failed".into(),
            });
        }
    }

    // ── ② 已符合目标则零开销返回 ──
    if let Some(curr) = CpuSet::get_affinity(tid) {
        if curr.bits == cpus.bits {
            SHORT_CIRCUIT_TOTAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            audit::record(AuditEntry {
                timestamp: 0,
                tid, pkg: pkg.to_string(),
                requested_cpus: requested.clone(),
                effective_cpus: requested.clone(),
                success: true,
                reason: "already".into(),
            });
            return ApplyOutcome::Applied;
        }
    }

    // ── ③ 在线过滤 ──
    let mut effective = *cpus;
    if topo.online_cpus.count() > 0 {
        let wbits = crate::topology::CPU_WORD_BITS;
        for i in 0..crate::topology::CPU_SETSIZE {
            if effective.is_set(i) && !topo.online_cpus.is_set(i) {
                effective.bits[i / wbits] &= !(1u64 << (i % wbits));
            }
        }
    }

    // ── ④ 内核 cgroup 允许集交集 ──
    if let Some(allowed) = CpuSet::read_allowed_mask(tid) {
        let before = effective.to_range_string();
        effective.and(&allowed);
        if effective.count() == 0 {
            // Structured log: avoid Chinese wording that reads like a bug ("target CPUs all excluded")
            if warn_once(&WARNED_BLOCKED_TIDS, tid) {
                eprintln!(
                    "skip affinity: tid={tid} requested={before} allowed={} reason=no_intersection (cpuset join failed or thread re-assigned by system)",
                    allowed.to_range_string()
                );
            }
            audit::record(AuditEntry {
                timestamp: 0,
                tid, pkg: pkg.to_string(),
                requested_cpus: before.clone(),
                effective_cpus: String::new(),
                success: false,
                reason: "cgroup".into(),
            });
            return ApplyOutcome::BlockedByCgroup;
        }
        let after = effective.to_range_string();
        if before != after {
            if warn_once(&WARNED_BLOCKED_TIDS, tid) {
                eprintln!(
                    "degrade affinity: tid={tid} requested={before} effective={after} allowed={} reason=cgroup_intersect",
                    allowed.to_range_string()
                );
            }
            audit::record(AuditEntry {
                timestamp: 0,
                tid, pkg: pkg.to_string(),
                requested_cpus: before,
                effective_cpus: after.clone(),
                success: true,
                reason: "downgraded".into(),
            });
            return ApplyOutcome::Downgraded;
        }
    }

    if effective.count() == 0 {
        return ApplyOutcome::SkippedNoCpus;
    }

    // ── ⑤ setaffinity ──
    let effective_str = effective.to_range_string();
    match effective.set_affinity(tid) {
        Err(e) => {
            let errno = e.raw_os_error();
            if errno == Some(libc::ESRCH) {
                audit::record(AuditEntry {
                    timestamp: 0,
                    tid, pkg: pkg.to_string(),
                    requested_cpus: requested,
                    effective_cpus: effective_str,
                    success: false,
                    reason: "esrch".into(),
                });
                return ApplyOutcome::Exited;
            }
            if errno == Some(libc::EINVAL) {
                if warn_once(&WARNED_EINVAL_TIDS, tid) {
                    eprintln!("setaffinity(tid={tid}) unexpected EINVAL (mask={})", effective_str);
                }
                audit::record(AuditEntry {
                    timestamp: 0,
                    tid, pkg: pkg.to_string(),
                    requested_cpus: requested,
                    effective_cpus: effective_str,
                    success: false,
                    reason: "einval".into(),
                });
                return ApplyOutcome::EINVAL;
            }
            if errno == Some(libc::EPERM) {
                // M2 修复：EPERM 去重（非 root 桌面场景避免刷屏）
                if warn_once(&WARNED_EPERM_TIDS, tid) {
                    eprintln!("warning: setaffinity(tid={tid}) EPERM (no CAP_SYS_NICE or target restricted)");
                }
                audit::record(AuditEntry {
                    timestamp: 0,
                    tid, pkg: pkg.to_string(),
                    requested_cpus: requested,
                    effective_cpus: effective_str,
                    success: false,
                    reason: "eperm".into(),
                });
                return ApplyOutcome::BlockedByPerm;
            }
            eprintln!("setaffinity(tid={tid}) failed: {e}");
            audit::record(AuditEntry {
                timestamp: 0,
                tid, pkg: pkg.to_string(),
                requested_cpus: requested,
                effective_cpus: effective_str,
                success: false,
                reason: "other".into(),
            });
            ApplyOutcome::Failed
        }
        Ok(()) => {
            audit::record(AuditEntry {
                timestamp: 0,
                tid, pkg: pkg.to_string(),
                requested_cpus: requested,
                effective_cpus: effective_str,
                success: true,
                reason: "applied".into(),
            });
            ApplyOutcome::Applied
        }
    }
}

fn apply_sched(tid: i32, policy: SchedPolicy, rt_prio: Option<i32>, nice: Option<i32>) {
    let pol = policy.to_libc();
    match policy {
        SchedPolicy::Fifo | SchedPolicy::Rr => {
            let mut p: libc::sched_param = unsafe { std::mem::zeroed() };
            p.sched_priority = rt_prio.unwrap_or(1).clamp(1, 99);
            unsafe {
                libc::sched_setscheduler(tid, pol, &p as *const libc::sched_param);
            }
        }
        _ => {
            let p: libc::sched_param = unsafe { std::mem::zeroed() };
            unsafe {
                libc::sched_setscheduler(tid, pol, &p as *const libc::sched_param);
            }
            if let Some(n) = nice {
                unsafe {
                    libc::setpriority(libc::PRIO_PROCESS, tid as u32, n.clamp(-20, 19));
                }
            }
        }
    }
}
