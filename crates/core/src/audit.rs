//! Audit feedback loop (ChatGPT 5.5 P5.1 must-have).
//!
//! Observe → Decide → Act → **Measure** → Adjust
//!
//! Records expected vs actual effect of every rule application, feeding back
//! into the decision engine.

use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// 单调秒（调试用，与 daemon 主循环对齐）。
fn now_secs() -> u64 {
    let t = SystemTime::now().duration_since(UNIX_EPOCH);
    match t {
        Ok(d) => d.as_secs(),
        Err(_) => 0,
    }
}

/// 单次应用的审计记录。
#[derive(Debug, Clone)]
pub struct AuditEntry {
    /// 记录时间（UNIX 秒）。
    pub timestamp: u64,
    pub tid: i32,
    pub pkg: String,
    /// 请求的 CPU 范围。
    pub requested_cpus: String,
    /// 实际生效的 CPU 范围（经过 online/allowed 交集后）。
    pub effective_cpus: String,
    /// setaffinity 是否成功。
    pub success: bool,
    /// 失败原因。
    pub reason: String,
}

/// 反馈摘要。
#[derive(Debug, Clone, Default)]
pub struct AuditSummary {
    pub total_attempts: usize,
    pub success: usize,
    pub blocked_by_cgroup: usize,   // Cpus_allowed 排除
    pub blocked_by_perm: usize,     // EPERM
    pub downgraded: usize,          // 目标被缩减但部分生效
    pub esrch: usize,               // 线程已退出
}

impl AuditSummary {
    /// 失败率 0.0~1.0（P6.2-2：DecisionEngine 的 slow 信号）。
    /// 失败 = total − success（downgraded 已计 success）。
    pub fn failure_rate(&self) -> f64 {
        if self.total_attempts == 0 {
            0.0
        } else {
            (self.total_attempts - self.success) as f64 / self.total_attempts as f64
        }
    }
}

/// 全局审计缓冲（最近 N 条记录，环形）。
static AUDIT_LOG: LazyLock<Mutex<Vec<AuditEntry>>> = LazyLock::new(|| Mutex::new(Vec::new()));
const AUDIT_LOG_MAX: usize = 256;

/// 记录一次审计（自动盖时间戳）。
pub fn record(mut entry: AuditEntry) {
    if entry.timestamp == 0 {
        entry.timestamp = now_secs();
    }
    // L2 修复：poison 恢复，与 summary() 一致
    let mut log = AUDIT_LOG.lock().unwrap_or_else(|e| e.into_inner());
    if log.len() >= AUDIT_LOG_MAX {
        log.remove(0);
    }
    log.push(entry);
}
pub fn recent(n: usize) -> Vec<AuditEntry> {
    AUDIT_LOG
        .lock()
        .map(|log| {
            let start = log.len().saturating_sub(n);
            log[start..].to_vec()
        })
        .unwrap_or_default()
}

/// 计算反馈摘要（全缓冲：反映缓冲内累计）。
pub fn summary() -> AuditSummary {
    AUDIT_LOG
        .lock()
        .map(|log| summarize(&log))
        .unwrap_or_default()
}

/// 计算最近 `window_secs` 秒内的摘要（反映近期活动，不被环形缓冲上限掩盖）。
pub fn summary_windowed(window_secs: u64) -> AuditSummary {
    if window_secs == 0 {
        return summary();
    }
    let cutoff = now_secs().saturating_sub(window_secs);
    AUDIT_LOG
        .lock()
        .map(|log| {
            let recent: Vec<AuditEntry> =
                log.iter().filter(|e| e.timestamp >= cutoff).cloned().collect();
            summarize(&recent)
        })
        .unwrap_or_default()
}

fn summarize(log: &[AuditEntry]) -> AuditSummary {
    let mut s = AuditSummary::default();
    s.total_attempts = log.len();
    for entry in log {
        if entry.success {
            s.success += 1;
        }
        match entry.reason.as_str() {
            "cgroup" => s.blocked_by_cgroup += 1,
            "cpuset_write_failed" => s.blocked_by_cgroup += 1,
            "eperm" => s.blocked_by_perm += 1,
            "downgraded" => s.downgraded += 1,
            "esrch" => s.esrch += 1,
            _ => {}
        }
    }
    s
}

/// 给调试/日志用的简短摘要（最近 60 秒窗口）。
pub fn summary_string() -> String {
    let s = summary_windowed(60);
    format!(
        "audit(last60s): total={} success={} cgroup_blocked={} perm_blocked={} downgraded={} esrch={}",
        s.total_attempts, s.success,
        s.blocked_by_cgroup, s.blocked_by_perm,
        s.downgraded, s.esrch
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 并行测试隔离：清空共享环形缓冲。
    fn reset() {
        AUDIT_LOG.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// 环形缓冲 + 原因计数 + recent 断言合并为单个测试：
    /// AUDIT_LOG 是全局静态，并行测试会互相污染，必须串行执行。
    #[test]
    fn ring_buffer_and_summary() {
        reset();
        // ① 环形上限
        for i in 0..300 {
            record(AuditEntry {
                timestamp: 0,
                tid: i,
                pkg: "com.x".into(),
                requested_cpus: "0-1".into(),
                effective_cpus: "0-1".into(),
                success: i % 2 == 0,
                reason: "applied".into(),
            });
        }
        let s = summary();
        assert_eq!(s.total_attempts, 256, "环形缓冲上限 256");
        assert_eq!(s.success, 128, "300 条中偶数 150 → 环形淘汰最早 44 条后剩 128");

        // ② 原因计数
        reset();
        record(AuditEntry {
            timestamp: 0,
            tid: 1, pkg: "com.x".into(),
            requested_cpus: "0-7".into(),
            effective_cpus: String::new(),
            success: false,
            reason: "cgroup".into(),
        });
        record(AuditEntry {
            timestamp: 0,
            tid: 2, pkg: "com.x".into(),
            requested_cpus: "0-7".into(),
            effective_cpus: "0-3".into(),
            success: true,
            reason: "downgraded".into(),
        });
        record(AuditEntry {
            timestamp: 0,
            tid: 3, pkg: "com.x".into(),
            requested_cpus: "0-7".into(),
            effective_cpus: String::new(),
            success: false,
            reason: "eperm".into(),
        });
        record(AuditEntry {
            timestamp: 0,
            tid: 4, pkg: "com.x".into(),
            requested_cpus: "0-7".into(),
            effective_cpus: String::new(),
            success: false,
            reason: "esrch".into(),
        });
        let s = summary();
        assert_eq!(s.blocked_by_cgroup, 1);
        assert_eq!(s.downgraded, 1);
        assert_eq!(s.blocked_by_perm, 1);
        assert_eq!(s.esrch, 1);

        // ③ recent 取最新
        let recent = recent(5);
        assert!(!recent.is_empty());
        assert_eq!(recent.last().unwrap().tid, 4);
    }
}
