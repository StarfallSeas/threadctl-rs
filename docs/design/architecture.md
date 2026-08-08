# threadctl-rs — 架构设计文档（v2.0.0）

> 本文档描述 threadctl-rs 全新重写版的架构规划与实现现状（P0 + P1 已完成）。
> 由 DeepSeek-V4 生成并持续更新，供 Claude 审核与确认。
>
> 上下文：本程序**不是** 既有实现 的延续版本，而是借鉴 既有实现 部分已验证的设计思路，
> 结合原 threadctl v1 的既有方案，全新设计的一个事件驱动的 Linux/Android 线程调度优化守护进程。
> eBPF 内核态保留并扩展。

---

## 1. 目标定位

- 一个**事件驱动**的 Linux/Android **线程调度优化守护进程**。
- 核心是一条 **事件 → 规则 → 策略执行** 管线；CPU 亲和性是第一个策略，
  未来冻结(nice)、cgroup 迁移、调度策略(sched/nice)等走同一管线。
- eBPF 内核态作为主要事件源；无 eBPF（缺 BTF/加载失败）时**自动降级**到 /proc 轮询。
- Android 优先（Magisk 模块形态），同时保持通用 Linux 可编译可运行。

---

## 2. 设计来源

### 通用实践（已验证、保留）

1. **双模式降级链**：eBPF 不可用 → /proc 轮询；每层失败都有一级降级。
2. **inotify 热加载 → 轮询兜底**：配置变更用 inotify 即时感知，
   watch 丢失自动重装，重装失败降级轮询。
3. **应用前 `sched_getaffinity` 短路**：已符合目标则零开销返回。
4. **cpuset 双通道**：`sched_setaffinity` + 写 `/dev/cpuset/<dir>/tasks`。
5. **eBPF 白名单前缀/后缀 8 字节匹配** + LruHashMap 防抖 + RingBuf 事件。
6. **栈上路径缓冲**、`fnmatch` 线程名匹配、CPU 范围解析。

### 从 threadctl v1（原目录已备份至 `代码仓库/threadctl-v1-backup.tar.gz`）继承

1. **TOML 配置** + serde：`[daemon]` / `[engine]` / `[[rule]]` 结构，旧配置零迁移。
2. **relock 周期锁定**（既有实现 没有）：周期重应用亲和性，对抗 Android 侧覆盖。
3. **sched 策略**：fifo / rr / batch / idle + nice，不止亲和性。
4. **IPC 控制面**：Unix socket JSON-line 协议（v1 是空壳，v2 完整实现）。

### v1 的教训 → v2 对策

| v1 问题 | v2 对策 |
|---|---|
| main 循环是 if-else 双模式 | `EventSource` trait，Orchestrator 只管状态机 |
| README 承诺的 `DiscoveryEngine` trait 未落地 | 抽象实现在 core，daemon 只组装 |
| IPC 空壳（dump 恒 0） | 挂到 Tracker/ConfigStore 真实数据上 |
| 无信号处理 | SIGTERM/INT 优雅退出、SIGHUP 重载、SIGUSR1 统计 |
| pid_file 只配置不写 | 启动写入、退出删除 |
| cpuset 目录只建不删（既有实现 同样缺陷） | Tracker 引用计数归零 → rmdir 回收 |

---

## 3. 架构分层

