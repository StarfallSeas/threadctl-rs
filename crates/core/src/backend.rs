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

use std::fs;
use std::io::Write as _;

use crate::policy::SchedPolicy;
use crate::topology::CpuSet;

// ── Traits ──────────────────────────────────────────────────────

/// CPU affinity operations.
pub trait AffinityOps {
    fn get_affinity(&self, tid: i32) -> Option<CpuSet>;
    fn set_affinity(&self, tid: i32, cpus: &CpuSet) -> Result<(), i32>;
    fn read_allowed_mask(&self, tid: i32) -> Option<CpuSet>;
}

/// Cpuset (cgroup v1 /dev/cpuset) operations.
pub trait CpusetOps {
    fn ensure_dir(&self, name: &str);
    fn attach_task(&self, tid: i32, dir: &str);
    fn remove_dir(&self, path: &str) -> bool;
    fn forget_dir(&self, name: &str);
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
        // name 即 CPU 范围字符串（如 "0-3"），与 cpuset 目录名 1:1。
        // mems 用 "0"（单 NUMA 节点设备；多节点需扩展 Backend 携带拓扑）。
        let path = format!("{}/{}", crate::topology::BASE_CPUSET, name);
        crate::topology::create_cpuset_dir(&path, name, "0");
    }

    fn attach_task(&self, tid: i32, dir: &str) {
        let path = if dir.is_empty() {
            format!("{}/tasks", crate::topology::BASE_CPUSET)
        } else {
            format!("{}/{}/tasks", crate::topology::BASE_CPUSET, dir)
        };
        let _ = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(format!("{tid}\n").as_bytes()));
    }

    fn remove_dir(&self, path: &str) -> bool {
        crate::topology::remove_cpuset_dir(path)
    }

    fn forget_dir(&self, name: &str) {
        crate::policy::forget_cpuset_dir(name);
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
        const UCLAMP_MIN: u64 = 0x20;
        const UCLAMP_MAX: u64 = 0x40;
        attr.sched_flags = (UCLAMP_MIN | UCLAMP_MAX)
            | if min.is_some() { 0 } else { 0 }  // flags always set both for simplicity
            | 0;
        // Flags: set both MIN and MAX so the kernel knows we're using uclamp
        attr.sched_flags = UCLAMP_MIN | UCLAMP_MAX;
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
