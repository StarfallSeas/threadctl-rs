# threadctl-rs — P4 阶段审查文档

> 供 Claude 审查。P4 目标：**完成守护进程化与可观测性** — IPC 控制面、
> 信号处理、pidfile、tracing 日志、telemetry 指标、`--parallel` 并行应用。

---

## 一、P4 概述

P0-P3 完成后，程序已具备完整的事件→规则→执行闭环（proc + eBPF 双源）。
P4 补齐**生产环境必需**的运维能力：

1. **IPC 控制面** — Unix socket JSON-line，运行时查询、动态重载
2. **信号与生命周期** — SIGTERM/INT 优雅退出、SIGHUP 重载、SIGUSR1 统计
3. **守护进程化** — pidfile 写入/删除、`-d` fork 模式
4. **追踪日志** — `tracing` + `tracing-subscriber` 替代裸 `eprintln!`
5. **Telemetry** — 原子计数器，运行时可观测
6. **并行应用** — `--parallel` worker 线程池（大进程 512 线程长尾优化）

---

## 二、P4 不变与前置依赖

| 组件 | 状态 | P4 操作 |
|---|---|---|
| `core/` (topology/config/ruleset/event/policy/engine/store/tracker/proc/caps) | ✅ P0-P2 稳定 | 不改 |
| `daemon/proc_source.rs` | ✅ P2 | 不改 |
| `daemon/orchestrator.rs` | ⏳ P3 骨架 | 不改 |
| `daemon/ebpf_source.rs` | ⏳ P3 | 不改 |
| `crates/ebpf/` | ⏳ P3 | 不改 |
| `policy.rs` EINVAL dedup | ✅ 刚修复 | 不改 |
| `topology.rs` CPU 集群检测 | ✅ 刚修复 | 不改 |

### 2.5 P4 前置已实施：线程亲和性锁定修复

P4 开始前，在真实设备（SM8550，1+4+3 三集群）上测试发现两类问题：

**问题 1**：`setaffinity EINVAL` 刷屏 — 规则要求的 CPU 部分被 Android cgroup（foreground/top-app）排除。

**问题 2**：线程被系统 cpuset 锁定 — 某些线程的 `Cpus_allowed` 被限制到特定集群
（如 `3-6`），规则请求 `0-6` 时内核静默降级。

**修复方案（三层过滤 + 顺序调整）**：

```
apply_affinity(tid, target_cpus):
  ① 写 cpuset/tasks — 移入 threadctl cgroup（放松 Android 限制）
  ② getaffinity 短路检查
  ③ online_cpus 过滤 — 排除离线核
  ④ read_allowed_mask(tid) — 读 /proc/<tid>/status Cpus_allowed_list
  ⑤ target ∩ online ∩ allowed → effective
  ⑥ 交集为空 → 跳过 + 诊断日志（每 tid 仅一次）
  ⑦ 交集与目标不同 → 降级日志
  ⑧ setaffinity(effective)
```

**新增 API**：
- `CpuSet::and(&mut, &CpuSet)` — 位与交集
- `CpuSet::read_allowed_mask(tid)` — 读 `/proc/<tid>/status` 解析 `Cpus_allowed_list`

**实际设备验证结果**（SM8550, Android 14, root）：
```
CPU 拓扑: 8 present, cpuset 可用
  Little 集群: 0-2 (capacity=280)
  Big 集群: 3-6 (capacity=855)
  Prime 集群: 7 (capacity=1024)

setaffinity(tid=7428) 跳过: 目标 CPU 7 全部被 cgroup 排除 (Cpus_allowed=0-6)
setaffinity(tid=27480) 降级: 目标 0-6 → 实际 3-6 (cgroup Cpus_allowed=3-6)
...
事件: 1 条, 应用 117 个线程
relock: 重应用 109 个线程
```

- **零 EINVAL** — 刷屏错误已根除
- **CPU 集群自动识别** — Little/Big/Prime 正确分组
- **cgroup 限制透明诊断** — 用户可看到每个被锁定线程的具体原因
- **运行稳定** — relock 计数波动在正常范围（109→119→116）

## 三、逐模块设计

### 3.1 IPC 控制面（`daemon/src/ipc.rs`）

Unix domain socket JSON-line 协议，与 v1 兼容：

