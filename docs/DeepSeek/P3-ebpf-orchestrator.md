# threadctl-rs — P3 最终架构审查文档

> 供 Claude 审查。P3 目标：**彻底完善所有架构骨架** — eBPF 事件源迁移、
> Orchestrator 状态机、Hybrid 双源模式、IPC/信号/守护进程化/可观测性。
> 本文档基于 Linux 内核官方文档与 AOSP 资料重新审视整套设计。

---

## 一、内核知识汇整（研究结论）

> 以下各节标注了内核文档来源。本程序依赖这些机制，理解其边界是设计的前提。

### 1.1 cpuset 与 sched_setaffinity 的交互规则

来源：`kernel.org/doc/html/latest/admin-guide/cgroup-v1/cpusets.html`

- cpuset 是 CPU + NUMA 内存节点的**硬边界**：任务不能逃离其所属的 cpuset
- `sched_setaffinity` 的结果被 cpuset 的 `effective_cpus` **隐性取交集**
- 这意味着：**仅做 setaffinity 是不够的**——如果 cpuset 只有 0-3 核，
  即使 setaffinity 设为 0-7，实际只生效 0-3
- **threadctl 的双通道策略（setaffinity + 写 cpuset/tasks）因此是必要的**：
  通过写入专用的 `/dev/cpuset/threadctl/<range>/tasks`，从 cgroup 层面同时约束，
  确保 setaffinity 不被 cpuset 暗地截断

### 1.2 Android 上的 cpuset 动态重分配（relock 根源）

来源：`source.android.com/docs/core/perf/cgroups`、
`androidmobiles.org` 关于 Android cgroup v2 Task Profiles 的文章

- Android 10+ 使用 **Task Profiles**（cgroup 抽象层），ActivityManager 按
  应用重要性（top → foreground → background）自动将进程移入不同 cpuset
- 这意味着：**守护进程即使设置了正确的亲和性，AMS 后续可以通过 cgroup 迁移覆盖它**
- **`lock_interval`（周期重锁定）是 Android 上的刚需** —— v1 继承的设计是正确的
- cgroup v2（Android 12+ 逐步采用）使用统一层次（`/sys/fs/cgroup/`），
  但兼容模式下 `/dev/cpuset` 仍存在
- **需同时支持 cgroup v1（/dev/cpuset）和 v2（/sys/fs/cgroup）**
  自动检测路径（P3 骨架，P4 实现）

### 1.3 sched_setaffinity 的权限模型

来源：`man7.org/linux/man-pages/man2/sched_setaffinity.2.html`

- 操作**自己**的线程：任何进程均可（无需特殊权限）
- 操作**其他进程**的线程：
  - 同 UID 或具有 `CAP_SYS_NICE`：**允许**
  - 否则：`EPERM`
- 在 Android/Magisk 模块场景，守护进程以 **root** 运行 → 无权限问题
- 桌面 Linux 场景，守护进程以普通用户运行 → 仅对同 UID 进程有效
- **P2 的 EPERM 告警逻辑因此是有值的**：非 root 时显式通知用户

### 1.4 sched_setscheduler (RT) 的权限模型

来源：`man7.org/linux/man-pages/man2/sched.7.html`

- `SCHED_FIFO` / `SCHED_RR` 需要 `CAP_SYS_NICE`
- non-root 进程即使对自己调用 `sched_setscheduler(fifo)` 也返回 `EPERM`
- **Q6 权限检查（P2 caps.rs）的 `capget` 方案是正确的**：在 Android
  上 init 阶段检测 CAP_SYS_NICE 位，提前宣告"fifo/rr 将不可用"，而非
  等到每个应用调用都失败一次

### 1.5 sched_switch tracepoint 格式与 eBPF 性能

来源：`include/trace/events/sched.h`（torvalds/linux）

```
TP_PROTO(bool preempt, struct task_struct *prev, struct task_struct *next)
TP_ARGS(preempt, prev, next)

TP_STRUCT__entry(
    __array(char, prev_comm, TASK_COMM_LEN)
    __field(pid_t, prev_pid)
    __field(int, prev_prio)
    __field(long, prev_state)
    __array(char, next_comm, TASK_COMM_LEN)
    __field(pid_t, next_pid)
    __field(int, next_prio)
)
```

