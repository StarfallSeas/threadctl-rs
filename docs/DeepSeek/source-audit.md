# threadctl-rs — 全源码逐行深度审查报告

> 审查日期：2026-08-08 | 审查范围：全部 19 个 .rs 源文件（3588 行）
> 方法：逐文件逐行阅读，对照 Linux 内核语义与设计文档
> **修复状态：19 项全部修复完成（2026-08-08 第二轮）**
>
> **⚠️ 本文档是 P6.1 阶段快照（2026-08-08）**：P6.2-P6.3 新增 Backend 抽象、
> detect_clusters v2（Mid/全大核）、DVFS 域探测、DecisionEngine 门控等，
> 源码已增至 ~5500 行 / 73 单测。历史审查数字保留供阶段追溯，
> 最新状态以 docs/DeepSeek/P6.3-delivery.md 与 repo-overview.md 为准。

---

## 零、修复确认

| # | 修复内容 | 验证 |
|---|---|---|
| H1 | `merge_policy` 字段级合并（cpus OR + 其余首个生效）+ 2 回归测试 | ✅ `multi_pkg_rules_merge_or` / `same_thread_rules_merge` |
| H2 | ruleset 支持空 cpus 占位规则 + `RuleConfig.cpus` serde default + resolve 空 cpus 仅 sched | ✅ `sched_only_rule_is_whitelist_placeholder` |
| H3 | proc_source 仅"进程数增加"全扫 | ✅ 冒烟正常 |
| H4 | tracker enter 零 I/O 快速路径 + pkg 更新（exec 修复） | ✅ 32 测试全绿 |
| M1 | detect_clusters 回退用 online cpus | ✅ |
| M2 | EPERM 去重（WARNED_EPERM_TIDS） | ✅ |
| M3 | KDL daemon/engine 节点（parse_daemon/parse_engine） | ✅ `kdl_daemon_engine_nodes` |
| M4 | 五模块接入 main（Capability 启动打印 / SystemContext 采样 / foreground 30s / audit 60s / decision 初始化） | ✅ 冒烟输出 Capability |
| M5 | oom_adj 阈值 201..=500 | ✅ |
| M6 | 文件缺失轮询静默 | ✅ |
| M7 | thermal_pressure() 读 cooling_device 使用率 | ✅ task_score_sums 更新 |
| L1 | event.rs Fork 注释修正 | ✅ |
| L2 | audit record() poison 统一 | ✅ |
| L3 | remove_cpuset_dir expect | ✅ |
| L4 | warn_once helper（32K 上限） | ✅ |
| L5 | 保留原顺序（正确性优先，AMS 迁移不改 affinity 时需每轮写 cpuset） | ⚠️ 设计取舍，文档记录 |
| L6 | SHORT_CIRCUIT_TOTAL 计数器 | ✅ |
| L7 | watch 重装前 sleep 1s | ✅ |
| L8 | 占位规则注释与实现一致 | ✅ |

**验证**：cargo check 零警告，32 单测全绿（+4 回归），release 825KB，冒烟通过。

---

## 一、审查结论总览

**高严重度（4 项，必须修复）**

| # | 位置 | 问题 |
|---|---|---|
| H1 | `config.rs` ConfigModel::from_toml | 多条**包级** `[[rule]]` 互相覆盖 → 规则丢失（行为回归） |
| H2 | `config.rs` to_rules_with_clusters | 占位规则 `cpus=""` 被 RuleSet::compile 判无效 → 纯 sched/thread_type 的包不进白名单 |
| H3 | `proc_source.rs` collect | `proc_total != last_proc_total` 触发几乎每轮全扫（sysinfo.procs 含线程数） |
| H4 | `tracker.rs` enter | 每次调用读 /proc/<pid>/stat（即使已跟踪）→ 热路径多余 I/O |

**中严重度（7 项，建议修复）**