| 命令 | 参数 | 响应 | 实现 |
|---|---|---|---|
| `status <pid>` | PID | `{"pid":N,"pkg":"…","threads":N,"cpuset_dir":"…"}` | ✅ |
| `status <pkg>` | 包名 | `{"pkg":"…","processes":[{"pid":N,"threads":N},…]}` | ✅ |
| `dump` | — | `{"tracked":N,"cpuset_refs":{…},"telemetry":{…}}` | ✅ |
| `reload` | — | `{"ok":true}` → 通知主循环触发 reload | ✅ |
| `apply <pid> <cpus>` | PID+范围 | 临时 setaffinity（不走规则引擎） | ⏳ P5 |

**共享数据访问**：IPC 线程通过 `Arc<Mutex<StateTracker>>` 和
`Arc<ConfigStore>` 读取状态（只读查询），reload 通过 channel 通知主循环。

**鉴权**（Q4 决策）：默认不鉴权（本地 socket 即为信任边界）。
可选 `SO_PEERCRED` 检查，通过配置开关 `ipc_require_root = true` 启用（P4 骨架，P5 实现）。

**socket 生命周期**：
- 启动时创建、bind、listen
- 主循环 tick 前 accept（非阻塞，透传给 worker 线程）
- SIGTERM 时 close + unlink

### 3.2 信号处理（`daemon/src/daemonize.rs`）

| 信号 | 动作 | 实现方式 |
|---|---|---|
| SIGTERM / SIGINT | 优雅退出 | `signal_hook` → channel → Orchestrator 退出循环 |
| SIGHUP | 强制配置重载 | channel → Orchestrator 调用 `store.reload()` |
| SIGUSR1 | 打印 telemetry 快照 | channel → dump 到 stderr/logcat |
| SIGCHLD | 忽略（P4） | `signal_hook::flag::register(SIGCHLD, …)` 或直接 `SIG_IGN` |

**优雅退出流程**：
```
SIGTERM → channel tx
         → Orchestrator 收
         → source.shutdown()（停 eBPF reader / proc 扫描）
         → ipc.close() + unlink socket
         → tracker.clear_all()（释放全部 cpuset 引用）
         → remove_pidfile()
         → exit(0)
```

**信号实现选择**：`signal_hook` crate（纯 Rust，无需 unsafe libc `sigaction`）。
备选：libc `sigaction` + self-pipe（零依赖）。
P4 用 `signal_hook`，体积小（~20KB），API 简洁。

### 3.3 守护进程化（`daemonize.rs`）

**pidfile**：
- `write_pidfile(path)`：写当前 PID，排他创建（`O_CREAT|O_EXCL`），已存在则报错退出
- `remove_pidfile(path)`：SIGTERM 时清理
- `read_pidfile(path)`：`--status` 时读取，检查进程是否存活

**`-d` 模式**（P4 骨架，完整实现留 P5）：
```rust
if opts.daemonize {
    // fork() → parent exit(0) → child setsid()
    // 关闭 stdin/stdout/stderr，重定向到 /dev/null
    // 写 pidfile
}
```

### 3.4 追踪日志（`core/src/logging.rs` + daemon）

**替换目标**：所有 `eprintln!` / `println!` → `tracing` 宏。

| 旧代码 | 新代码 | 级别 |
|---|---|---|
| `println!("配置热加载: …")` | `info!("配置热加载: …")` | INFO |
| `eprintln!("警告: …")` | `warn!("…")` | WARN |
| `eprintln!("setaffinity(tid={}) …")` | `warn!(tid, "setaffinity EINVAL")` | WARN |
| `eprintln!("配置重载失败: …")` | `error!("配置重载失败: {e}")` | ERROR |
| 无 | `debug!("relock 扫描: {} 进程", n)` | DEBUG |

**初始化**（daemon 启动时）：
```rust
use tracing_subscriber::{fmt, EnvFilter};

let filter = EnvFilter::try_from_env("THREADCTL_LOG")
    .unwrap_or_else(|_| EnvFilter::new("info"));

fmt()
    .with_env_filter(filter)
    .with_target(false)
    .with_timer(fmt::time::uptime())  // 用守护进程 uptime 作为时间戳
    .init();
```

**Android logcat 集成**（P4 骨架，完整实现 P5）：
- 用 `tracing-android` crate（可选依赖，`#[cfg(target_os = "android")]`）
- P4 阶段：若 `tracing-android` 可用则自动启用，否则退回到 stderr

**依赖**：`tracing = "0.1"`, `tracing-subscriber = { version = "0.3", features = ["env-filter", "time"] }`。

### 3.5 Telemetry（`core/src/telemetry.rs`）

