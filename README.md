# threadctl-rs — Android/Linux Task Policy Engine

**[English](./README.en.md) | 中文**

> 让 Android/Linux 用户定义每个应用、每个线程的调度策略（CPU 亲和性、
> 调度策略、优先级），并在运行时持续强制执行——对抗 Android AMS/cgroup
> 对线程亲和的系统级覆盖。
>
> 可以理解为 **Android 应用线程级的 systemd policy engine**：systemd 管理服务，
> 它管理线程怎么跑。

> ## threadctl-rs 不是什么（What it is not）
>
> - **不杀后台进程**（kill apps）
> - **不冻结进程**（freeze processes）
> - **不取代 Android LMKD**（内存管理）
> - **不取代 Linux 调度器**（scheduler）
>
> 只应用用户显式定义的线程策略（亲和性 / 调度 / uclamp 约束）。

```text
Application Threads
        ↓
threadctl daemon
        ↓
Config Compiler → RuleSet → Policy Merge → Kernel Action
```

![license](https://img.shields.io/badge/license-GPL--3.0-blue.svg)
![rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)
![tests](https://img.shields.io/badge/tests-60%20passing-green.svg)
![platform](https://img.shields.io/badge/platform-Linux%20%7C%20Android-lightgrey.svg)

---

## 目录

- [为什么选择 threadctl-rs？](#为什么选择-threadctl-rs)
- [功能特性](#功能特性)
- [快速开始](#快速开始)
  - [Linux](#linux)
  - [Android](#android)
- [配置格式](#配置格式)
- [架构设计](#架构设计)
- [安全](#安全)
- [测试与质量](#测试与质量)
- [路线图](#路线图)
- [FAQ](#faq)
- [限制](#限制)
- [许可](#许可)

---

## 为什么选择 threadctl-rs？

| | taskset / 绑核工具 | threadctl-rs |
|---|---|---|
| 作用范围 | 单个进程 | 按应用/线程匹配 |
| 生命周期 | 一次性设置 | 运行时持续强制执行 |
| 对抗系统覆盖 | 无（被 AMS 覆盖即失效） | relock 自动恢复 |
| 前后台感知 | 无 | 后台省电、前台恢复 |
| 配置模型 | 命令行参数 | 声明式规则（继承/覆盖） |

普通绑核工具回答"这条命令把这个进程绑到这些核"；
threadctl-rs 回答"**这个应用在这个场景下，每个线程应该怎么跑**"——并且一直维持。

### 配置继承模型

```text
Global 默认
  ↓
Profile 模板（game / chat / ...）
  ↓
Wildcard 包规则（com.tencent.*）
  ↓
Exact 包规则（com.tencent.mm）
  ↓
Thread 线程规则（RenderThread）
```

规则从上到下叠加，精确者覆盖模糊者、未设置字段向上继承——**只描述差异，不重复配置**。

## 功能特性

- **双格式配置**：KDL（推荐）+ TOML（`[app]` 新式 / `[[rule]]` 兼容旧式）
- **Profile 抽象**：7 个内置场景模板，一条指令启用。注意：**profile 是策略模板，不绑定具体应用**——`profile "game"` 适用于任何游戏，不是"原神专用优化"：

  ```kdl
  app "com.miHoYo.Yuanshen" { profile "game" }   // 游戏：渲染上最强核+保频
  app "com.tencent.mm" { profile "chat" }         // 聊天：渲染流畅，音频清晰
  ```

- **包名通配**：`com.tencent.*` 最长固定前缀匹配（类似 nginx location 优先级），多个通配命中取最具体者
- **继承语义**：精确规则覆盖通配规则，低优先级来源填充空缺——例：通配规则提供默认 CPU 集群，精确规则只覆盖渲染线程的核

  ```kdl
  app "com.tencent.*" { default { cluster "big" } }        // 腾讯系默认性能核
  app "com.tencent.mm" {                                   // 微信精确规则
      thread "RenderThread" { cluster "prime"; sched "fifo" }
  }
  // 微信 RenderThread = prime + fifo；其他线程继承 big
  ```

- **线程匹配**：fnmatch 线程名模式 + 内置 thread-type 别名（render / audio / binder / main）
- **能力检测**：uclamp > schedtune > cpuset > affinity 优先级链
- **三层过滤**：online → cgroup allowed 交集 → setaffinity（应用亲和性前先规避无效 CPU 掩码）
- **热加载**：inotify 优先、轮询降级、快照版本化（失败保留旧配置）
- **审计闭环**：Observe → Decide → Act → Measure → Adjust
- **relock**：周期重锁定对抗 AMS 覆盖；自动跳过后台/缓存进程（省电）
- **自动降级**：eBPF 不可用 → /proc 轮询
- **低开销**：事件驱动，进程数无变化时不做全量扫描

---

## 快速开始

### Linux

```bash
cargo build --release -p threadctl
./target/release/threadctl -c examples/threadctl.kdl
```

### Android

```bash
# 1. 推送二进制（Magisk 模块建议放 /data/adb/threadctl/）
adb push target/release/threadctl /data/adb/threadctl/
adb shell "chmod 755 /data/adb/threadctl/threadctl"

# 2. 推送配置（改包名为你的应用）
adb push examples/user-mode.kdl /data/adb/threadctl/threadctl.kdl

# 3. root 启动
adb shell "su -c '/data/adb/threadctl/threadctl -c /data/adb/threadctl/threadctl.kdl'"
```

> Magisk 用户：`lock-interval` 建议 60s（对抗 AMS 覆盖）；配置用绝对路径。

**第一次体验（30 秒）**：

```bash
cp examples/user-mode.kdl threadctl.kdl
# 编辑 threadctl.kdl，把包名改成你的应用
./target/release/threadctl -c threadctl.kdl
```

### 用户模式（推荐）

```kdl
// 一条指令启用场景策略——改包名即用
app "com.miHoYo.Yuanshen" { profile "game" }
app "com.tencent.mm" { profile "chat" }
app "com.miui.home" { profile "launcher" }
```

内置 profile：`game` / `chat` / `video` / `launcher` / `audio` / `balanced` / `power-save`

### 精细模式

```kdl
app "com.example.game" {
    default { cluster "big" }                    // 所有线程默认性能核
    thread "UnityMain" { cluster "prime"; sched "fifo"; priority 60 }
    thread-type "render" { cluster "big" }       // 内置别名：渲染线程
}
```

- `cluster` 接受 `little` / `big` / `prime`；**数字范围自动识别为 cpus**（`cluster "0-6"` ≡ `cpus "0-6"`）
- 线程名超过 15 字节会被内核截断——启动日志会给出截断警告

---

## 配置格式

- `examples/threadctl.kdl` — 完整 KDL 示例
- `examples/user-mode.kdl` — 用户模式模板（改包名即用）
- `crates/core/config/threadctl.toml` — TOML 默认模板

## 开发者文档

- `docs/matcher.md` — 包匹配器与策略合并设计
- `docs/ai-review-process.md` — 开发与审查流程
- `docs/DeepSeek/` — 架构/阶段设计 + 回应采纳
- `docs/ChatGPT/` — ChatGPT 审查原文
- `docs/Claude/` — Claude 审查原文

---

## 架构设计

### 分层

```text
┌───────────────────────────────────────────────────────────┐
│  threadctl (daemon, bin)                                   │
│  CLI / 热加载主循环 / SystemContext 采样 / audit 摘要        │
├───────────────────────────────────────────────────────────┤
│  threadctl-core (lib, 纯逻辑，零 aya 依赖，可单测)           │
│                                                           │
│  Config Compiler      KDL/TOML → ConfigModel AST           │
│  Rule Compiler        → RuleSet：exact + wildcard 并存      │
│  ThreadMatcher        fnmatch 线程名命中集                  │
│  Policy Merge Layer   merge_by_priority：字段级合并         │
│                       （字段级合并核心已实现；                │
│                         动态决策接入计划于 P6.2）             │
│  Kernel Action        online∩allowed → setaffinity         │
│                       + cpuset + sched/nice + uclamp        │
│                                                           │
│  支撑：store(热加载) tracker(状态) audit(闭环)               │
│        system_context(压力) decision(决策) capability(链)   │
├───────────────────────────────────────────────────────────┤
│  threadctl-ebpf (内核态, no_std)                            │
│  fork/exec 迁移 + sched_switch 采样（P7）                   │
└───────────────────────────────────────────────────────────┘
```

### 核心原则

1. **匹配与合并解耦**：`RuleSet` 只产出 `RuleMatch{index, source}`，最终策略由合并层决定
2. **group/profile 属编译期**：在 Config Compiler 阶段展开，规则引擎不感知高级语义
3. **来源并存而非互斥**：exact 覆盖 wildcard 字段，低优先级来源填充空缺
4. **错误可见性**：规则从不静默丢弃——无效 cluster 名、超长线程名、cpuset 写入失败都会告警

---

## 安全

- **无内核补丁**：纯用户态 syscall，不改内核、不打补丁
- **不修改系统分区**：只写自己的 cpuset 子目录（`/dev/cpuset/threadctl/`），不动系统分区
- **失败安全回退**：策略应用失败（权限、cgroup 限制、线程退出）→ 跳过该线程并告警，不影响其他线程；配置解析失败 → 保留旧配置继续运行
- **白名单约束**：只影响显式配置的包，其余进程保持系统默认

---

## 架构不变量（Architecture Invariants）

**threadctl-rs 不会**：

- 取代 Android LMKD（内存管理）
- 冻结应用（freeze）或杀死后台进程（kill）
- 自动迁移任务（migrate）
- 取代 Linux 调度器

**threadctl-rs 提供**：

- 线程亲和性控制（affinity）
- 调度属性控制（sched / nice）
- uclamp 约束
- 基于策略的执行提示
- 可观测性（audit / telemetry）

> 用户态提供的是 **constraint（约束）**，不是 **replacement policy（替代策略）**——
> 内核已拥有 wakeup migration、load balancing、PELT、EAS、uclamp 的完整信息。

---

## 测试与质量

- **46 单测**：matcher 继承语义、specificity 排序、实例级缓存（1000 通配 × 10000 resolve）、审计环形、配置合并、profile 展开、热加载版本化
- **零警告**：`cargo check --workspace` 0 警告 0 错误
- **轻量实现**：约 5000 行 Rust，release 852KB（strip + LTO）
- **真机验证**：SM8550 设备上验证通过（cpuset 移入成功、零 cgroup 降级）

---

## 路线图

| 阶段 | 内容 | 状态 |
|---|---|---|
| P0-P2 | workspace / ConfigStore / proc 全链路 | ✅ |
| P5 | 五模块（audit / foreground / system_context / capability / decision） | ✅ |
| P6.0 | Profile 抽象 + 7 内置模板 | ✅ |
| P6.1 | pkg matcher（MatchPriority / specificity / 继承语义 / 缓存） | ✅ 冻结 |
| P6.2 | Policy Merge Engine 深化：决策引擎接入 relock、来源优先级矩阵、Zygote pending、@@main、tracing | 🔄 |
| P6.3 | group（内置常用包名表） | ⏳ |
| P7 | eBPF 内核态：fork/exec 迁移 + sched_switch 采样 | ⏳ |
| P8 | Magisk 生产包（module.prop / service.sh / 更新通道） | ⏳ |

---

## FAQ

**Q: 和普通的"CPU 绑核工具"（taskset / app pinning）有什么区别？**
A: 普通工具是"一次性设置亲和性"。threadctl-rs 是**运行时策略引擎**：声明式配置持续匹配，
并在 Android AMS/cgroup 覆盖后自动恢复（relock），还会感知前后台、系统压力调整策略强度。

**Q: 需要 root 吗？**
A: 是。设置 CPU 亲和性（sched_setaffinity）和实时调度（fifo/rr）需要 root 或
CAP_SYS_NICE。Android 端建议以 Magisk 模块运行。

**Q: 会影响电池吗？**
A: 设计上省电优先：后台/缓存进程自动跳过 relock（让 Linux scheduler 自然决定），
`power-save` profile 只使用能效核。

**Q: 会不会导致游戏性能下降？**
A: 不会强制所有线程跑大核。策略只约束匹配的线程（如渲染线程），未配置的线程
保持系统调度；`game` profile 反而把渲染线程锁到最强核 + 保频，通常是正向收益。

**Q: 支持非小米设备吗？**
A: 支持。集群检测基于 `cpu_capacity`（标准 sysfs），不依赖厂商接口。MIUI/HyperOS
的 `/dev/cpuset` 布局已验证兼容。

**Q: eBPF 模式什么时候可用？**
A: P7 规划。当前 /proc 轮询模式已覆盖全部功能；eBPF 用于更低延迟的 fork/exec
事件和 sched_switch 迁移观察。

---

## 限制

- 只能约束**白名单内**的包；未配置的进程保持系统默认（不做兜底）
- 线程名超过 15 字节会被内核截断——用 `thread-type` 或参考启动日志警告
- Android AMS 会在应用切后台时移动 cpuset 归属，relock 是其对抗手段
  （后台进程不重锁，切回前台后恢复）
- 多进程应用（`:service` 子进程）需要单独配置

---

## 许可

GPL-3.0