| # | 位置 | 问题 |
|---|---|---|
| M1 | `topology.rs` detect_clusters | 回退路径 CpuSet 含 0-1023，range_str="0-1023" 误导 cluster 解析 |
| M2 | `policy.rs` apply_affinity | EPERM 无去重 → 非 root 桌面会刷屏 |
| M3 | `config.rs` load | KDL 模式 daemon/engine 配置全用默认值（无法自定义 pidfile 等） |
| M4 | daemon 集成 | decision/system_context/capability/foreground/audit 五模块未被调用（dead code） |
| M5 | `decision.rs` from_oom_adj | 阈值 201..=700 把普通后台（500）误判为延迟敏感 |
| M6 | `store.rs` spawn_hot_reload | 文件被删期间轮询每轮打印"配置重载失败"（刷屏） |
| M7 | `system_context.rs` evaluate | 温度阈值 45°C/38°C 硬编码（ChatGPT 已指出应读 thermal level） |

**低严重度（8 项，记录）**

| # | 位置 | 问题 |
|---|---|---|
| L1 | `event.rs` | Fork 注释"由 tid!=pid 区分"未实现 |
| L2 | `audit.rs` | record() 的 poison 处理与 summary() 不一致 |
| L3 | `topology.rs` remove_cpuset_dir | unwrap_or_else 与 Claude 审查 ❻ 的 expect 建议不一致 |
| L4 | `policy.rs` | WARNED_* 三个静态 HashSet 无上限 |
| L5 | `policy.rs` apply_affinity | cpuset tasks 写入先于 getaffinity 短路（每轮都写） |
| L6 | `engine.rs` refresh | applied 计数含短路命中（统计语义偏差） |
| L7 | `store.rs` wait | DELETE/MOVE_SELF 后 sleep(poll_interval) 才重装 watch |
| L8 | `config.rs` | 占位规则逻辑意图（白名单）与实现脱节 |

---

## 二、逐文件审查详情

### 2.1 lib.rs（22 行）— ✅ 无问题

- 双 compile_error 防护正确（非 Linux/Android + 32 位）
- 19 个模块导出完整

### 2.2 event.rs（76 行）

- ✅ EventKind/ProcessEvent/EventSource 设计一致
- ⚠️ L1：`Fork` 注释 "由 tid != pid 区分线程 clone"——实际 ProcSource 产 Fork 时 tid==pid，ThreadClone 独立事件，无 tid!=pid 判断。注释应更新。

### 2.3 proc.rs（92 行）

- ✅ `read_proc_file` 栈上 32 字节缓冲：最长路径 `/proc/<10位pid>/cmdline` = 24 字节，无溢出风险
- ✅ `read_start_time` 字段索引验证：`rsplit_once(')')` 后第 20 个字段（0-indexed 19）= starttime（第 22 字段），**计算正确**
- ✅ `list_tids` 无问题
- ⚠️ 注意：comm 含 ')' 时 rsplit_once 截断错误（罕见，可接受）

### 2.4 caps.rs（58 行）— ✅ 无问题

- CAP_SYS_NICE=23 正确；V3=0x20080522 正确；capget(pid=0) 自身检查正确
- cap<32 → data[0] 正确

### 2.5 audit.rs（92 行）

- ✅ 环形缓冲 256 条，remove(0) O(n) 可接受
- ⚠️ L2：`record()` 用 `if let Ok`（poison 静默丢），`summary()` 用 unwrap_or_else——不一致

### 2.6 topology.rs（330 行）

- ✅ CpuSet 位图 16×u64 与 cpu_set_t 布局一致；sched_setaffinity 传 128 字节：内核 `len > cpumask_size()` 时截断、`len <` 时 EINVAL——128B 对 NR_CPUS≤1024 均安全（已验证内核语义）
- ✅ `read_allowed_mask` 用 Cpus_allowed_list 字符串解析（可读、无位宽限制）
- ⚠️ **M1**：`detect_clusters` 回退分支 `for cpu in 0..CPU_SETSIZE { set(cpu) }` → CpuSet 含 0-1023。`range_str` 输出 "0-1023"。若用户配 `cluster "unknown"`，policy_to_rules 直接用该 range_str（不经过 present 过滤）→ 错误的超大掩码。**修复**：回退用 present/online 而非全 1024。
- ⚠️ L3：`remove_cpuset_dir` 的 `unwrap_or_else(|_| CString::new("."))` 与 ❻ 的 expect 建议不一致

### 2.7 ruleset.rs（210 行）— ✅ 核心逻辑正确

- resolve 的 OR 合并 + fallback 语义保持一致
- 坏规则 `else continue` 修复到位（Claude ❷）
- cpuset_dir 运行时派生（Claude ❹）到位
- fnmatch BUF_LEN=32 > comm 最大 15 字符，安全

