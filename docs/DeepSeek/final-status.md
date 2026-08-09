# threadctl-rs — 最终状态审计文档（供 Claude 审查）

> 生成日期：2026-08-08
> 项目：threadctl-rs v2.0.0 — Android/Linux 任务策略编排引擎（KDL→AST→Policy Compiler→RuleSet→Decision Engine→Kernel Action）
> 状态：P0-P6.1 全部完成，44 单测全绿，零警告，release 851KB
>
> **⚠️ 本文档是 P6.1 阶段快照（2026-08-07）**：P6.2 新增 16 项修复后
> 当前为 60 单测全绿 / 858KB / v2.0.0-dev。历史数字保留供阶段追溯，
> 最新状态以 P6.2-final-delivery.md 与 README 徽章为准。
> 本文档汇总：实现现状 / 三轮 AI 审查落地 / 最终测试矩阵 / 遗留项 / P6.2 方向

---

## 一、项目定位（演进结论）

```
最初：给线程绑核的工具
现在：Android/Linux 任务策略编排引擎
```

架构分层（GPT 三次审查确认的最终形态）：

```text
KDL / TOML
    │
    ▼
Config Compiler（profile/group 展开，P6.2 完善）
    │
    ▼
Rule Compiler → RuleSet（PackageMatcher：exact + wildcard 并存）
    │
    ▼
ThreadMatcher（fnmatch 命中集）
    │
    ▼
Policy Merge Engine（merge_by_priority：字段级覆盖 + 继承，CSS 模型）
    │
    ▼
Kernel Action（uclamp / cpuset / affinity / sched）
```

**核心原则**（三轮审查沉淀）：
- group/profile 属 Config Compiler 阶段，RuleSet 不感知高级语义
- 匹配（RuleSet 产出 RuleMatch）与合并（PolicyMerge）彻底解耦
- 包级来源并存（exact override wildcard，低优先级填充空缺），非唯一来源

---

## 二、模块实现状态

### core（19 个模块，纯逻辑，无 aya 依赖）

| 模块 | 状态 | 关键点 |
|---|---|---|
| topology.rs | ✅ | CpuSet 1024 位图 / 集群检测（cpu_capacity）/ cpuset 目录管理 / read_allowed_mask |
| config.rs | ✅ | TOML + KDL 双层格式 → ConfigModel AST / profile 展开 / cluster fallback + 非法名告警 |
| ruleset.rs | ✅ P6.1 冻结 | RuleMatch{index,source} / nginx specificity / 继承语义 / 实例级缓存 |
| policy.rs | ✅ | 三层过滤（online→allowed 交集→setaffinity）/ ApplyOutcome 枚举 / audit 全路径记录 / EINVAL/EPERM/cpuset 失败去重 |
| engine.rs | ✅ | handle_events / refresh / relock（跳过 Background/Frozen）/ SkippedNoCpus 不计数 |
| store.rs | ✅ | inotify→轮询降级 / 快照版本化 / 失败保留旧快照 |
| tracker.rs | ✅ | start_time 防 PID 复用 / 线程名 TTL 缓存 / cpuset 引用计数回收 |
| decision.rs | ✅ 骨架 | TaskIntent / ActionLevel / TaskScore 权重 / MigrateAction 默认 Observe |
| system_context.rs | ✅ | 自适应轮询（10s/3s/1s）/ thermal_pressure 缓存 / PSI 降级提示 |
| capability.rs | ✅ | uclamp > schedtune > cpuset 检测（文件探测避免 SIGSYS） |
| audit.rs | ✅ | 256 环形缓冲 + 时间戳 + summary_windowed(60s) |
| foreground.rs | ✅ 骨架 | cpuset tasks → UID 缓存（M4 未完整接入） |
| profile.rs | ✅ | 5 个内置模板（game/audio/launcher/balanced/power-save） |
| proc.rs / caps.rs / event.rs / kdl_parser.rs | ✅ | — |

### daemon

| 模块 | 状态 | 关键点 |
|---|---|---|
| main.rs | ✅ | CLI / 配置接入 / 热加载主循环 / Capability 打印 / SystemContext 采样 / audit 60s |
| proc_source.rs | ✅ | **/proc 目录计数**（Bionic sysinfo.procs 是线程数的替代）/ 增量路径 / Exit 检测 |

### ebpf（内核态）
- 空骨架，待 P3 迁移（fork/exec 保留 + sched_switch 扩展）

---

## 三、三轮 AI 审查落地清单

### ChatGPT 首轮（P6.1 约束）
- MatchPriority 模型 / specificity / 实例级缓存（reload 重建即失效）/ API 不变 / 4 个指定测试 ✅

### GPT 二次审查
- specificity 升级 nginx 评分（前缀权重 + 通配惩罚）✅
- 分层 merge（包级覆盖 / 包内线程 merge）✅
- group 归入 Config Compiler（ruleset 不感知）✅

### GPT 三次审查（P6.1 冻结）
- **继承语义**：来源并存而非唯一（`com.tencent.*` default + `com.tencent.mm` 线程规则 → 字段叠加/填充）✅
- RuleMatch{index, source} 结构（匹配与合并解耦）✅
- merge_by_priority（CSS 模型：高优先级覆盖 + 低优先级继承）✅