- **sched_switch 在每次上下文切换时触发**（CPU 密集型路径，每核每秒数千次）
- eBPF 挂载此 tracepoint 必须**极度轻量**：仅白名单 PID 检查 + 采样防抖
- 线程被调度到的 **CPU 编号**不直接在 tracepoint 字段中，需
  用 `bpf_get_smp_processor_id()` 获取
- **设计中采用低频采样模式**：每进程每秒最多 1 次 CpuMigrate 事件，
  通过 BPF `LruHashMap<pid, last_ns>` 防抖（DEDUP_MAP 模式）

### 1.6 sched_process_fork 的线程语义

来源：内核源码

- `sched_process_fork` 在 `clone()` / `fork()` 时触发（均在 `_do_fork` 内）
- `pthread_create` 也走 `clone()` → **fork tracepoint 同时捕获线程创建**
- fork tracepoint 的 `child_pid` 是子进程（/线程）的 PID/TID
- 因此 **eBPF 事件源用 fork tracepoint 就能发现新线程，无需额外的 clone 钩子**

---

## 二、P3 完整架构（最终版）

```
┌───────────────────────────────────────────────────────────────┐
│  threadctl (daemon)                                           │
│                                                               │
│  main.rs                                                      │
│  ├─ CLI > ConfigStore > Orchestrator::new(…) → .run()        │
│  │                                                            │
│  ┌──────────────── Orchestrator ───────────────────────────┐ │
│  │                                                          │ │
│  │  状态机: Init → (Ebpf|Proc|Hybrid) → Degraded → Stop    │ │
│  │                                                          │ │
│  │  loop {                                                  │ │
│  │    signal_rx ? → 停源 / 关 IPC / 写 pidfile / 退出      │ │
│  │    reload_rx ? → 全量重扫 (relock_all)                   │ │
│  │    lock_interval → relock_all                            │ │
│  │    dead_interval → cleanup_dead                          │ │
│  │    events = source.poll(deadline)                        │ │
│  │    engine.handle_events(tracker, events, cfg, topo)      │ │
│  │    telemetry.tick()                                      │ │
│  │  }                                                       │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                                               │
│  ipc.rs            信号处理        proc_source.rs             │
│  Unix socket       daemonize.rs    ebpf_source.rs             │
│  status/reload/    SIGTERM/INT/    (EventSource 双实现)       │
│  dump/apply        HUP/USR1                                  │
└───────────────────────────────────────────────────────────────┘
```

### 2.1 Orchestrator 状态机

```
                    ┌─────────┐
                    │  Init   │
                    └────┬────┘
                         │ engine.mode
            ┌────────────┼────────────┐
            ▼            ▼            ▼
    ┌───────────┐ ┌───────────┐ ┌───────────┐
    │  EbpfMode │ │  ProcMode │ │ HybridMode│
    └─────┬─────┘ └───────────┘ └─────┬─────┘
          │ ebpf 通道断开              │ ebpf 组件失败
          ▼                            ▼
    ┌──────────────┐          ┌──────────────┐
    │  Degraded    │──────────▶  ProcMode    │
    │ (自动回退)   │  回退完成  │              │
    └──────────────┘          └──────┬───────┘
                                     │ SIGTERM/SIGINT
                                     ▼
                               ┌──────────┐
                               │ Stopping │
                               └────┬─────┘
                                    │ 清理完成
                                    ▼
                               ┌──────────┐
                               │ Stopped  │
                               └──────────┘
```

- **Auto** 模式：Init 尝试 eBPF → 不可用则降级 Proc；中途 eBPF 断开 → Degraded → Proc
- **Hybrid** 模式：eBPF 主源 + ProcSource 低频补漏**同时运行**，
  任一组件独立失败不影响另一个（eBPF 挂 → 仍在 Proc 兜底）
- **Degraded 状态**：记录降级原因到 telemetry，eBPF 失败不清空 tracker（已有状态无缝衔接 proc）
- 降级时 ProcSource 需**全量扫描一次**（`scan_all=true`）以补 eBPF 空窗

### 2.2 EbpfSource 设计

**用户态**（`daemon/src/ebpf_source.rs`）：

- 实现 `EventSource` trait
- 持有 `aya::Ebpf` + `mpsc::Receiver<EbpfProcEvent>`（RingBuf reader 线程）
- `poll(deadline)`：从 channel 非阻塞收事件 → 转换为 `ProcessEvent`
- `on_config_changed`：重建 `TARGET_COMM_MAP`（白名单），触发全量扫描
- 初始化失败返回 `None`（调用方按 engine.mode 决定降级或退出）