```
┌──────────────────────────────────────────────────────────────┐
│  threadctl (daemon, bin)                                      │
│  CLI / pidfile / 信号 / Orchestrator 状态机                   │
│  Init → (Ebpf|Proc|Hybrid) → Degraded → Stopped               │
├──────────────────────────────────────────────────────────────┤
│  threadctl-core (lib, 纯逻辑可单测，不依赖 aya)                │
│  ┌───────────┐  ┌───────────┐  ┌───────────┐  ┌──────────┐  │
│  │ ConfigStore│ │  RuleSet   │ │  Tracker   │ │  Policy  │  │
│  │ 热加载+快照│ │ 编译索引   │ │ 状态+缓存  │ │ 执行动作 │  │
│  └─────┬─────┘  └─────┬─────┘  └─────┬─────┘  └────┬─────┘  │
│  ┌─────┴──────────────────────────────────────────────┴─────┐  │
│  │                     EventSource (trait)                  │  │
│  │              poll() → Vec<ProcessEvent>                  │  │
│  └─────┬───────────────────────────────────────────┬───────┘  │
│   ProcSource                                   EbpfSource    │
│  (增量扫描+存活检查)                          (RingBuf 读线程)│
├──────────────────────────────────────────────────────────────┤
│  threadctl-ebpf (bin, no_std)                                  │
│  fork/exec 白名单 + 防抖 → RingBuf                             │
│  sched_switch 采样 → CpuMigrate 事件 (P5 扩展)                 │
└──────────────────────────────────────────────────────────────┘

配置文件 ──inotify──▶ ConfigStore ──▶ ConfigChanged ──▶ Orchestrator
                                                          │
  eBPF tracepoint ──RingBuf──▶ EbpfSource ──┐            │ poll 事件
  /proc 轮询 ───────────────▶ ProcSource  ──┴▶ ProcessEvent
                                                          │
                                              RuleSet.resolve(pkg, thread)
                                                          │
                                     Policy（并行）→ setaffinity + cpuset + sched
                                                          │
                                          Tracker 维护缓存/引用计数 ─▶ cpuset 回收
```

---

## 4. 核心设计

### 4.1 统一事件模型

```rust
enum EventKind { Fork, Exec, ThreadClone, CpuMigrate, Exit }

struct ProcessEvent {
    pid: i32,
    tid: i32,
    kind: EventKind,
    cpu: Option<u32>,   // CpuMigrate 目标 CPU
}

trait EventSource: Send {
    fn poll(&mut self, deadline: Instant) -> Vec<ProcessEvent>;
    fn on_config_changed(&mut self, cfg: &Arc<ConfigSnapshot>);
    fn shutdown(&mut self);
}
```

- eBPF 与 /proc 产出**同一事件流**；Orchestrator 只面对 trait。
- **CpuMigrate**（P5）：`sched_switch` 采样，纠正大核↔小核漂移。

### 4.2 引擎模式

`EngineMode::{Auto, Ebpf, Proc, Hybrid}`（config `[engine] mode`）：

- **Auto**：优先 eBPF，失败降级 /proc；降级目标为 Hybrid 语义（eBPF 断开后 /proc 接管）。
- **Hybrid**：eBPF 主源 + /proc 低频补漏同时运行（防白名单漏报）。
- **Ebpf / Proc**：强制指定。

### 4.3 配置系统（解析/编译分离）

- `RawConfig`：serde 反序列化，带默认值，语法错误带位置。
- `ConfigSnapshot`：编译后的**不可变快照**（版本号 + 规则索引），模块间唯一流通形态。
- `ConfigStore`（P1 已实现）：持有当前快照，`current()` O(1) 获取；`reload()` 成功则
  版本 +1 原子替换，**失败保留旧快照**。
- 热加载线程（`spawn_hot_reload`）：inotify 优先、轮询降级，成功重载后 channel 广播版本号。

### 4.4 规则引擎（纯函数，可单测）

- `RuleSet::compile` 编译期校验（长度/CPU 范围/present 过滤），`by_pkg` 索引。
- 语义（既有实现 已验证）：线程规则命中按位或合并；miss → 包级规则按位或合并；
  sched/nice 取先命中。
- **cpuset 目录名由运行时实际合并 CPU 派生**（Claude 审查 ❹ 修复），
  应用前懒创建（`ENSURED_CPUSET_DIRS` 缓存避免热路径重复 syscall）。

### 4.5 策略执行（Policy）

```rust
struct Policy {
    cpus: CpuSet,
    cpuset_dir: String,   // 运行时派生
    sched: Option<SchedPolicy>,   // fifo/rr/batch/idle/other
    sched_prio: Option<i32>,
    nice: Option<i32>,
}
```

