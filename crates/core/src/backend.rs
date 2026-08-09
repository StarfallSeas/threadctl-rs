//! Backend abstraction (P6.2-3, ChatGPT P6.2 approval).
//!
//! Traits cover the three kernel interfaces threadctl touches; a single
//! `LinuxV1Backend` provides the current Linux 5.10+ (sysfs cpuset +
//! sched_setaffinity + sched_setattr) implementation. Future kernels
//! (cgroup v2-only, no /dev/cpuset) need only implement these traits.
//!
//! ChatGPT constraint: no empty `AndroidV2Backend` stub. Only define
//! traits + the v1 implementation; future backends are written when
//! the target kernel interface is actually available.

use std::collections::HashSet;
use std::fs;
use std::io::Write as _;
use std::sync::{LazyLock, Mutex};

use crate::policy::SchedPolicy;
use crate::topology::CpuSet;

/// 已确保存在的 cpuset 子目录缓存（CLAUDE NEW-H1/L1：缓存必须由
/// LinuxV1Backend 拥有——Backend 重构后 policy.rs 的旧缓存成了死结构，
/// 每次 apply 都重写 cpuset 目录（mkdir/chmod/chown/write × N 线程 × relock）。
/// 移入此处，ensure_dir 检查缓存；tracker 回收目录时经 forget_cpuset_dir 清除）。
static ENSURED_CPUSET_DIRS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// 目录被 rmdir 回收后同步清除 ensure 缓存（否则下次 ensure 被缓存跳过 →
/// cpuset tasks 写入失败）。tracker 回收目录时调用。
pub fn forget_cpuset_dir(dir_name: &str) {
    if let Ok(mut guard) = ENSURED_CPUSET_DIRS.lock() {
        guard.remove(dir_name);
    }
}

// ── Traits ──────────────────────────────────────────────────────

/// CPU affinity operations。
pub trait AffinityOps {
    fn get_affinity(&self, tid: i32) -> Option<CpuSet>;
    fn set_affinity(&self, tid: i32, cpus: &CpuSet) -> Result<(), i32>;
    fn read_allowed_mask(&self, tid: i32) -> Option<CpuSet>;
}

/// Cpuset (cgroup v1 /dev/cpuset) operations。
/// CLAUDE NEW-M3/Q5: attach_task 返回错误（policy.rs 调用处做 warn_once + audit），
/// 不再静默吞错；forget_dir 从 trait 删除（消除对 policy.rs 的反向依赖——
/// tracker 直接调 `backend::forget_cpuset_dir`）。
pub trait CpusetOps {
    fn ensure_dir(&self, name: &str);
    fn attach_task(&self, tid: i32, dir: &str) -> Result<(), std::io::Error>;
    fn remove_dir(&self, path: &str) -> bool;
}

/// Scheduler operations (sched policy + nice + uclamp).
pub trait SchedulerOps {
    fn set_scheduler(&self, tid: i32, policy: SchedPolicy, prio: Option<i32>) -> Result<(), i32>;
    fn set_nice(&self, tid: i32, nice: i32) -> Result<(), i32>;
    fn set_uclamp(&self, tid: i32, min: Option<u32>, max: Option<u32>) -> Result<(), i32>;
}

/// Composite backend: threadctl only ever uses a single concrete backend.
pub trait TaskBackend: AffinityOps + CpusetOps + SchedulerOps {}
impl<T: AffinityOps + CpusetOps + SchedulerOps> TaskBackend for T {}

// ── Linux V1 (sysfs cpuset + sched_setaffinity + sched_setattr) ─

/// Default backend: Linux 5.10+ with /dev/cpuset, sched_setaffinity,
/// sched_setscheduler, and sched_setattr for uclamp.
pub struct LinuxV1Backend;

impl AffinityOps for LinuxV1Backend {
    fn get_affinity(&self, tid: i32) -> Option<CpuSet> {
        CpuSet::get_affinity(tid)
    }

