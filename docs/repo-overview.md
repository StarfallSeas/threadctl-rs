# threadctl-rs — Repository Structure Overview

> Generated: 2026-08-08 · For design review input
> Repo: github.com/StarfallSeas/threadctl-rs · v2.0.0 · 46 unit tests · 5084 lines of Rust

---

## 1. Repository Tree

```
threadctl-rs/
├── Cargo.toml                  # workspace: core / daemon / ebpf（release: z + LTO + strip）
├── Cargo.lock
├── LICENSE                     # GPL-3.0 (full text)
├── README.md / README.en.md    # 双语首页
├── crates/
│   ├── core/                   # lib（纯逻辑，零 aya 依赖，可单测）
│   │   ├── Cargo.toml          # deps: libc, serde, toml, kdl(feature)
│   │   ├── config/threadctl.toml   # TOML 默认模板
│   │   └── src/                # 18 模块
│   ├── daemon/                 # bin: threadctl
│   │   └── src/                # main.rs + proc_source.rs
│   └── ebpf/                   # bin: 内核态（空骨架）
│       └── src/main.rs         # no_std panic-handler 占位
├── docs/
│   ├── matcher.md              # matcher 设计文档（冻结）
│   ├── ai-review-process.md    # 开发/审查流程
│   ├── DeepSeek/               # 架构/阶段设计 + 回应采纳
│   ├── ChatGPT/                # ChatGPT 审查原文
│   └── Claude/                 # Claude 审查原文
├── examples/
│   ├── threadctl.kdl           # 完整 KDL 示例
│   ├── user-mode.kdl           # 用户模式模板
│   └── threadctl.toml          # TOML 示例
└── scripts/                    # i18n 日志 / 检查脚本
```

---

## 2. Crate 依赖关系

```text
threadctl (daemon, bin)
  └── threadctl-core (lib)        ← 全部领域逻辑
        └── libc, serde, toml     （kdl 是 optional feature）
  └── threadctl-ebpf (bin)        ← 独立，无依赖关系（待迁移 既有实现 内核程序）
```

core 不依赖 aya / tracing——纯 syscall + 纯逻辑，这是单测友好的根基。

---

## 3. core 模块详解（18 文件，~4400 行）

### 3.1 配置层

| 模块 | 行数 | 职责 | 关键类型/API |
|---|---|---|---|
| `config.rs` | 775 | 配置模型 + 编译 | `RawConfig`(serde) → `ConfigModel`(AST) → `RuleConfig[]`；`ConfigSnapshot{version, daemon, engine, rules}`；`EngineMode{Auto,Ebpf,Proc,Hybrid}`；`AppConfig{profile,cpus,sched,nice,threads}`；cluster 容错（数字范围自动当 cpus）+ fallback + 非法名告警 |
| `kdl_parser.rs` | 223 | KDL → ConfigModel | `parse_kdl() -> (ConfigModel, DaemonConfig, EngineConfig)`；支持 daemon/engine/app/default/profile/thread/thread-type 节点；裸布尔 `#true`/`"true"` 兼容 |
| `profile.rs` | 182 | 内置模板 | `ProfileModel{default, threads, thread_types}`；`builtin_profiles()`：game/chat/video/launcher/audio/balanced/power-save；`is_valid_profile()` |
| `store.rs` | 302 | 热加载 + 版本化 | `ConfigStore::new/current/reload`（快照 `Mutex<Arc<ConfigSnapshot>>`，失败保留旧快照）；`spawn_hot_reload()` inotify→轮询降级线程，channel 广播版本号 |

### 3.2 匹配与合并层（P6.1 冻结）

| 模块 | 行数 | 职责 | 关键类型/API |
|---|---|---|---|
| `ruleset.rs` | 729 | 规则编译 + 匹配 + 合并 | `RuleSet::compile(&[RuleConfig], &CpuTopology)`；`resolve(pkg, thread) -> Option<Policy>`；`RuleMatch{index, source}`；`RuleSource`（Global..ThreadExact，显式 `priority()` match 表）；nginx 风格 specificity；继承语义（来源并存，字段级覆盖+填充）；实例级缓存 `Mutex<HashMap<pkg, Vec<RuleMatch>>>` |
| `event.rs` | 74 | 事件模型 | `EventKind{Fork,Exec,ThreadClone,CpuMigrate,Exit}`；`ProcessEvent{pid,tid,kind,cpu,pkg}`；`EventSource` trait（poll/on_config_changed/shutdown） |