```rust
pub struct Telemetry {
    pub events_total: AtomicU64,
    pub events_fork: AtomicU64,
    pub events_exec: AtomicU64,
    pub events_thread_clone: AtomicU64,
    pub events_exit: AtomicU64,
    pub applied_total: AtomicU64,
    pub short_circuit_total: AtomicU64,   // getaffinity 短路命中
    pub esrch_total: AtomicU64,           // ESRCH（线程退出）
    pub einval_total: AtomicU64,          // EINVAL（内核限制）
    pub cgroup_blocked_total: AtomicU64,  // cgroup 排除 CPU
    pub eperm_total: AtomicU64,           // EPERM（权限不足）
    pub ebpf_degradations: AtomicU64,     // eBPF 降级次数
    pub config_reloads: AtomicU64,        // 配置重载次数
    pub relocks: AtomicU64,               // relock 执行次数
    pub relock_applied: AtomicU64,        // relock 实际 apply 数
    pub uptime_secs: AtomicU64,           // 进程运行秒数（单调时钟）
}

pub struct TelemetrySnapshot {
    // 与 Telemetry 字段一一对应的普通类型
}
```

**全局单例**：`static TELEMETRY: Telemetry`（`const fn new()` + 零初始化）。

**集成点**：
- `handle_events`：++events_*, ++applied_total
- `apply_affinity`：++short_circuit_total（getaffinity 命中）, ++esrch_total, ++einval_total, ++eperm_total
- `orchestrator`：++ebpf_degradations, ++config_reloads, ++relocks, ++relock_applied
- 主循环每 tick：`TELEMETRY.uptime_secs.store(now, Relaxed)`

**SIGUSR1 输出**：
```
threadctl telemetry:
  uptime: 3600s
  events: total=12345 fork=1203 exec=45 thread_clone=11097 exit=0
  applied: total=12345 short_circuit=11000 esrch=12 einval=0 cgroup_blocked=5 eperm=0
  ebpf_degradations: 2  config_reloads: 5
  relocks: 7200 (applied 14400)
  tracked_processes: 3  cpuset_refs: {"0-3": 2}
```

### 3.6 并行应用（`--parallel`）

**问题**：串行 `for tid in tids { apply_thread(tid) }`，512 线程进程的长尾可达 100ms+。

**方案**：`std::thread::scope` + 固定 worker 数（默认 CPU 核数，`--parallel N` 覆盖）。

```rust
pub fn apply_threads_parallel(
    tids: &[i32],
    resolve: impl Fn(i32) -> Option<Policy>,
    topo: &CpuTopology,
    rt_allowed: bool,
    worker_count: usize,
) -> usize {
    if worker_count <= 1 || tids.len() < 4 {
        return apply_threads_serial(tids, resolve, topo, rt_allowed);
    }
    let counter = AtomicUsize::new(0);
    std::thread::scope(|s| {
        let chunk_size = (tids.len() + worker_count - 1) / worker_count;
        for chunk in tids.chunks(chunk_size) {
            s.spawn(|| {
                for &tid in chunk {
                    if let Some(pol) = resolve(tid) {
                        if !apply_thread(tid, &pol, topo, rt_allowed) {
                            counter.fetch_add(1, Relaxed);
                        }
                    }
                }
            });
        }
    });
    counter.load(Relaxed)
}
```

**注意**：
- `ThreadNameCache` 不能被多线程同时访问（当前 `HashMap` + `get_or_read` 可变借用）
- 并行模式下读线程名需要**提前预取**或使用 `RwLock<HashMap>` 保护
- **P4 策略**：并行模式下跳过线程名缓存（直接读 /proc，避免同步开销）。
  缓存 miss 开销在并行场景下被分摊，且 512 线程只发生在少数进程
- `--parallel` 默认关闭（`worker_count = 1`），需要时手动启用
- `--parallel 0` 或 `--parallel auto` → 自动检测 CPU 核数

**性能预期**：512 线程从 ~100ms（串行）降到 ~15-20ms（8 核并行）。

---

## 四、P4 新增/修改文件

| 文件 | 操作 | 行数（估） | 内容 |
|---|---|---|---|
| `core/src/telemetry.rs` | 新增 | 120 | Telemetry 结构 + 全局单例 + snapshot |
| `core/src/logging.rs` | 新增 | 30 | tracing 初始化封装 |
| `daemon/src/ipc.rs` | 新增 | 200 | Unix socket listen + accept + 命令解析 + 响应 |
| `daemon/src/daemonize.rs` | 新增 | 100 | 信号安装 + pidfile + daemonize |
| `daemon/src/main.rs` | 重写 | 200 | 装配 IPC/信号/telemetry/并行 + Orchestrator |
| `crates/core/Cargo.toml` | 修改 | +2 | 加 tracing 依赖（可选 feature flag） |
| `crates/daemon/Cargo.toml` | 修改 | +5 | 加 tracing-subscriber, signal-hook |

