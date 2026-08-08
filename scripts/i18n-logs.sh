#!/bin/sh
# Batch English-ify user-visible log messages (open-source convention).
# Only touches string literals inside eprintln!/println!; never comments/tests/examples.

set -e
cd "$(dirname "$0")/.."

# ── policy.rs ──（占位符用 \{id\} 形式匹配）
perl -pi -e '
s/警告: tid=\{tid\} 需要 RT 调度但无 CAP_SYS_NICE，跳过 sched 字段/warning: tid={tid} needs RT scheduling but lacks CAP_SYS_NICE; sched skipped/;
s/setaffinity\(tid=\{tid\}\) 意外 EINVAL \(mask=\{mask\}\)/setaffinity(tid={tid}) unexpected EINVAL (mask={mask})/;
s/警告: setaffinity\(tid=\{tid\}\) EPERM（无 CAP_SYS_NICE 或目标受限）/warning: setaffinity(tid={tid}) EPERM (no CAP_SYS_NICE or target restricted)/;
s/setaffinity\(tid=\{tid\}\) 失败: \{e\}/setaffinity(tid={tid}) failed: {e}/;
' crates/core/src/policy.rs

# ── config.rs ──
perl -pi -e '
s/警告: \{\} 条规则无效: \{\}/warning: {} rules invalid: {}/;
s/警告: 未知 profile "\\\{name\}"（app \{pkg\}），回退默认策略/warning: unknown profile "{name}" (app {pkg}), falling back to default/;
s/读取配置失败: \{e\}/config read failed: {e}/;
s/配置语法错误: \{e\}/config syntax error: {e}/;
s/KDL 解析失败: \{e\}/KDL parse failed: {e}/;
s/—— 该规则已跳过，请改用 cpus 或有效集群名/ \x{2014} rule skipped; use cpus or a valid cluster name/;
s/无效（可用: \{\}/invalid (available: {}/;
' crates/core/src/config.rs

# ── store.rs ──
perl -pi -e '
s/配置热加载: inotify 已启用/config hot-reload: inotify enabled/;
s/配置热加载: inotify 不可用，使用轮询模式/config hot-reload: inotify unavailable, using polling/;
s/配置重载失败: \{e\}（保留旧配置）/config reload failed: {e} (keeping old config)/;
s/配置热加载: inotify 失效，降级为轮询模式/config hot-reload: inotify failed, degrading to polling/;
' crates/core/src/store.rs

# ── tracker.rs ──
perl -pi -e '
s/cpuset 目录回收失败: \{path\}/cpuset dir reclaim failed: {path}/;
s/cpuset 目录回收: \{path\}/cpuset dir reclaimed: {path}/;
' crates/core/src/tracker.rs

# ── system_context.rs ──
perl -pi -e '
s/系统感知: \/proc\/pressure\/memory 不可用（内核未启用 PSI），内存压力感知已禁用/system context: \/proc\/pressure\/memory unavailable (kernel without PSI), memory-pressure sensing disabled/;
' crates/core/src/system_context.rs

# ── daemon/main.rs ──
perl -pi -e '
s/错误: -c 需要指定配置文件路径/error: -c requires a config file path/;
s/错误: -s 需要 >=1 的整数间隔/error: -s requires an integer interval >= 1/;
s/未知选项: \{\}/unknown option: {}/;
s/配置文件不存在，已生成默认模板: \{\}/config file missing; generated default template: {}/;
s/警告: 无法生成默认配置 \{\}: \{\}/warning: cannot generate default config {}: {}/;
s/初始配置加载失败: \{\}/initial config load failed: {}/;
s/RT 调度权限: \{\}/RT scheduling: {}/;
s/"有"/"full"/;
s/"无 \(fifo\/rr 将被跳过\)"/"none (fifo\/rr will be skipped)"/;
s/警告: 规则包含 fifo\/rr 调度策略，但当前无 CAP_SYS_NICE，sched 字段将被跳过/warning: config contains fifo\/rr rules but CAP_SYS_NICE is missing; sched fields will be skipped/;
s/threadctl-rs v\{\} 启动 \(P2: proc 事件链路\)/threadctl-rs v{} started (P2: proc event pipeline)/;
s/配置热加载: 版本 \{\}/config hot-reload: version {}/;
s/配置变更重扫完成: 应用 \{\} 个线程/config change rescan done: applied {} threads/;
s/relock: 重应用 \{\} 个线程/relock: re-applied {} threads/;
s/CPU 拓扑: \{\} present, cpuset \{\}/CPU topology: {} present, cpuset {}/;
s/"可用"/"available"/;
s/"不可用"/"unavailable"/;
s/ 集群: / cluster: /;
s/初始配置加载成功: 版本 \{\}，\{\} 个规则包/initial config loaded: version {}, {} rule packages/;
s/RT 调度权限: 有/RT scheduling: full/;
s/RT 调度权限: 无/RT scheduling: none/;
' crates/daemon/src/main.rs

echo "done"
