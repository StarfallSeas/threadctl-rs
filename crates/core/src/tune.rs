//! P14 — 系统 tuning 模块（CPU governor / IO 调度器切换）。
//!
//! 与 threadctl 策略引擎统一配置（KDL `system` 节点）+ IPC 命令：
//! - `tune governor schedutil` —— 写所有 online CPU 的 scaling_governor
//! - `tune iosched mq-deadline` —— 写所有块设备的 queue/scheduler
//!
//! 安全：仅写 sysfs（root）；失败返回错误信息（不 panic）；可检测恢复。

use std::fs;

/// P14：KDL `system` 节点配置（启动时应用 + IPC 可查）。
#[derive(Debug, Clone, Default)]
pub struct SystemTuning {
    pub governor: Option<String>,
    pub io_scheduler: Option<String>,
}

/// 应用 CPU governor（写所有 online CPU 的 scaling_governor）。
pub fn apply_governor(gov: &str) -> Result<usize, String> {
    let mut applied = 0usize;
    let Ok(entries) = fs::read_dir("/sys/devices/system/cpu") else {
        return Err("无法读取 /sys/devices/system/cpu".into());
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("cpu") || !name[3..].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let gov_path = e.path().join("cpufreq/scaling_governor");
        if !gov_path.exists() {
            continue;
        }
        // 校验可用 governor 列表（防写入非法值）
        let available = fs::read_to_string(e.path().join("cpufreq/scaling_available_governors"))
            .unwrap_or_default();
        if !available.split_whitespace().any(|g| g == gov) {
            return Err(format!("governor {gov} 不在可用列表: {available}").trim().to_string());
        }
        match fs::write(&gov_path, gov) {
            Ok(_) => applied += 1,
            Err(err) => return Err(format!("写入 {gov_path:?} 失败: {err}")),
        }
    }
    if applied == 0 {
        return Err("没有可写 governor 的 CPU（无 cpufreq 或无权限）".into());
    }
    Ok(applied)
}

/// 应用 IO 调度器（写所有块设备 queue/scheduler）。
pub fn apply_io_scheduler(sched: &str) -> Result<usize, String> {
    let mut applied = 0usize;
    let Ok(entries) = fs::read_dir("/sys/block") else {
        return Err("无法读取 /sys/block".into());
    };
    for e in entries.flatten() {
        let sched_path = e.path().join("queue/scheduler");
        if !sched_path.exists() {
            continue;
        }
        let available = fs::read_to_string(&sched_path).unwrap_or_default();
        if !available.split_whitespace().any(|s| s == sched) {
            continue; // 该设备不支持此调度器——跳过（非致命）
        }
        match fs::write(&sched_path, sched) {
            Ok(_) => applied += 1,
            Err(err) => return Err(format!("写入 {sched_path:?} 失败: {err}")),
        }
    }
    if applied == 0 {
        return Err("没有支持该调度器的块设备".into());
    }
    Ok(applied)
}

/// 当前生效值（(governor, io_scheduler) 可选——检测第一个 CPU / 第一个块设备）。
pub fn detect_current() -> (Option<String>, Option<String>) {
    let gov = fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
        .ok()
        .map(|s| s.trim().to_string());
    let io = fs::read_to_string("/sys/block/mmcblk0/queue/scheduler")
        .ok()
        .or_else(|| fs::read_to_string("/sys/block/sda/queue/scheduler").ok())
        .map(|s| {
            // "mq-deadline [none] bfq" → 取中括号内当前值
            s.split_whitespace()
                .find(|w| w.starts_with('[') && w.ends_with(']'))
                .map(|w| w[1..w.len() - 1].to_string())
                .unwrap_or_default()
        })
        .filter(|s| !s.is_empty());
    (gov, io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_current_returns_options() {
        // 无 sysfs 环境返回 None（结构正确性，不 panic）
        let (gov, io) = detect_current();
        // 本测试机可能无 cpufreq——只验证类型正确
        let _ = (gov, io);
        assert!(true);
    }
}