**内核态**（`crates/ebpf/src/main.rs`）：

- **实现** fork tracepoint（`sched_process_fork`）+ 白名单 + 防抖 + RingBuf
- **实现** exec tracepoint（`sched_process_exec`）+ 白名单检查
- **预留** sched_switch 采样（P5）——在文件末尾以注释形式留下接口
- 白名单 map 动态容量：ConfigStore 计算后通过 `EbpfLoader::set_max_entries` 传入
- 防抖 map（DEDUP_MAP）：LruHashMap<pid, (last_ns, count)>，0.1s 窗口

**事件结构**～ EbpfProcEvent（与内核 `ProcEvent` 布局一致）：
```rust
#[repr(C)]
struct EbpfProcEvent {
    pid: i32,          // FORK: child_pid；EXEC: tgid
    tid: i32,          // FORK: child_pid；EXEC: pid
    child_pid: i32,    // FORK: 子进程 PID；EXEC: 0
    comm: [u8; 16],    // FORK: child_comm；EXEC: bpf_get_current_comm()
    event_type: u32,   // 1=FORK, 2=EXEC
}
```
转换为 `ProcessEvent` 时：FORK → `ProcessEvent::fork(child_pid, child_pid)`；
EXEC → `ProcessEvent::exec(pid, tid)`；pkg 由 engine 后续读 /proc 补。

### 2.3 IPC 骨架

Unix domain socket 监听，JSON-line 协议（v1 延续）：

| 命令 | 参数 | 响应 | 实现阶段 |
|---|---|---|---|
| `status <pid>` | PID | `{"pid":…,"pkg":…,"threads":…}` | P4 |
| `status <pkg>` | 包名 | `{"pkg":"…","processes":[…]}` | P4 |
| `dump` | — | `{"tracked":…,"cpuset_refs":…}` | P4 |
| `reload` | — | `{"ok":true}` → 触发 SIGHUP 等价逻辑 | P4 |
| `apply <pid> <cpus>` | PID+CPU 范围 | 临时应用（不走规则引擎） | P5 |

P3 骨架：创建 socket、accept 循环、命令解析、空响应返回。

### 2.4 信号处理 / 守护进程化

| 信号 | 动作 |
|---|---|
| SIGTERM / SIGINT | 退出循环 → shutdown(源) → 清 tracker（cpuset 引用释放）→ 删 pidfile → exit |
| SIGHUP | 强制触发 ConfigStore::reload（绕过 inotify）+ telemetry 打印 |
| SIGUSR1 | telemetry 输出到 stderr / logcat（调试用） |

`daemonize.rs`：
- `write_pidfile(path)` / `remove_pidfile(path)`
- `install_signal_handlers()` 返回 `mpsc::Receiver<Signal>`（用 `signal_hook` crate 或 libc `sigaction` + self-pipe）
- `daemonize()`：fork + setsid + 关闭 stdin/stdout（`-d` 模式，P4 实现）

### 2.5 Telemetry 与日志

**telemetry**（`core/src/telemetry.rs`）：
```rust
pub struct Telemetry {
    pub events_total: AtomicU64,
    pub events_fork: AtomicU64,
    pub events_exec: AtomicU64,
    pub events_thread_clone: AtomicU64,
    pub events_exit: AtomicU64,
    pub applied_total: AtomicU64,
    pub short_circuit_total: AtomicU64,  // getaffinity 短路
    pub esrch_total: AtomicU64,
    pub eperm_total: AtomicU64,
    pub ebpf_degradations: AtomicU64,
    pub config_reloads: AtomicU64,
    pub relocks: AtomicU64,
    // …
}
impl Telemetry { pub fn snapshot(&self) -> TelemetrySnapshot { … } }
```
全局单例 `static TELEMETRY: Telemetry = Telemetry::new();`

**日志**：引入 `tracing` + `tracing-subscriber` 替代所有 `eprintln!/println!`。
- P3 骨架：`tracing_subscriber::fmt().init()`，`info!/warn!/error!/debug!` 宏替换
- P4：Android logcat layer（`tracing-android` crate）

### 2.6 cgroup v1/v2 自适应路径

当前 `BASE_CPUSET = "/dev/cpuset/threadctl"`（cgroup v1）。
P3 骨架加入自动检测（`topology.rs` 新增 `detect_cpuset_path`）：