### 3.3 执行层

| 模块 | 行数 | 职责 | 关键类型/API |
|---|---|---|---|
| `policy.rs` | 367 | 内核动作 | `apply_thread(tid, pkg, &Policy, topo, rt_allowed) -> ApplyOutcome`；`ApplyOutcome{Applied,Exited,BlockedByCgroup,Downgraded,BlockedByPerm,EINVAL,Failed,SkippedNoCpus}`；`Policy{cpus, cpuset_dir, sched, sched_prio, nice}`；三层过滤（online→allowed 交集→setaffinity）；EPERM/EINVAL/cpuset 失败 warn_once 去重；audit 全路径记录 |
| `engine.rs` | 288 | 事件引擎 | `handle_events(&mut StateTracker, &[ProcessEvent], cfg, topo, now)`；`relock_all`（读 oom_adj 跳过后台）；`cleanup_dead`；`tracked_summary`；`refresh_process_rules`（线程名 TTL 缓存 + fnmatch 匹配 + apply） |
| `tracker.rs` | 254 | 进程状态 | `StateTracker`：start_time 防 PID 复用；每进程线程名缓存（TTL 60s，exec 失效）；cpuset 目录引用计数回收；`enter/get/get_mut/remove/register_dirs` |

### 3.4 感知与决策层（P5 骨架，部分未接入主循环）

| 模块 | 行数 | 职责 | 关键类型/API | 接入状态 |
|---|---|---|---|---|
| `decision.rs` | 216 | 决策引擎 | `TaskIntent{Interactive,BackgroundLatencySensitive,Background,Frozen}`（from_oom_adj/from_sources）；`ActionLevel{Observe,Steer,Force}`；`TaskScore` 权重模型；`DecisionEngine::decide/evaluate`；`MigrateAction{Observe,Suggest,Force}` | ⚠️ 未接入主循环（relock 仅用 from_oom_adj 阈值） |
| `system_context.rs` | 285 | 系统状态 | `PressureLevel{Normal,Moderate,Critical}`；`SystemContext::sample()`（PSI + thermal_zones + battery + thermal_pressure 缓存）；`AdaptivePoller`（10s/3s/1s） | ✅ 主循环采样，异常时打印 |
| `capability.rs` | 93 | 能力检测 | `CapabilitySet::detect()`；`preferred_order()`（uclamp>schedtune>cpuset）；文件探测避免 SIGSYS | ✅ 启动打印 |
| `foreground.rs` | 74 | 前台检测 | `refresh_foreground_uids() -> usize`；`is_foreground_uid(uid)`；UID 缓存 | ⚠️ 主循环 30s 刷新，仅 debug 打印 |
| `audit.rs` | 203 | 审计闭环 | `AuditEntry{timestamp,tid,pkg,requested,effective,success,reason}`；256 环形缓冲；`summary()/summary_windowed(60)`；`record()` | ✅ 主循环 60s 摘要打印 |

### 3.5 工具层

| 模块 | 行数 | 职责 | 关键 API |
|---|---|---|---|
| `topology.rs` | 384 | CPU 拓扑 | `CpuSet`（1024 位图，与 cpu_set_t 布局一致）；`CpuTopology{present,online,clusters,cpuset_enabled}`；`detect_clusters()`（cpu_capacity → Little/Big/Prime）；`read_allowed_mask()`；cpuset 目录创建/回收 |
| `proc.rs` | 111 | /proc 工具 | `read_cmdline`（栈上缓冲）；`read_thread_name`；`read_start_time`（PID 复用检测）；`read_oom_adj`；`list_tids` |
| `caps.rs` | 61 | 权限检查 | `can_rt_sched()`（euid==0 短路或 capget CAP_SYS_NICE） |
| `lib.rs` | 34 | crate 门面 | compile_error 防护（非 Linux/Android、32 位）；模块导出 |

---

## 4. daemon 模块（2 文件，~420 行）

