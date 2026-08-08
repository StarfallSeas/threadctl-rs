# threadctl-rs — P5 最终架构文档（ChatGPT 5.5 审查后修正）

> 已实施 ChatGPT 5.5 审查的必改项。核心原则：
> **threadctl 不应该和 Linux scheduler 对抗，而应该给 scheduler 提供约束和提示。**

---

## 一、从"纠正器"到"观察者"的转向

ChatGPT 5.5 审查指出最关键的设计缺陷：

> *"CPU 迁移不一定是错误。Linux scheduler 主动迁移任务是正确行为。"*

**旧设计**：sched_switch → 检测 CPU 变化 → setaffinity 纠正
**新设计**：sched_switch → 观察（telemetry 计数）→ `force_affinity=true` 才纠正

已实现 `MigrateAction` 枚举：

| 级别 | 行为 | 触发条件 |
|---|---|---|
| `Observe` | 仅记录 telemetry，不干预 | 默认 |
| `Suggest` | 通过 uclamp/cpuset 温和引导 | P5 实现 |
| `Force` | 直接 setaffinity 纠正 | `force_affinity=true`（用户明确配置） |

**默认行为已修改**：`MigrateAction::Observe`（不干预调度器）。

---

## 二、已实施的 P5 模块

### 2.1 `system_context.rs` — 自适应系统感知

- 内存压力：读 `/proc/pressure/memory` 的 `avg10`
- 温度：读 `/sys/class/thermal/thermal_zone*/type` + `temp`（动态识别，不硬编码阈值）
- 电池：读 `/sys/class/power_supply/*/capacity` + `status`
- **自适应轮询**：Normal→10s, Moderate→3s, Critical→1s（ChatGPT 第 5 条采纳）

### 2.2 `capability.rs` — 运行时能力检测

- 检测 uclamp（`/proc/sys/kernel/sched_util_clamp_max`）
- 检测 schedtune（`/dev/stune`）
- 检测 cpuset（`/dev/cpuset` 或 `/sys/fs/cgroup/cpuset`）
- 策略推荐顺序：`uclamp > schedtune > cpuset > affinity`（ChatGPT 第 4 条采纳）

### 2.3 `decision.rs` — 决策引擎

- `TaskIntent`：从 `oom_score_adj` 推断（Interactive / BackgroundLatencySensitive / Background / Frozen）
- `ActionLevel`：Observe / Steer / Force
- `DecisionEngine.decide(intent, pressure)` → 推导动作级别
  - Interactive 始终 Steer，Frozen 始终 Observe
  - Background 在 Critical 压力下降级为 Observe
- `MigrateAction`：默认 Observe（ChatGPT 第 1 条采纳）

### 2.4 `config.rs` 新增字段

```toml
[engine]
migrate_action = "observe"  # observe | suggest | force
pressure_sensitive = true    # 系统压力下调策略强度
```

### 2.5 前置修复（P4→P5 过渡）

| 修复 | 状态 |
|---|---|
| PID 复用检测（pid+start_time） | ✅ 已落地 |
| relock 默认值 5s→60s | ✅ 已落地 |
| sched_switch 默认 Observe | ✅ 已落地 |
| uclamp 改用文件检测（非 syscall） | ✅ 已落地 |

---

## 三、待 P5 实施的新增模块

| 模块 | 内容 | 优先级 |
|---|---|---|
| `crates/ebpf/src/main.rs` | fork/exec 加 UID 过滤 + sched_switch 观察器（不纠正） | 高 |
| `core/src/uclamp.rs` | sched_setattr SCHED_FLAG_UTIL_CLAMP 封装 | 高 |
| `core/src/thread_type.rs` | 线程类型别名表（render/audio/binder） | 中 |
| config profile | `performance_level="game"` 预设映射 | 中 |
| daemon 集成 | main.rs 接入 SystemContext + DecisionEngine | 中 |

---

## 四、修正后的最终架构

```
eBPF (UID过滤)             SystemContext (自适应轮询)
      │                            │
      ▼                            ▼
Event Collector              Pressure/Thermal
      │                            │
      └────────┬───────────────────┘
               ▼
       StateTracker (pid+start_time)
               │
               ▼
        DecisionEngine
       (intent + pressure → ActionLevel)
               │
       ┌───────┼───────┐
       ▼       ▼       ▼
    Observe  Steer   Force
    (默认)  (uclamp/ (affinity)
            cpuset)
```

**关键原则**：
- Observe 是默认（不干预调度器）
- Steer 是推荐（uclamp/cpuset 软引导）
- Force 是例外（用户显式 `force_affinity=true`）

---

## 五、测试覆盖

20 个单测全部通过：

| 模块 | 测试数 |
|---|---|
| config | 2 |
| store | 3 |
| tracker | 3 |
| proc | 2 |
| caps | 1 |
| capability | 2 (新增) |
| system_context | 2 (新增) |
| decision | 5 (新增) |

---

## 六、待 Claude/GPT 确认

1. MigrateAction::Suggest 触发条件：仅 thermal pressure 时引导迁移，还是任何系统状态变化都引导？
2. uclamp 具体数值的 profile 预设：game/audio/background 各对应什么 min/max？
3. eBPF UID 过滤：用户态传入 UID 列表还是内核自动检测 foreground UID？
4. 是否需要"规则审计"：应用规则后记录实际效果（affinity 是否生效、被 cgroup 降级次数）