    fn set_affinity(&self, tid: i32, cpus: &CpuSet) -> Result<(), i32> {
        match cpus.set_affinity(tid) {
            Ok(()) => Ok(()),
            Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn read_allowed_mask(&self, tid: i32) -> Option<CpuSet> {
        CpuSet::read_allowed_mask(tid)
    }
}

impl CpusetOps for LinuxV1Backend {
    fn ensure_dir(&self, name: &str) {
        // CLAUDE NEW-H1：缓存检查在 backend 层——已确保过的目录不再
        // mkdir/chmod/chown/write（旧 policy.rs 缓存是死结构，Backend 重构后
        // 每次 apply 都重写 cpuset 目录，真机上数百线程 × relock = 每轮上千 syscall）。
        let mut guard = ENSURED_CPUSET_DIRS.lock().unwrap_or_else(|e| e.into_inner());
        if !guard.insert(name.to_string()) {
            return;
        }
        drop(guard);
        // name 即 CPU 范围字符串（如 "0-3"），与 cpuset 目录名 1:1。
        // mems 用 "0"：单 NUMA 节点设备恒为 "0"；多节点需扩展 Backend 携带拓扑
        // （旧 ensure_cpuset_dir 用 topo.mems_str，语义等价，CLAUDE NEW-L3）。
        let path = format!("{}/{}", crate::topology::BASE_CPUSET, name);
        crate::topology::create_cpuset_dir(&path, name, "0");
    }

    fn attach_task(&self, tid: i32, dir: &str) -> Result<(), std::io::Error> {
        let path = if dir.is_empty() {
            format!("{}/tasks", crate::topology::BASE_CPUSET)
        } else {
            format!("{}/{}/tasks", crate::topology::BASE_CPUSET, dir)
        };
        // CLAUDE NEW-M3/Q5：返回错误（不再静默吞掉），policy.rs 调用处做
        // warn_once + audit——cpuset 移入失败是亲和性被系统限制的首要原因。
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(format!("{tid}\n").as_bytes()))
    }

    fn remove_dir(&self, path: &str) -> bool {
        crate::topology::remove_cpuset_dir(path)
    }
}

impl SchedulerOps for LinuxV1Backend {
    fn set_scheduler(&self, tid: i32, policy: SchedPolicy, prio: Option<i32>) -> Result<(), i32> {
        let pol = policy.to_libc();
        match policy {
            SchedPolicy::Fifo | SchedPolicy::Rr => {
                let mut p: libc::sched_param = unsafe { std::mem::zeroed() };
                p.sched_priority = prio.unwrap_or(1).clamp(1, 99);
                if unsafe { libc::sched_setscheduler(tid, pol, &p as *const libc::sched_param) } == 0
                {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO))
                }
            }
            _ => {
                let p: libc::sched_param = unsafe { std::mem::zeroed() };
                if unsafe { libc::sched_setscheduler(tid, pol, &p as *const libc::sched_param) } == 0
                {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO))
                }
            }
        }
    }

    fn set_nice(&self, tid: i32, nice: i32) -> Result<(), i32> {
        if unsafe { libc::setpriority(libc::PRIO_PROCESS, tid as u32, nice.clamp(-20, 19)) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO))
        }
    }

    fn set_uclamp(&self, tid: i32, min: Option<u32>, max: Option<u32>) -> Result<(), i32> {
        if min.is_none() && max.is_none() {
            return Ok(());
        }
        // SCHED_FLAG_UTIL_CLAMP values (include/uapi/linux/sched.h, Linux 5.10+).
        // BUG-H1 修复 (Claude)：此前用了错误的 0x4000_0000/0x01/0x02，导致
        // sched_setattr 遇到未知 flag 位返回 EINVAL——uclamp 从未真正生效。
        const SCHED_FLAG_UTIL_CLAMP_MIN: u64 = 0x20;
        const SCHED_FLAG_UTIL_CLAMP_MAX: u64 = 0x40;
        #[repr(C)]
        struct SchedAttr {
            size: u32,
            sched_policy: u32,
            sched_flags: u64,
            sched_nice: i32,
            sched_priority: u32,
            sched_runtime: u64,
            sched_deadline: u64,
            sched_period: u64,
            sched_util_min: u32,
            sched_util_max: u32,
        }
        let mut attr: SchedAttr = unsafe { std::mem::zeroed() };
        attr.size = std::mem::size_of::<SchedAttr>() as u32;
        // CLAUDE NEW-M1：按 min/max 分别置 flag（不再无脑同时设两个——
        // 用户只配 min 时不应发 MAX flag 重置内核既有 max）。
        // 未指定的一侧：min→0（无下限）、max→1024（无上限），语义"不约束"。
        attr.sched_flags = (if min.is_some() { SCHED_FLAG_UTIL_CLAMP_MIN } else { 0 })
            | (if max.is_some() { SCHED_FLAG_UTIL_CLAMP_MAX } else { 0 });
        attr.sched_util_min = min.unwrap_or(0).clamp(0, 1024);
        attr.sched_util_max = max.unwrap_or(1024).clamp(0, 1024);

        if unsafe {
            libc::syscall(
                libc::SYS_sched_setattr,
                tid,
                &mut attr as *mut SchedAttr,
                0u32,
            )
        } == 0
        {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO))
        }
    }
}

/// Default backend instance (zero-size, constructed on first use).
pub fn default_backend() -> LinuxV1Backend {
    LinuxV1Backend
}