### 4.1 依赖变更

**core**:
```toml
[dependencies]
tracing = { version = "0.1", optional = true }
```
(telemetry 计数不需要 tracing，只有 `trace!`/`debug!` 宏需要)

**daemon**:
```toml
[dependencies]
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "time"] }
signal-hook = "0.3"
```

### 4.2 eprintln! → tracing 迁移清单

| 源文件 | 旧调用数 | 操作 |
|---|---|---|
| `core/src/policy.rs` | 4 | eprintln! → warn!/error! |
| `core/src/store.rs` | 4 | eprintln! → warn! |
| `core/src/config.rs` | 2 | eprintln! → warn! |
| `core/src/engine.rs` | 0 | 新增 debug! 日志 |
| `core/src/tracker.rs` | 2 | eprintln! → warn! |
| `daemon/src/proc_source.rs` | 0 | 不变 |
| `daemon/src/main.rs` | 6 | println!/eprintln! → info!/warn! |
| `daemon/src/ipc.rs` | 3 | eprintln! → error! |
| `daemon/src/daemonize.rs` | 2 | eprintln! → error! |

---

## 五、与 既有实现 的运维能力对比

| 维度 | 既有实现 | threadctl v2 P4 |
|---|---|---|
| 运行时查询 | 无 | IPC status/dump |
| 动态重载 | inotify 自动 | inotify + IPC reload + SIGHUP |
| 优雅退出 | 无信号处理 | SIGTERM/INT → cleanup → exit |
| 日志 | println! 到 stderr | tracing 结构化日志 + logcat |
| 可观测性 | 无 | telemetry 计数器 + SIGUSR1 dump |
| daemon 化 | 无 pidfile | pidfile + -d 模式 |
| 线程亲和性锁定 | 无感知（静默失败） | 三层过滤 + cgroup 透明诊断 + 零 EINVAL |
| 并行应用 | 串行 | --parallel worker 池 |
| EINVAL | 无处理 | 在线 CPU 过滤 + cgroup 交集中断 + 去重诊断 |

---

## 六、待审查的架构决策

1. **signal_hook vs libc sigaction**：`signal_hook` crate 纯 Rust（基于 `sigaction` + self-pipe），
   体积 ~20KB，API 简洁。但增加一个依赖。备选方案：直接用 libc `sigaction` + 自建 pipe
   （代码 ~30 行，零依赖）。倾向哪个？
2. **tracing 日志体积**：tracing + tracing-subscriber 增加 ~200KB 二进制体积
   （release LTO 后 ~50KB）。Android Magisk 模块场景可接受？还是保持 eprintln +
   P5 再加？
3. **IPC reload 与 inotify 的交互**：IPC reload 命令应触发一次即时重载（调用
   `store.reload()`），但不应干扰 inotify 监听（两者并存）。实现：reload 命令
   直接调用 store.reload()，成功后在主循环中发配置变更通知。是否需要防抖动
   （inotify 刷新文件时可能同时触发 IPC reload 和 inotify 事件）？
4. **并行模式下的 cpuset tasks 写入**：多线程并行写同一个
   `/dev/cpuset/threadctl/<range>/tasks` 文件——Linux 允许并发 append。
   是否存在 file position 竞争？内核 `write()` 对 `/dev/cpuset/*/tasks`
   是一次性写一个 PID（echo 写入），每次 `write()` 是原子的。
   多线程并发写入应安全（各写各的 tid）。
5. **telemetry 全局单例的测试隔离**：`static TELEMETRY` 在单元测试间共享状态。
   是否需要 `reset()` 方法或 `#[cfg(test)]` 的 isolate 机制？

---

## 七、进度时间线

| 日期 | 事件 |
|---|---|
| 2026-08-07 | P0-P2 完成（workspace + 配置 + proc 全链路），Claude P0 审查通过 |
| 2026-08-07 | P3 架构规划完成（内核文档研究 + eBPF/Orchestrator/Hybrid 设计） |
| 2026-08-07 | P4 文档生成（初版） |
| 2026-08-08 | 真实设备 SM8550 压测：EINVAL 刷屏 + cgroup 锁定问题暴露 |
| 2026-08-08 | 三层过滤修复（online + allowed交集 + cpuset先行）+ 设备验证通过 |
| 2026-08-08 | P4 文档合入：更新 affinity 修复、telemetry 计数器、设备结果 |
