# threadctl-rs — P2 阶段审查文档

> 供 Claude 审查。P2 完成：ProcSource + StateTracker + 事件处理引擎 + relock 周期锁定 + Q6 权限检查。
> 纯用户态跑通"事件 → 规则 → 策略执行"全链路。

---

## P2 概述

P2 实现了守护进程的第一个可运行闭环：
1. **ProcSource** — 实现 `EventSource` trait，/proc 轮询发现进程（全量 + 增量）
2. **StateTracker** — 进程/线程状态管理，线程名 TTL 缓存（每进程粒度），cpuset 目录引用计数 + 自动回收
3. **事件处理引擎** — 事件 → 规则匹配 → 策略执行（affinity + sched/nice），是各事件源的统一入口
4. **relock 周期锁定** — 对抗 Android AMS/cgroup 覆盖
5. **Q6 权限检查** — capget 检测 CAP_SYS_NICE，无权限时跳过 fifo/rr 并告警

---

## 新增/修改文件清单

### core（lib）

| 文件 | 操作 | 行数 | 说明 |
|---|---|---|---|
| `proc.rs` | 新增 | 88 | /proc 读取工具（栈上缓冲）：`read_cmdline`/`read_thread_name`/`list_tids` |
| `caps.rs` | 新增 | 58 | CAP_SYS_NICE 检查（capget syscall 封装，Android bionic 兼容） |
| `tracker.rs` | 新增 | 230 | StateTracker + ThreadNameCache（Q3/Q5 落地） |
| `engine.rs` | 新增 | 268 | 事件处理核心：`handle_events`/`refresh_process_rules`/`relock_all`/`cleanup_dead` |
| `event.rs` | 修改 | +8 | `ProcessEvent` 加 `pkg: Option<String>` + `with_pkg()` builder |
| `ruleset.rs` | 修改 | +10 | `has_rt_sched()` 查询方法（Q6 用） |
| `policy.rs` | 修改 | +10 | `apply_thread` 加 `rt_allowed` 参数；`SchedPolicy::is_rt()` |
| `lib.rs` | 修改 | +5 | 新增 `pub mod proc/caps/tracker/engine` |

### daemon（bin）

| 文件 | 操作 | 行数 | 说明 |
|---|---|---|---|
| `proc_source.rs` | 新增 | 155 | ProcSource：sysinfo 增量判断 + /proc 全量/增量扫描 + Exit 检测 + `EventSource` impl |
| `main.rs` | 重写 | 190 | Orchestrator 主循环：poll/relock/cleanup/配置变更重扫 |

### 测试

- core 测试：11 passed（config 2 + store 3 + tracker 3 + proc 2 + caps 1）
- 真实冒烟测试：完整链路通过（sleep 进程发现 → apply 1 线程 → relock 重应用）

---

## 逐模块设计说明

### 1. `proc.rs` — /proc 读取

沿用栈上路径缓冲设计。新增 `list_tids`（枚举 /proc/<pid>/task）。

**注意**：`read_proc_file` 用 `FileExt::read_at`（pread）而非 `Read::read`，
避免 lseek 竞争。

### 2. `caps.rs` — CAP_SYS_NICE 检查

- 使用原始 capget syscall（`libc::syscall(libc::SYS_capget, ...)`）而非 libc wrapper
- 原因：Android bionic 的 libc crate 可能未导出 `libc::capget` 或 `libc::__user_cap_*_struct`
- `CAP_SYS_NICE = 23`：常量硬编码（libc crate 未导出此常量）

### 3. `tracker.rs` — StateTracker（Q3 / Q5 落地）

**ThreadNameCache**（Q5 — Claude 批准方案）：
- 每进程实例，TTL 60s 各自计时（天然错开，无全局清峰）
- `get_or_read`：过期清空 → entry 查询 → miss 读 /proc
- `clear`：exec 主动失效
- `retain`：线程收缩时清理死条目

**ProcessState**：
- `applied_tids: HashSet<i32>`：增量检测基准（新线程 vs 已应用差异）
- `applied_dirs: HashSet<String>`：cpuset 引用归属（退出时释放）

**StateTracker**：
- `cpuset_refs: HashMap<String, u32>`：目录名 → 引用计数
- `remove(pid)`：遍历 `applied_dirs` 逐目录 decr，归零调用 `remove_cpuset_dir` 回收
- `register_dirs`：进程首次应用某目录时 incr（幂等）
- `retain_interested`：配置变更后清理不再关注的进程

**审 Conrad**：`cpuset_refs` 的引用计数是安全的——每个进程最多引用一个目录一次（`applied_dirs` 幂等记录），
移除时 `saturating_sub(1)` 防下溢，归零从 map 移除后调用 rmdir。

### 4. `engine.rs` — 事件处理引擎

**handle_events** 是各事件源的统一入口（proc 和 P3 eBPF 共用）：

```
ProcessEvent → 读取 pkg（事件自带 or fallback /proc）
           → 白名单检查（is_interested）
           → 事件分发：
             Exit    → remove 进程（释放 cpuset 引用）
             Fork    → refresh_process_rules（全线程扫描）
             Exec    → 清缓存 + refresh_process_rules
             ThreadClone → TTL 内单 tid 增量 or 全线程重扫
```

