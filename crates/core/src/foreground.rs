//! Foreground UID detection & cache (ChatGPT 5.5 P5.2 must-have).
//!
//! Three-tier model:
//!   SystemState (userspace) → Foreground UID Cache → BPF UID MAP (eBPF)
//!
//! Android cpusets: /dev/cpuset/top-app/tasks and /dev/cpuset/foreground/tasks —
//! reading these files extracts foreground app UIDs for eBPF filtering.

use std::collections::HashSet;
use std::fs;
use std::sync::{LazyLock, Mutex};

/// 前台 UID 缓存。
static FOREGROUND_UIDS: LazyLock<Mutex<HashSet<u32>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// 刷新前台 UID 集合。
pub fn refresh_foreground_uids() -> usize {
    let mut uids = HashSet::new();
    for cpuset in &["/dev/cpuset/top-app/tasks", "/dev/cpuset/foreground/tasks"] {
        if let Ok(content) = fs::read_to_string(cpuset) {
            for line in content.lines() {
                let pid: i32 = match line.trim().parse() {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if let Some(uid) = pid_uid(pid) {
                    uids.insert(uid);
                }
            }
        }
    }
    let count = uids.len();
    if let Ok(mut guard) = FOREGROUND_UIDS.lock() {
        *guard = uids;
    }
    count
}

/// 检查 UID 是否在前台。
pub fn is_foreground_uid(uid: u32) -> bool {
    FOREGROUND_UIDS
        .lock()
        .map(|g| g.contains(&uid))
        .unwrap_or(false)
}

/// 获取已缓存的前台 UID 数量。
pub fn foreground_uid_count() -> usize {
    FOREGROUND_UIDS.lock().map(|g| g.len()).unwrap_or(0)
}

/// 读取 /proc/<pid>/status 的 Uid 字段。
fn pid_uid(pid: i32) -> Option<u32> {
    let content = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("Uid:") {
            // "Uid:\t1000\t1000\t1000\t1000"
            return v.split_whitespace().next()?.parse::<u32>().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_does_not_crash() {
        let n = refresh_foreground_uids();
        // 无论是否有前台进程，函数不崩溃
        let _ = n;
    }
}