`apply_thread`：getaffinity 短路 → setaffinity → cpuset 双通道 → sched/nice。
ESRCH 返回 true 触发重扫；EPERM 显式告警（不再静默）。

### 4.6 StateTracker（P2）

- 统一维护 pid → 扫描状态、线程名 TTL 缓存（每进程 TTL + 全局滑动，exec 主动失效）。
- **cpuset 引用计数**：`dir_name → refcount`，归零 rmdir 回收。

### 4.7 生命周期与可观测性（P4）

- SIGTERM/SIGINT 优雅退出；SIGHUP 重载；SIGUSR1 统计。
- telemetry 原子计数器；tracing 日志（Android 同步 logcat）。

---

## 5. 目录结构

```
threadctl-rs/
├── Cargo.toml            # workspace（core / daemon / ebpf）
├── README.md / README.en.md
├── LICENSE               # GPL-3.0
├── crates/
│   ├── core/             # lib：纯逻辑，不依赖 aya（19 模块，可单测）
│   │   ├── src/
│   │   │   ├── topology.rs     # CpuSet / CpuTopology / 集群检测 / cpuset 目录
│   │   │   ├── event.rs        # EventKind / ProcessEvent / EventSource trait
│   │   │   ├── config.rs       # serde 模型 + ConfigModel AST + ConfigSnapshot
│   │   │   ├── kdl_parser.rs   # KDL → ConfigModel（daemon/engine/app/profile/thread）
│   │   │   ├── profile.rs      # 7 个内置模板
│   │   │   ├── ruleset.rs      # 规则编译索引 + RuleMatch（纯匹配，P6.1 冻结）
│   │   │   ├── merge.rs        # Policy Merge Engine（MERGE_TABLE 策略表，P6.2-1）
│   │   │   ├── policy.rs       # 三层过滤 + ApplyOutcome + uclamp + audit 全路径
│   │   │   ├── engine.rs       # handle_events / relock / cleanup
│   │   │   ├── tracker.rs      # StateTracker + 线程名缓存 + cpuset 引用计数
│   │   │   ├── store.rs        # ConfigStore + inotify 热加载
│   │   │   ├── decision.rs     # TaskIntent / ActionLevel / TaskScore（骨架）
│   │   │   ├── system_context.rs # PSI / thermal / 自适应轮询
│   │   │   ├── capability.rs   # uclamp > schedtune > cpuset 检测
│   │   │   ├── audit.rs        # 256 环形缓冲 + summary_windowed
│   │   │   ├── foreground.rs   # 前台 UID 检测（骨架）
│   │   │   ├── proc.rs         # /proc 工具 + is_alive
│   │   │   ├── caps.rs         # CAP_SYS_NICE 检查
│   │   │   └── lib.rs          # crate 门面
│   │   └── config/threadctl.toml  # TOML 默认模板
│   ├── daemon/            # bin：threadctl
│   │   └── src/
│   │       ├── main.rs        # CLI + 热加载循环 + SystemContext + audit 摘要
│   │       └── proc_source.rs # /proc 事件源（目录计数 + 增量路径）
│   └── ebpf/             # bin：内核态（空骨架，待迁移）
│       └── src/main.rs   # no_std 占位
├── docs/
│   ├── design/           # 架构/阶段设计（本文档）
│   ├── reviews/          # 外部 AI 审查原文（claude/ + chatgpt5.5/）
│   ├── responses/        # DeepSeek 回应/采纳记录
│   ├── matcher.md        # matcher 设计（冻结）
│   ├── repo-overview.md  # 仓库结构概览
│   └── ai-review-process.md
├── examples/             # threadctl.kdl / user-mode.kdl / threadctl.toml
└── scripts/              # i18n-logs.sh / check-cn-logs.py 等
```

---

## 6. 实施路线