### 2.8 policy.rs（195 行）

- ✅ cpuset 先行 → allowed 交集 → setaffinity 顺序正确（设备验证过）
- ⚠️ **M2**：EPERM 分支直接 eprintln 无去重（EINVAL/cgroup 都有去重）——非 root 场景刷屏
- ⚠️ L4：三个静态 HashSet 无上限（tid 空间有限，长期 ~32K 项 ≈ 1MB，可控）
- ⚠️ L5：cpuset tasks 写入在 getaffinity 短路**之前**——每轮 relock 对每个线程都写一次 tasks（幂等但非零开销）

### 2.9 config.rs（~570 行）

- 🔴 **H1**：`ConfigModel::from_toml` 中
  ```rust
  if r.thread.is_empty() {
      entry.default_policy = pol;  // ← 覆盖！
  }
  ```
  旧行为：多条包级规则独立存储，resolve 时 OR 合并。新行为：**后一条覆盖前一条**。用户配置 `[[rule]] pkg="X" cpus="0-3"` + `[[rule]] pkg="X" sched="fifo:60"` 时只有 sched 生效，cpus 丢失。**这是 TOML→ConfigModel 迁移引入的行为回归**。修复：PolicyModel 合并（字段级"非 None 覆盖"）。
  
- 🔴 **H2**：`to_rules_with_clusters` 占位规则：
  ```rust
  rules.push(RuleConfig { cpus: String::new(), ... });  // 空 cpus
  ```
  意图是"让白名单包含此包"。但 RuleSet::compile 对 `cpus=""` → parse_cpu_ranges 空集 → **判为无效规则跳过**（输出警告）。结果：只有 `thread-type "render" { sched "fifo" }`（无 cpus）的包**不在白名单中**，进程永远不会被发现。修复：RuleSet 支持无 cpus 的"白名单占位"规则，或 ConfigModel 展开时把 sched-only 规则转为带 cpus 的占位（如全部 online cpu）。

- ⚠️ **M3**：KDL 路径 `(m, DaemonConfig::default(), EngineConfig::default())` —— KDL 文件无法配置 daemon/engine 段。需 KDL 支持顶层 `daemon {}` / `engine {}` 节点。
- ⚠️ L8：占位规则注释"让白名单包含此包"与实际行为脱节（见 H2）

### 2.10 kdl_parser.rs（106 行）

- ✅ 属性 + 子节点双语法解析（`sched="fifo"` 和 `sched "fifo"` 都支持）
- ✅ priority 合并逻辑（`sched "fifo"` + `priority 60` → `"fifo:60"`）
- ⚠️ M3 关联：无 daemon/engine 节点处理

### 2.11 store.rs（294 行）

- ✅ 降级链完整（inotify→轮询），Mutex<Arc> 快照原子替换
- ⚠️ **M6**：inotify 失效降级轮询后，若文件被删除，`file_mtime` 返回 -1 ≠ last_mtime → 每轮 `reload()` 失败 → **每 poll_interval 打印一次"配置重载失败"**。修复：文件不存在时静默（或降频）。
- ⚠️ L7：DELETE_SELF 后 `sleep(poll_interval)` 才重装 watch——poll_interval=60s 时恢复监听延迟 60s。（保持兼容行为），但可优化为短 sleep。
- ✅ 测试覆盖版本递增/坏配置保留/Arc 共享

### 2.12 tracker.rs（230 行）

- 🔴 **H4**：`enter()` 无条件 `read_start_time(pid)` —— 即使 pid 已在 tracker 中。relock_all → refresh_process_rules → enter（每进程每轮）→ 读 stat 文件。**优化**：`if !self.procs.contains_key(&pid) { 读 start_time }`。
- ✅ PID 复用检测逻辑正确（start_time>0 且不等 → 移除旧状态）
- ✅ cpuset 引用计数 + rmdir 回收正确（测试覆盖）
- ⚠️ 注意：`enter()` 里 remove+insert 而非 entry().or_insert——行为等价但更慢（可接受）

### 2.13 engine.rs（268 行）