### Claude 通用审查
- 1.1 模板 `[app]` 格式 ✅
- 1.2 thread 超长截断警告（`com.coolapk.market` → `com.coolapk.mar`）✅
- 1.3 `[app]` merge 而非全量替换 ✅
- 2.2 relock 区分前后台（oom_adj > 500 跳过）✅
- Q2 cpuset_refs 边角 case（注释确认低风险）✅
- Q4 H3 阈值 ✅

### Claude Android 专项
- 🔴 `sysinfo.procs`（Bionic 线程数）→ `/proc` 目录计数 + 阈值 5 ✅
- 🟠 SkippedNoCpus 不计数 applied ✅
- 🟠 thermal_pressure 缓存到 sample 快照 ✅
- 🟡 cluster 合法名 fallback（无 prime → 最大集群）+ 非法名告警 ✅
- 🟡 PSI 不可用启动提示 ✅
- 🟡 merge_by_priority 注释修正（继承 vs 覆盖）✅
- 🟡 read_start_time 注释修正 ✅

---

## 四、最终深度测试矩阵（2026-08-08）

| 层 | 项目 | 结果 |
|---|---|---|
| 单测 | 44 passed / 0 failed（0.11s） | ✅ |
| 静态 | workspace check 0 警告 0 错误 | ✅ |
| 构建 | release 851KB，strip + LTO | ✅ |
| 告警矩阵 | cluster 非法名告警（`"0-6" 无效`） | ✅ |
| 告警矩阵 | thread 超长截断告警（`com.coolapk.mar`） | ✅ |
| 链路 | 3 规则包加载 + 集群检测 + 事件应用 | ✅ |
| 链路 | relock 周期（含后台跳过逻辑） | ✅ |
| 链路 | PSI 不可用降级提示（termux 实测） | ✅ |
| 链路 | 热加载版本 2 + 增量重扫 | ✅ |
| 语义 | 继承（wildcard default + exact 补充） | ✅ |
| 性能 | 1000 通配 + 10000 resolve 缓存命中 | ✅ |

**真机验证（SM8550，用户设备）**：
- cpuset 可用、RT 权限有、uclamp 检测到
- audit: total=256 success=256 cgroup_blocked=0 —— cpuset 移入全成功，零降级
- 修正 `cluster "0-6"` → `cpus "0-6"` 后 miui.home 规则生效

---

## 五、遗留项（按优先级）

| 优先级 | 项 | 说明 |
|---|---|---|
| 🟠 | Zygote fork 空窗 | 新 App 发现延迟 ~2s（cmdline 填充延迟）→ pending 队列 200ms 重读 |
| 🟠 | MIUI 冻结进程 | SIGSTOP 进程 relock 无意义 → is_frozen 检查跳过 |
| 🟠 | 多进程 App `:service` | 子进程匹配不到主规则 → 文档说明（短期） |
| 🟡 | `@@main` 主线程语法 | tid==pid 匹配（根解法，替代手动截断包名） |
| 🟡 | foreground UID→pid | foreground.rs 精度提升（M4 完整接入时） |
| 🟡 | M4 五模块完整接入 | 仅 Capability 打印接入；DecisionEngine 驱动 relock 待 P6.2 |
| 🟡 | 日志英文化 | 部分 eprintln 中文 → tracing 结构化（P4 规划） |

---

## 六、P6.2 路线（Policy Merge Engine 深化）

1. **来源优先级矩阵**：RuleSource 已预留 Global/Profile/Group 变体；
   Config Compiler 展开 profile/group 时标注来源，merge_by_priority 统一处理
2. **决策引擎接入**：relock 从"oom_adj 阈值"升级为 DecisionEngine
   （TaskIntent::from_sources + decide → ActionLevel::Observe 跳过），
   audit summary_windowed 作为 Adjust 环节输入
3. **Zygote pending 队列**：新进程延迟确认（200ms 重读 cmdline）
4. **@@main 主线程匹配**：tid==pid 特殊语法
5. **tracing 日志**：eprintln → tracing 宏 + Android logcat

---

## 七、给 Claude 的审查问题

1. **继承语义的边界**：当前"线程规则命中时包级规则填充空缺字段"——若用户
   想让线程规则**完全独立**（不继承包级 default），如何表达？（如 RenderThread
   只上 prime 7，不含包级 big 3-6）是否需要 `no-inherit` 标记？
2. **relock 后台跳过**：oom_adj > 500 跳过 vs DecisionEngine 完整接入——
   P6.2 升级的正确时机？（游戏中途切后台 30s 再回来，规则靠什么恢复？）
3. **cpuset 目录生命周期**：引用计数归零 rmdir 后，进程重启瞬间的目录重建
   窗口——`ensure_cpuset_dir` 缓存是否足够？（Q2 边角 case 的工程化）
4. **profile fallback 语义**：`cluster "prime"` 在无 prime 设备 fallback 到
   最大集群——这是"尽力而为"还是应该告警让用户知道规则变了？
5. **audit 环形 256 的决策价值**：summary_windowed(60) 对 Adjust 环节的
   输入是否够用？（是否应加 per-pkg 聚合计数而非仅最近 256 条）