| 阶段 | 内容 | 状态 |
|---|---|---|
| **P0** | workspace 骨架 + 领域类型 + 单测基线 | ✅ |
| **P1** | ConfigStore 热加载（inotify + 轮询降级）+ 快照版本化 + daemon 接入 | ✅ |
| **P2** | ProcSource + StateTracker + Policy 执行 + relock + Q6 权限检查 | ✅ |
| **P3** | EbpfSource（fork/exec 迁移）+ Orchestrator 状态机 + Hybrid | ⏳ |
| **P4** | IPC 完整实现 + 信号 + pidfile + 日志 + telemetry + `--parallel` | ⏳ |
| **P5** | 内核态 sched_switch 采样 + CpuMigrate 事件处理 | ⏳ |
| **P6** | Magisk 模块打包 + 更新通道 | ⏳ |
| **P6.0** | Profile 抽象（game/audio/launcher/balanced/power-save）+ 覆盖语义 | ✅ |
| **P6.1** | pkg 通配符（`com.tencent.mm*`）+ MatchPriority + specificity + 实例级缓存 | ✅ |
| **P6.2** | 内置 profile 表（`profile.rs`，5 个模板） | ✅ |

---

## 7. 开放决策（已定案，详见 9.2）

1. `EngineMode::Hybrid` 语义：Auto 降级目标为 Hybrid 语义。✅ 定案
2. 规则优先级字段：P2 前不引入。✅ 定案
3. cpuset 目录共享：运行时派生 + 引用计数。✅ 定案（❹ 已落地）
4. 并行 apply：串行起步，`--parallel` P4。✅ 定案
5. 线程名 TTL 缓存：每进程 TTL + 全局滑动。✅ 定案（Claude 批准）
6. 调度策略权限：P2 Init 阶段检查 CAP_SYS_NICE。✅ 定案
7. 跨平台门禁：compile_error 显式报错。✅ 已落地

---

## 8. 实现现状

### P0（✅）

- 旧目录备份 `代码仓库/threadctl-v1-backup.tar.gz`（99MB/1504 文件）并清空。
- workspace 三 crate；`cargo check --workspace --exclude threadctl-ebpf` 零错误零警告。

### P1（✅）

- `store.rs`：`ConfigStore`（快照 Mutex 持有 + 版本化 reload，失败保留旧快照）、
  `InotifyWatch`（poll + 事件解析 + 失效重装 + 降级）、`spawn_hot_reload` 线程。
- `main.rs`：默认模板生成、初始加载、热加载主循环（P2 由 Orchestrator 接管）。
- **5 个单测通过**：config 2 + store 3（版本递增 / 坏配置保留旧快照 / Arc 共享）。
- **真实冒烟测试通过**：
  - 启动：`CPU 拓扑: 8 present, cpuset 不可用`（termux 无 /dev/cpuset，符合预期）
  - `初始配置加载成功: 版本 1，0 个规则包` + `inotify 已启用`
  - 追加规则到配置文件 → `配置热加载: 版本 2，1 个规则包`（inotify 即时触发）

---

## 9. Claude 审查结论与决策记录

> 审查来源：`reviews/claude/initial.md`。结论：架构方向正确，分层清晰。发现 2 个 P0 Bug、
> 4 个重要设计问题、若干次要问题。**全部已修复并验证。**

### 9.1 已修复项

| # | 问题 | 修复 |
|---|---|---|
| ❶ | `daemon/main.rs` 循环变量缺 `mut`，P1 接 `-c` 后死循环 | 重写完整 CLI，`mut i` 恢复 |
| ❷ | `ruleset.rs` `?` 使 resolve 整体短路 | `let Some(pat) = ... else { continue; }` |
| ❸ | `has_thread_rules` Vec 线性查找 | 改 `HashSet<String>`，O(1) |
| ❹ | 多规则 OR 合并后 `cpuset_dir` 与实际 CPU 不符 | resolve 末尾派生目录名 + policy 懒建目录 |
| ❺ | `setaffinity` 非 ESRCH 错误静默 | EPERM 显式告警，其余打日志 |
| ❻ | `CString::unwrap_or_default()` 回退语义错误 | 统一 `expect()` |
| ❼ | 配置模板缺注释 | 补齐单位/有效值/Android 场景/绝对路径示例 |