**refresh_process_rules**：
- 枚举 `list_tids` → 取出线程名缓存（`mem::replace` 短借用）
- 逐 tid：读线程名（缓存命中/读 /proc）→ `RuleSet::resolve` → `policy::apply_thread`
- 归还缓存 + 记录 `applied_tids`

**借用纪律**（本次修复重点）：
外层不持有 `tracker.enter()` 的长借用跨函数调用；`refresh_process_rules`
通过 `mem::replace` 取出缓存立即归还，`mark_scanned` 单独短借用。

**relock_all**：
- 遍历全部 tracked pid → kill(0) 存活性 → refresh_process_rules
- getaffinity 短路保证空转开销小（大部分线程无事发生）

**cleanup_dead**：
- kill(0) 失败 → remove（释放 cpuset 引用）

### 5. `proc_source.rs` — ProcSource

**发现策略**（proc 轮询模式演进）：
- sysinfo 进程总数变化 → 全量扫描（遍历 /proc 全目录）
- 稳定 → 增量路径（只检查 tracked pid 的线程增量 + kill 存活）
- Exit 检测：tracked pid 不在当前 /proc 中 → 产出 `ProcessEvent::exit`

**EventSource 实现**：
- `poll(deadline)`：collect → 有事件立即返回，无事件 sleep 到 deadline
- `on_config_changed`：设 `scan_all=true`，清 `last_proc_total`

**增量线程检测**：
- `new_tids_for`：`list_tids(pid)` ∩ `applied_tids` 的差集
- `applied_tids` 为空（进程未扫）或 `initial_scan_done=false` → 不产事件（由 Fork 全扫接管）

### 6. `main.rs` — Orchestrator 主循环

单线程事件循环：

```
loop {
    // 1. 配置变更（reload channel try_recv）
    //    → augment source + retain_interested + relock_all 全量刷新
    // 2. relock（lock_interval 到点）
    //    → relock_all（遍历全部进程重新应用）
    // 3. cleanup（dead_cleanup_interval 到点）
    //    → cleanup_dead（kill 检查 + remove）
    // 4. poll 事件
    //    → source.poll(deadline) → engine.handle_events → apply
}
```

`tracker: Arc<Mutex<StateTracker>>` 由 ProcSource 和引擎共享（单线程无竞争，
Mutex 为 P4 IPC 线程访问铺路）。

---

## 与常见 proc 轮询实现的差异

| 维度 | 常见 proc 轮询实现 | threadctl v2 |
|---|---|---|
| 事件源 | if-else 主循环（ebpf/proc 分支） | `EventSource` trait（可插拔） |
| 状态管理 | 散落 HashMap（`process_cache`/`ProcCache`） | 统一 `StateTracker`（含引用计数） |
| 线程名缓存 | 全局 60s 全清 | 每进程 TTL（各自计时 + exec 主动失效） |
| cpuset 目录 | 只建不删（泄漏） | 引用计数 + 归零 rmdir 回收 |
| 事件→应用 | proc 模式串行全扫 + 5 轮 apply | 事件驱动 + Fork 立即全扫 + ThreadClone 增量 |
| 权限检查 | 无 | capget CAP_SYS_NICE 检查 |
| relock | 通常无 | 有（v1 继承，Android 刚需） |

---

## 冒烟测试结果

```
$ ./target/debug/threadctl -c /tmp/test.toml
CPU 拓扑: 8 present, cpuset 不可用
初始配置加载成功: 版本 1，1 个规则包
RT 调度权限: 无 (fifo/rr 将被跳过)
threadctl-rs v2.0.0 启动 (P2: proc 事件链路)
配置热加载: inotify 已启用
事件: 1 条, 应用 1 个线程
relock: 重应用 1 个线程
事件: 1 条, 应用 1 个线程
```

配置规则 `pkg="sleep" cpus="0-1"`，发现 sleep 进程 → apply 1 线程 → relock 确认。

---

## 待你审查的要点

1. **借用结构**：engine.rs 的 `refresh_process_rules` 用 `mem::replace` 取出缓存，
   `mark_scanned` 单独短借用——是否有潜在的 NLL 释放失效风险？
2. **cpuset_refs 计数正确性**：`register_dirs` 幂等 + `remove` 逐个 decr 的逻辑是否有边角 case？
   （如同目录被两个进程引用，一个退出时归零→ rmdir，另一个进程后续写入失败）
3. **ProcSource 事件重复**：Fork 事件后 engine 做全扫（refresh 更新 applied_tids），
   下轮 poll 增量检测 `applied_tids` 差集——是否有事件漏发？（如 fork 后立即创建新线程）
4. **sysinfo.procs 变化阈值**：当前 `proc_total != last_proc_total` 即全扫（加阈值避免频繁全扫），
   Android 上 task 频繁创建是否导致过度全扫？
5. **单测覆盖盲区**：engine 层（`refresh_process_rules`/`relock_all`）因需要真实 /proc，
   未加单元测试——是否有推荐的 mock 方式？