```rust
fn detect_cpuset_path() -> Option<&'static str> {
    if Path::new("/dev/cpuset").exists() { Some("/dev/cpuset") }    // v1
    else if Path::new("/sys/fs/cgroup/cpuset").exists() { Some(…) }  // v2 或在 /sys/fs/cgroup
    else { None }
}
```

具体 v2 支持在 P4 完成（v2 使用 `cgroup.procs` 文件 + `cpuset.cpus` 配置，
接口差异大），P3 骨架保留检测 + 打印警告。

---

## 三、P3 实施计划

### 3.1 新增/修改文件

| 文件 | 操作 | 内容 |
|---|---|---|
| `core/src/telemetry.rs` | 新增 | Telemetry 原子计数器 |
| `core/src/logging.rs` | 新增 | 日志宏封装（tracing facade） |
| `daemon/src/orchestrator.rs` | 新增 | Orchestrator 状态机 + 主循环 |
| `daemon/src/ebpf_source.rs` | 新增 | EbpfSource（EventSource impl + aya 加载） |
| `daemon/src/ipc.rs` | 新增 | Unix socket IPC 骨架 |
| `daemon/src/daemonize.rs` | 新增 | 信号处理 + pidfile |
| `daemon/src/main.rs` | 重写 | 装配 Orchestrator + 精简 main |
| `crates/ebpf/src/main.rs` | 重写 | fork/exec 内核程序 + sched_switch 预留 |
| `crates/core/src/topology.rs` | 修改 | detect_cpuset_path + cgroup v2 检测骨架 |
| `Cargo.toml`（daemon） | 修改 | 加 aya, signal-hook, tracing 依赖 |

### 3.2 实施顺序

1. **基础设施**：tracing 日志（`logging.rs`）→ telemetry（`telemetry.rs`）
2. **Orchestrator**：状态机骨架 → 信号处理（`daemonize.rs`）→ IPC 骨架
3. **eBPF**：内核态程序迁移 → `EbpfSource` 用户态
4. **集成**：`main.rs` 装配 → 编译验证 → Hybrid 冒烟测试（eBPF + proc 同时运行）

---

## 四、待审查的架构决策

1. **cgroup v1/v2 双路径支持**：目前 hardcode `/dev/cpuset`（v1）。
   P3 加检测骨架，P4 实现 v2（`/sys/fs/cgroup/` 路径 + `cpuset.cpus`/`cgroup.procs`）。
   是否应在 P3 就完成 v2 支持，还是 P4 与更完整的 cgroup 管理一并做？
2. **tracing crate 引入时机**：P3 引入 tracing 替换 eprintln，增加约 200KB 依赖体积。
   在 Android/Magisk 模块场景是否可接受？还是保持 eprintln + P4 再加？
3. **sched_switch 采样频率**：每进程每秒最多 1 个 CpuMigrate 事件。
   Android 大核/小核调度频率高，1/s 的纠正延迟是否足够及时？
   是否需要增加采样密度（如每进程每 200ms）同时配合更强的防抖？
4. **IPC 协议安全**：Unix socket 无鉴权，任何本地进程可发送 reload/dump 命令。
   是否需要引入简单鉴权（如 fd 凭证检查 `SO_PEERCRED`，只允许 UID 0 或
   特定 GID）？还是接受"本地 IPC 即为信任边界"？
5. **aya 版本选择**：锁 aya 0.13（API 可能变动）。
   确认使用哪个版本并锁定 workspace Cargo.toml。内核态程序也依赖 aya-ebpf
   匹配版本。

---

## 五、与 P0-P2 的衔接

- P0 domain types（CpuSet/Topology/Event/Policy/RuleSet/ConfigSnapshot）**不变**
- P1 ConfigStore **不变**（eBPF 白名单配置更新通过 `on_config_changed` → `setup_target_map`）
- P2 StateTracker + engine + ProcSource **不变**（EbpfSource 产同一事件流，engine 统一处理）
- `EventSource` trait **不变**（ProcSource 和 EbpfSource 均实现）
- Hybrid 模式下 tracker 被**两个源共享**（`Arc<Mutex<StateTracker>>`），锁竞争在单线程
  poll 循环下为零（proc poll → 释放锁 → ebpf poll 事件处理 → 释放锁，串行化执行）