- ✅ 事件分发逻辑正确（Fork/Exec/ThreadClone/Exit）
- ✅ 借用纪律到位（mem::replace 取缓存 + mark_scanned 短借用）
- ⚠️ L6：`applied += 1` 在 getaffinity 短路命中时也计数——"应用 N 个线程"实际含已符合的。telemetry 的 short_circuit 计数尚未接入（P4 项）
- ✅ 配置变更后 relock_all 全量重扫（含新包延迟 1 poll 周期，可接受）
- ⚠️ 注意：`apply_single_tid` 的 resolve None → 标记 applied_tids（防重复尝试），规则变更后由 relock 全量恢复——闭环正确

### 2.14 system_context.rs（218 行）

- ✅ 自适应轮询（10s/3s/1s）符合 ChatGPT 5.5 建议
- ⚠️ **M7**：温度阈值 45/38°C 硬编码（ChatGPT 明确建议读 cooling_device state 或 vendor thermal 等级）——P5.2 待改
- ⚠️ 低：memory pressure 用 "some avg10"，阈值 10/60 无文档来源（启发式，可接受）

### 2.15 decision.rs（230 行）

- ✅ MigrateAction 默认 Observe（ChatGPT 第 1 条落地）
- ✅ TaskScore 权重模型（intent + pressure + thermal）
- ⚠️ **M5**：`from_oom_adj` 阈值：Android 语义 oom_score_adj 500=后台，200=感知服务。当前 `201..=700 => BackgroundLatencySensitive` 把 500（后台）误判。建议：`201..=500 => LatencySensitive, 501..=900 => Background`（或依据 from_sources 的 ThreadHint 修正）

### 2.16 capability.rs（108 行）

- ✅ 文件检测（/proc/sys/kernel/sched_util_clamp_max）避免 SIGSYS（termux 验证）
- ✅ 优先级顺序 uclamp > schedtune > cpuset

### 2.17 foreground.rs（74 行）

- ✅ 三级模型实现（cpuset tasks → UID 缓存）
- ⚠️ M4 关联：未被 daemon 调用（refresh 接口就绪，等 P6 eBPF 集成）
- ⚠️ 低：每次 refresh 读 N 个 status 文件（100+ 进程 ≈ 1ms，可接受）

### 2.18 daemon/main.rs（190 行）

- ✅ CLI 完整（-c/-s/-v/-h），配置热加载闭环
- ✅ Q6 权限检查 + 集群打印
- ⚠️ M4：五模块未接入（decision/system_context/capability/foreground/audit）
- ⚠️ 低：无信号处理（P4 规划项）；poll 期间配置变更延迟 ≤2s（可接受）

### 2.19 daemon/proc_source.rs（155 行）

- 🔴 **H3**：`need_full = proc_total != last_proc_total`——`sysinfo.procs` 是**任务数（含线程）**。多线程进程频繁创建/销毁线程 → procs 每轮变化 → **几乎每轮全扫**（遍历 /proc 全部 pid + 读 cmdline）。**修复**：`proc_total > last_proc_total + 阈值` 或仅增加时全扫；减少时依赖 Exit 检测。
- ✅ 增量路径（tracked 进程线程 diff）正确
- ✅ Exit 检测双路径覆盖（全扫 + 增量）

---

## 三、修复优先级建议

**立即修复（P5.1 批次）**
1. H1：ConfigModel 包级规则合并（字段级非 None 覆盖）
2. H2：RuleSet 支持无 cpus 白名单占位规则（或展开为全部 online cpus）
3. H3：proc_source 全扫阈值（+5 或仅增加时）
4. H4：tracker::enter 仅新 pid 读 start_time

**随 P5.2 修复**
5. M1：detect_clusters 回退用 present cpus
6. M2：EPERM 去重
7. M3：KDL 支持 daemon/engine 节点
8. M5：oom_adj 阈值修正
9. M6：文件缺失时轮询静默

**记录待 P6**
10. M4：五模块接入 daemon 主循环
11. M7：thermal 等级化（冷却设备状态）
12. L1-L8：低优先级清理

---

## 四、审查方法说明

- 全部 19 个文件逐行阅读（3588 行）
- 关键验证：sched_setaffinity 128B 传参安全（内核截断语义）、start_time 字段索引（第 22 字段）、capget V3 布局、PSI 文件格式、Android oom_score_adj 语义
- 未复测：真实设备（SM8550）行为差异、多线程竞态压测
