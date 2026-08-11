//! P7.2.1 — debug 日志开关（排查用）。
//!
//! 启用方式：`TC_DEBUG=1 threadctl ...` 或 `threadctl --debug ...`。
//! 所有 `debug_log!` 插点仅在开关开启时输出——运行时零开销（单次
//! AtomicBool 读取）。
//!
//! 排查场景（游戏线程不生效等）：
//! - eBPF 原始事件（fork/exec/exit 的 pid/tid/comm）
//! - pending 入队/退避/分流（Tgid 判断）
//! - 白名单重建键列表
//! - 事件→规则命中（is_interested 结果、tracker enter/remove）
//! - apply 逐 tid 规则来源（线程规则 vs default）
//! - 覆盖采样每进程归属、relock 决策明细

use std::sync::atomic::{AtomicBool, Ordering};

static DEBUG: AtomicBool = AtomicBool::new(false);

/// 设置 debug 开关（daemon 启动时由 env/CLI 设置）。
pub fn set_debug(on: bool) {
    DEBUG.store(on, Ordering::Relaxed);
}

/// 当前是否开启 debug。
pub fn enabled() -> bool {
    DEBUG.load(Ordering::Relaxed)
}

/// debug 日志宏（工程级）：`debug_log!("module", ...)` → `[debug][module] ...`。
/// 仅在 debug 开关开启时输出（单次 AtomicBool 读取，零开销）。
/// 模块标记便于 grep 分域排查：`grep "\[ebpf\]" threadctl.log`。
#[macro_export]
macro_rules! debug_log {
    ($module:expr, $($arg:tt)*) => {
        if $crate::debug::enabled() {
            println!("[debug][{}] {}", $module, format_args!($($arg)*));
        }
    };
}