| 模块 | 行数 | 职责 | 关键 API |
|---|---|---|---|
| `main.rs` | 242 | 主循环 | CLI（-c/-s/-v/-h）；配置加载 + 热加载线程；`ProcSource` 事件循环（poll → handle_events）；relock（60s）/cleanup（15s）/foreground 刷新（30s）/audit 摘要（60s）；SystemContext 自适应采样；Capability 打印 |
| `proc_source.rs` | 177 | /proc 事件源 | `ProcSource::collect()`：**/proc 目录计数**（Bionic sysinfo.procs 是线程数的替代）+ 阈值 5 触发全扫；增量路径（tracked 进程线程差集 → ThreadClone）；Exit 检测；`new_tids_for()` |

---

## 5. 配置格式

```kdl
daemon { pid-file "..."; ipc-socket "..."; log-level "info" }
engine { mode "auto"; scan-interval 2; lock-interval 60; migrate-action "observe"; pressure-sensitive "true" }

app "com.miHoYo.Yuanshen" {
    profile "game"                          # 模板（不绑定应用）
    default { cluster "big" }               # 未命名线程默认
    thread "UnityMain" { cluster "prime"; sched "fifo"; priority 10 }
    thread-type "render" { cluster "big" }  # 别名展开
}
```

- 包名通配：`com.tencent.*`（最长固定前缀优先）
- `cluster` 数字范围自动识别为 cpus（`cluster "0-6"` ≡ `cpus "0-6"`）
- TOML 等价：`[app."pkg"]` + `[app."pkg".threads.Name]`

---

## 6. 运行时数据流

```text
/proc 轮询（ProcSource）                   配置热加载（store inotify）
   │ 事件流：Fork/Exec/ThreadClone/Exit        │ 版本号 channel
   ▼                                          ▼
engine::handle_events ──▶ StateTracker ──▶ ConfigSnapshot（新实例，缓存自然失效）
   │ ruleset.resolve(pkg, thread)
   ▼
Policy{cpus, cpuset_dir, sched, nice}
   ▼
policy::apply_thread（三层过滤 + cpuset 移入 + audit 记录）
   ▼
内核：sched_setaffinity + /dev/cpuset/threadctl/*/tasks + sched_setscheduler

周期任务：relock（60s，跳过 oom_adj>500）/ cleanup（15s）/ SystemContext（10s/3s/1s）/ audit 摘要（60s）
```

---

## 7. 当前状态与设计输入

### 已完成（冻结）
- P0-P2：workspace / ConfigStore 热加载 / proc 全链路
- P5：五模块骨架（audit 闭环 + decision + system_context + capability + foreground）
- P6.0：Profile 抽象（7 模板）
- P6.1：matcher（RuleMatch / specificity / 继承语义 / 实例缓存）—— **冻结，不再改**

### P6.2 设计输入（ChatGPT 5.5 V2 已定界）
1. **Policy Merge Engine**：从 `ruleset.rs` 拆出 `merge.rs`——数据驱动 merge table：
   `MergeStrategy{Override, FillMissing, BitOr, FirstWins}` 按字段映射
   （Cpus→BitOr / Sched→Override / Nice→FirstWins），未来 profile/group 来源接入不重构
2. **DecisionEngine 接入 relock**：替换裸 oom_adj 阈值 → TaskIntent + SystemContext
   + Foreground + Audit；输出 **allow/skip/degrade**（不做 kill/freeze/migrate）
3. **Backend 抽象**：AffinityBackend / CpusetBackend / SchedBackend（Android cgroup v1→v2 迁移准备）
4. **Zygote fork 空窗**：PendingProcess{pid, first_seen, retry_count, deadline}
   + 指数退避（100ms/300ms/1s），非固定 200ms
5. **tracing 日志**：eprintln → tracing 宏 + Android logcat

**明确不做（ChatGPT V2 禁止项）**：userspace freezer、自动 CPU 迁移、
后台杀进程、scheduler replacement。`inherit false` 暂缓（观察真实配置需求）。

### 已记录的技术债
- `decision.rs` / `foreground.rs` 未真正接入主循环（仅启动打印 / debug）
- 多进程 App（`:service`）子进程需单独配置
- MIUI 冻结进程（SIGSTOP）relock 浪费 → is_frozen 检查
