# threadctl-rs — Android/Linux Task Policy Engine

**事件驱动的 Android/Linux 线程调度策略编排引擎** · Event-driven task policy orchestration engine for Android/Linux

```
KDL/TOML → Config Compiler → RuleSet → Policy Merge Engine → Kernel Action
```

threadctl-rs 是一个将「给线程绑核」升级为「运行时策略匹配引擎」的守护进程：
把应用/线程的调度意图（CPU 亲和性、调度策略、优先级、uclamp）声明式地写成配置，
由引擎持续匹配并应用——对抗 Android AMS/cgroup 对线程亲和的系统级覆盖。

threadctl-rs is a daemon that elevates "CPU pinning" into a runtime policy-matching engine:
declaratively express per-app/per-thread scheduling intent (affinity, sched policy, priority, uclamp),
and let the engine continuously match & apply it — fighting Android AMS/cgroup-level affinity overrides.

> **由多个 AI 协作开发** · Built collaboratively by multiple AIs — see [AI 协作记录](#ai-协作记录--ai-collaboration-log)

---

## 目录 · Table of Contents

- [功能特性 · Features](#功能特性--features)
- [架构设计 · Architecture](#架构设计--architecture)
- [配置格式 · Configuration](#配置格式--configuration)
- [AI 协作工作流 · AI Collaboration Workflow](#ai-协作工作流--ai-collaboration-workflow)
- [AI 协作记录 · AI Collaboration Log](#ai-协作记录--ai-collaboration-log)
- [测试与质量 · Testing & Quality](#测试与质量--testing--quality)
- [路线图 · Roadmap](#路线图--roadmap)
- [许可 · License](#许可--license)

---

## 功能特性 · Features

| 特性 | 说明 |
|---|---|
| 双格式配置 | KDL（推荐）+ TOML（`[app]` 新式 / `[[rule]]` 兼容旧式） |
| Profile 抽象 | 7 个内置场景模板（game/chat/video/launcher/audio/balanced/power-save），一条指令启用 |
| 包名通配 | `com.tencent.*` 前缀匹配 + nginx 风格 specificity（最长前缀优先） |
| 继承语义 | exact 规则覆盖 wildcard，低优先级来源填充空缺（CSS 模型） |
| 线程匹配 | fnmatch 线程名模式 + 内置 thread-type 别名（render/audio/binder/main） |
| 能力检测 | uclamp > schedtune > cpuset > affinity 优先级链 |
| 三层过滤 | online → cgroup allowed 交集 → setaffinity（消除 EINVAL） |
| 热加载 | inotify 优先、轮询降级、快照版本化（失败保留旧配置） |
| 审计闭环 | Observe→Decide→Act→Measure→Adjust，256 环形缓冲 + 60s 窗口摘要 |
| relock | 周期重锁定对抗 AMS 覆盖；自动跳过后台/缓存进程（省电） |
| 自动降级 | eBPF 不可用 → /proc 轮询（当前以 proc 为主，eBPF 内核态待 P3 迁移） |

**Feature highlights**

- KDL & TOML config formats, 7 built-in profiles for one-line setup
- Wildcard package matching with nginx-style longest-prefix specificity
- Inheritance semantics (exact overrides wildcard, low-priority sources fill gaps)
- Three-layer affinity filtering (online → cgroup allowed ∩ → setaffinity)
- Hot-reload via inotify with poll fallback and versioned snapshots
- Audit feedback loop (Measure→Adjust), background-aware relock

---

## 架构设计 · Architecture

### 分层 · Layered design

```
┌───────────────────────────────────────────────────────────┐
│  threadctl (daemon, bin)                                   │
│  CLI / 热加载主循环 / SystemContext 采样 / audit 摘要        │
├───────────────────────────────────────────────────────────┤
│  threadctl-core (lib, 纯逻辑，零 aya 依赖，可单测)           │
│                                                           │
│  Config Compiler      KDL/TOML → ConfigModel AST           │
│                       profile/group 展开（P6.2 完善）       │
│  Rule Compiler        → RuleSet：PackageMatcher            │
│                       exact + wildcard 并存 + 实例级缓存     │
│  ThreadMatcher        fnmatch 线程名命中集                  │
│  Policy Merge Engine  merge_by_priority：字段级覆盖+继承    │
│                       （RuleMatch{index,source} 解耦）      │
│  Kernel Action        online∩allowed → setaffinity         │
│                       + cpuset + sched/nice + uclamp        │
│                                                           │
│  支撑模块：store(热加载) tracker(状态) audit(闭环)           │
│           system_context(压力感知) decision(决策)            │
│           capability(能力链) foreground(前台检测)            │
├───────────────────────────────────────────────────────────┤
│  threadctl-ebpf (内核态, no_std)                            │
│  fork/exec 迁移 + sched_switch 采样（P3/P5 迁移）            │
└───────────────────────────────────────────────────────────┘
```

### 核心原则 · Core principles

1. **匹配与合并解耦** · Matching decoupled from merging — `RuleSet` emits `RuleMatch{index, source}`, `merge_by_priority` decides the final policy.
2. **group/profile 属编译期** · Groups/profiles belong to the Config Compiler phase; `RuleSet` never knows high-level semantics.
3. **来源并存而非互斥** · Sources coexist — exact overrides wildcard fields, low-priority sources fill gaps (CSS-like inheritance).
4. **错误可见性** · Never silently drop a rule — invalid cluster names, over-long thread names, and cpuset write failures all warn.

### 模块清单 · Module inventory (21 source files, 5018 lines)

| 模块 | 职责 |
|---|---|
| `topology.rs` | CpuSet(1024) 位图、集群检测（cpu_capacity）、cpuset 目录管理、`read_allowed_mask` |
| `config.rs` | serde/KDL → `ConfigModel` AST、profile 展开、cluster 容错+fallback、`merge_policy` |
| `ruleset.rs` | `RuleMatch{index,source}`、nginx specificity、继承语义、实例级缓存 |
| `policy.rs` | 三层过滤、`ApplyOutcome` 枚举、audit 全路径、EINVAL/EPERM/cpuset 失败去重 |
| `engine.rs` | 事件处理、全线程刷新、relock（后台跳过）、SkippedNoCpus 不计数 |
| `store.rs` | inotify→轮询降级、快照版本化、失败保留旧快照 |
| `tracker.rs` | start_time 防 PID 复用、线程名 TTL 缓存、cpuset 引用计数回收 |
| `decision.rs` | TaskIntent、ActionLevel、TaskScore 权重、MigrateAction 默认 Observe |
| `system_context.rs` | 自适应轮询（10s/3s/1s）、thermal_pressure 缓存、PSI 降级提示 |
| `capability.rs` | uclamp > schedtune > cpuset 文件探测（避免 SIGSYS） |
| `audit.rs` | 256 环形缓冲 + 时间戳 + `summary_windowed(60s)` |
| `profile.rs` | 7 个内置模板（game/chat/video/launcher/audio/balanced/power-save） |
| `foreground.rs` | cpuset tasks → UID 缓存（M4 部分接入） |
| `proc.rs` | /proc 工具、`read_oom_adj`、comm 读取 |
| `kdl_parser.rs` | KDL→ConfigModel（daemon/engine/app/profile/thread/thread-type） |
| daemon: `proc_source.rs` | **/proc 目录计数**（Bionic sysinfo.procs 是线程数的替代）、增量路径、Exit 检测 |
| daemon: `main.rs` | CLI、热加载主循环、SystemContext 采样、audit 60s 摘要 |

---

## 配置格式 · Configuration

### 用户模式（推荐）· User mode (recommended)

```kdl
// 一条指令启用场景策略——改包名即用
app "com.miHoYo.Yuanshen" { profile "game" }
app "com.tencent.mm" { profile "chat" }
app "com.miui.home" { profile "launcher" }
```

内置 profile：`game`（渲染上最强核+保频）/ `chat` / `video` / `launcher` / `audio` / `balanced` / `power-save`

### 精细模式 · Advanced mode

```kdl
app "com.example.game" {
    default { cluster "big" }                    // 所有线程默认性能核
    thread "UnityMain" { cluster "prime"; sched "fifo"; priority 60 }
    thread-type "render" { cluster "big" }       // 内置别名渲染线程
}
```

- `cluster` 接受 little/big/prime；**数字范围自动识别为 cpus**（`cluster "0-6"` ≡ `cpus "0-6"`）
- 包名支持通配：`com.tencent.*`（最长固定前缀优先）
- 线程名 >15 字节会被内核截断——启动日志会给出截断警告

### TOML 等价 · TOML equivalent

```toml
[engine]
mode = "proc"
lock_interval = 60

[app."com.miHoYo.Yuanshen"]
profile = "game"

[app."com.example.game".threads.UnityMain]
cpus = "7"
sched = "fifo:60"
```

---

## AI 协作工作流 · AI Collaboration Workflow

本项目是「**AI 驱动、人工裁决**」的协作范例：四个 AI 各司其职，阶段化交付 + 交叉审查。

This project is a case study in "**AI-driven, human-adjudicated**" collaboration:
four AIs with distinct roles, phase-based delivery + cross-review.

### 角色分工 · Role assignment

| AI | 角色 | 贡献 |
|---|---|---|
| **DeepSeek V4 Pro** | 架构师 / 审查者 | 总体架构设计、多轮审查意见（P6.1 matcher 约束、Policy Merge Engine 方向）、文档 |
| **DeepSeek V4 Flash** | 主开发者 | 全部代码实现（5018 行）、测试、调试、文档更新 |
| **ChatGPT 5.5** | 架构审查 | P5 阶段 3 项"必须加入"（audit 闭环 / TaskIntent 多源 / 权重模型）、P6.1 matcher 三次审查（MatchPriority → specificity → 继承语义）、P6.2 方向 |
| **Claude Opus 4.x** | 深度审查 | 通用审查（模板格式、线程截断、merge 语义）+ **Android 专项审查**（Bionic sysinfo.procs 线程数、Zygote 空窗、MIUI 冻结、TASK_COMM_LEN） |

### 阶段化交付 · Phase-based delivery

```
P0 workspace 骨架 → P1 ConfigStore → P2 proc 全链路 → P5 五模块
→ P6.0 Profile → P6.1 Matcher（冻结）→ P6.2 Policy Merge Engine（进行中）
```

每阶段：实现 → 生成审查文档（`design/P*.md`）→ 交叉 AI 审查 → 修复 → 回归 → 冻结。

Each phase: implement → write review doc → cross-AI review → fix → regression → freeze.

### 审查→修复闭环 · Review→fix loop

1. **GPT 三轮审查（P6.1 matcher）**：每轮意见落地 + 专项测试
   - 首轮：MatchPriority、specificity、实例级缓存、4 个指定测试
   - 二轮：nginx 评分、分层 merge、group 归位 Config Compiler
   - 三轮：**继承语义**（来源并存）、`RuleMatch{index,source}`、P6.1 冻结
2. **Claude 通用审查**：模板 `[app]` 化、线程截断警告、`[app]` merge 而非替换、relock 前后台
3. **Claude Android 专项**（关键纠偏）：
   - `sysinfo.procs` 在 Bionic 是**线程数**→ 改 /proc 目录计数（根治每轮全扫）
   - 线程名 TASK_COMM_LEN 截断 → 编译期警告
   - relock 与 AMS/MIUI 对抗 → 按 oom_adj 跳过后台
   - thermal_pressure 快照同步、cluster fallback、PSI 降级提示

### 质量门禁 · Quality gates

- 44 单测全绿（matcher 语义 / 审计环形 / 配置合并 / profile 展开 / 热加载版本化）
- `cargo check --workspace` 零警告零错误
- release 852KB（strip + LTO）
- 真实设备 SM8550 验证：cpuset 移入 100% 成功、零降级、audit success=256

---

## AI 协作记录 · AI Collaboration Log

| 日期 | 参与者 | 事件 |
|---|---|---|
| 2026-08-07 | DeepSeek V4 Flash | 需求确认：全新重写（非 既有实现 延续），eBPF 保留；P0 workspace 骨架 |
| 2026-08-07 | DeepSeek V4 Pro | 架构文档 `design/architecture.md`，P1-P6 路线 |
| 2026-08-07 | Claude Opus 4.x | P0 审查：2 P0 Bug + 4 设计问题，Q1-Q7 定案 |
| 2026-08-07 | DeepSeek V4 Flash | P1 ConfigStore（inotify 降级链）+ P2 proc 全链路 |
| 2026-08-07 | DeepSeek V4 Flash | P5 五模块（audit/foreground/system_context/capability/decision） |
| 2026-08-07 | ChatGPT 5.5 | 终审 3 项"必须加入"落地（audit 闭环等） |
| 2026-08-08 | DeepSeek V4 Flash | KDL 格式落地；P6.0 Profile + P6.1 通配符 |
| 2026-08-08 | ChatGPT 5.5 | P6.1 三次审查：MatchPriority→specificity→继承语义，**P6.1 冻结** |
| 2026-08-08 | DeepSeek V4 Flash | P6.1 matcher 三轮重构 + 44 单测 |
| 2026-08-08 | Claude Opus 4.x | 通用 + **Android 专项**审查（Bionic procs / 截断 / relock 对抗） |
| 2026-08-08 | DeepSeek V4 Flash | Android 专项落地 + 最终深度测试 + 配置友好化（容错/profile/用户模板） |

**协作模式总结** · Collaboration pattern

```
DeepSeek V4 Flash 实现 → 审查文档 → GPT 架构审查 → Claude 深度审查
→ 人工（用户）真机验证/裁决 → 修复 → 冻结 → 下一阶段
```

---

## 测试与质量 · Testing & Quality

- **44 unit tests** covering: matcher inheritance semantics, specificity ordering, instance-scoped cache (1000 wildcards × 10000 resolves), audit ring buffer, config merging, profile expansion, hot-reload versioning
- **Zero warnings** across workspace
- **852KB stripped release** (LTO + opt-level=z)
- **Real-device validated** (SM8550): cpuset join 100% success, zero cgroup degradation

### 已知遗留 · Known backlog

- Zygote fork 后 cmdline 空窗（新 App 发现延迟 ~2s）→ pending 队列
- MIUI 冻结进程（SIGSTOP）relock 浪费 → is_frozen 检查
- 多进程 App `:service` 子进程匹配 → 文档说明
- `@@main` 主线程（tid==pid）匹配语法
- eBPF 内核态迁移（P3）+ sched_switch 采样（P5）

---

## 路线图 · Roadmap

| 阶段 | 内容 | 状态 |
|---|---|---|
| P0-P2 | workspace / ConfigStore / proc 全链路 | ✅ |
| P5 | 五模块（audit/foreground/system_context/capability/decision） | ✅ |
| P6.0 | Profile 抽象 + 7 内置模板 | ✅ |
| P6.1 | pkg matcher（MatchPriority/specificity/继承语义/缓存） | ✅ 冻结 |
| P6.2 | Policy Merge Engine 深化：决策引擎接入 relock、来源优先级矩阵、Zygote pending、@@main、tracing 日志 | 🔄 进行中 |
| P6.3 | group（内置常用包名表——用户连包名都不用查） | ⏳ |
| P3/P5 | eBPF 内核态（fork/exec 迁移 + sched_switch 采样） | ⏳ |
| P6/P7 | Magisk 模块打包、`Arc<[RuleMatch]>` 零拷贝缓存 | ⏳ |

---

## 许可 · License

GPL-3.0

---

*Built with [DeepSeek V4 Pro] · [DeepSeek V4 Flash] · [ChatGPT 5.5] · [Claude Opus 4.x] — four AIs, one engine.*