### 9.2 设计决策定案

- **Q1**：Auto = 单一主源 + 降级（目标 Hybrid 语义）；Hybrid = 双源并行。均保留。
- **Q2**：优先级字段暂缓，出现真实覆盖需求再引入 `priority: i32`。
- **Q3**：cpuset 目录运行时派生（❹ 已落地），引用计数 P2 实现。
- **Q4**：串行起步，`--parallel` P4（先 telemetry 采集延迟数据）。
- **Q5**：批准每进程 TTL + 全局滑动方案。
- **Q6**：P2 Init 阶段检查 `geteuid()==0 || CAP_SYS_NICE`，无权限跳过 sched 并告警。
- **Q7**：`compile_error!` 已加，非 Linux 直接编译失败。

### 9.3 移入 P2/P4 的遗留项

- Q6 权限检查：P2 Init 阶段。
- Q4 `--parallel`：P4。
- ❺ 错误日志现为 `eprintln!`，P4 接入 tracing + telemetry 计数器。

---

## 10. 进度时间线（向 Claude 同步）

| 日期 | 事件 |
|---|---|
| 2026-08-07 | 完成需求确认：全新程序 threadctl v2（非 既有实现 延续），eBPF 内核态保留并扩展 |
| 2026-08-07 | 备份并清空旧 threadctl-rs，P0 workspace 骨架完成（core/daemon/ebpf） |
| 2026-08-07 | 架构文档生成，交 Claude 审查 |
| 2026-08-07 | Claude 审查返回（reviews/claude/initial.md）：2 Bug + 4 设计问题 + 若干次要，全部修复，Q1-Q7 定案 |
| 2026-08-07 | P1 完成：ConfigStore 热加载（inotify→轮询降级链）+ 快照版本化 + daemon 接入，5 单测 + 真实冒烟通过 |
| 2026-08-07 | 本文档覆盖更新，同步进度（本次） |
| 2026-08-08 | P2 完成：ProcSource + StateTracker + Policy 全链路（含 Q6 权限检查、relock）；三层过滤修复（online + allowed 交集） |
| 2026-08-08 | P5 架构规划 + 五模块（audit/foreground/system_context/capability/decision）落地 |
| 2026-08-08 | KDL 配置格式落地（threadctl.kdl 完整示例 + 裸布尔兼容修复） |
| 2026-08-08 | P6.0/P6.1/P6.2 完成：profile 抽象 + pkg 通配符 + 内置表（35 单测全绿） |
| 2026-08-08 | ChatGPT 审查约束落地：MatchPriority（exact > group > wildcard-specificity > default）+ 实例级缓存 + 6 个 matcher 测试（41 单测全绿） |
| 2026-08-08 | GPT 二次审查落地：specificity 升级 nginx 评分（前缀权重+通配惩罚）、分层 merge（包级覆盖/包内线程 merge）、group 归入 Config Compiler（42 单测全绿） |
| 2026-08-08 | GPT 三次审查落地：**P6.1 冻结**。继承语义（exact override wildcard，来源并存）、RuleMatch{index,source} 结构、merge_by_priority 字段级合并（CSS 模型），P6.2 Policy Merge Engine 核心就位（44 单测全绿） |
| 2026-08-08 | Claude Android 专项审查落地：🔴 sysinfo.procs（Bionic 线程数）→ /proc 计数；🟠 SkippedNoCpus 不计数；🟠 thermal_pressure 缓存到 sample 快照；🟡 cluster 合法名 fallback + 非法名告警；🟡 PSI 不可用提示；注释修正（merge 继承语义 / stat 字段） |

**下一阶段**：P3 — EbpfSource（fork/exec 迁移）+ Orchestrator 状态机 + Hybrid 模式。
