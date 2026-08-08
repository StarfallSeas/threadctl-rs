//! CapabilitySet — runtime capability detection (ChatGPT review item #4).
//!
//! Android versions and vendor kernels differ wildly in supported features:
//! - uclamp: Android 10+ (kernel 5.4+), `sched_setattr(SCHED_FLAG_UTIL_CLAMP)`
//! - schedtune: legacy devices (kernel 4.x vendor), `/dev/stune/*`
//! - cpuset: almost all Android, `/dev/cpuset` or cgroup v2
//!
//! Detected once at startup; policy decisions only read `CapabilitySet`.

use std::fs;

/// 进程可用能力。
#[derive(Debug, Clone)]
pub struct CapabilitySet {
    /// sched_setattr SCHED_FLAG_UTIL_CLAMP 是否可用。
    pub uclamp: bool,
    /// /dev/stune 目录是否可访问（旧设备 schedtune boost）。
    pub schedtune: bool,
    /// /dev/cpuset 或 /sys/fs/cgroup/cpuset 是否可用。
    pub cpuset: bool,
}

impl CapabilitySet {
    pub fn detect() -> Self {
        Self {
            uclamp: Self::check_uclamp(),
            schedtune: Self::check_schedtune(),
            cpuset: Self::check_cpuset(),
        }
    }

    fn check_uclamp() -> bool {
        // 非侵入式检测：读 /proc/sys/kernel/sched_util_clamp_max
        // 该文件仅在启用 uclamp 的内核上存在
        if let Ok(v) = fs::read_to_string("/proc/sys/kernel/sched_util_clamp_max") {
            if let Ok(val) = v.trim().parse::<u32>() {
                return val > 0;
            }
        }
        // 回退：检查 sysfs
        fs::metadata("/sys/fs/cgroup/cpu.uclamp.max").is_ok()
    }

    fn check_schedtune() -> bool {
        fs::metadata("/dev/stune").is_ok()
    }

    fn check_cpuset() -> bool {
        fs::metadata("/dev/cpuset").is_ok()
            || fs::metadata("/sys/fs/cgroup/cpuset").is_ok()
    }

    /// 策略推荐顺序（ChatGPT 审查第 4 条：uclamp > schedtune > cpuset > affinity）。
    pub fn preferred_order(&self) -> &[&str] {
        if self.uclamp {
            &["uclamp", "cpuset", "affinity"]
        } else if self.schedtune {
            &["schedtune", "cpuset", "affinity"]
        } else {
            &["cpuset", "affinity"]
        }
    }

    /// 给启动日志用的摘要。
    pub fn summary(&self) -> String {
        let mut v = Vec::new();
        if self.uclamp { v.push("uclamp"); }
        if self.schedtune { v.push("schedtune"); }
        if self.cpuset { v.push("cpuset"); }
        format!("Capability: [{}], order: [{}]",
            v.join(", "),
            self.preferred_order().join(" > "),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_does_not_crash() {
        let caps = CapabilitySet::detect();
        // 非 Android/Linux 桌面可能全部为 false（正常），只验证不崩溃
        let _ = caps.summary();
    }

    #[test]
    fn preferred_order_is_non_empty() {
        let caps = CapabilitySet { uclamp: false, schedtune: false, cpuset: true };
        assert_eq!(caps.preferred_order(), &["cpuset", "affinity"]);
    }
}
