# P9 Delivery — 非 eBPF 路线全部落地 + 日志体系重构

> **ADR 风格头**
> Author: DeepSeek V4 Flash（执行者）
> Reviewers: Claude（代码·架构）· ChatGPT（文档）
> Status: Delivered — 119 单测全绿、零警告、SM8550 真机验证
> Date: 2026-08-13
> References: P8.1-evolution.md（P10-P14 路线）· 用户指令（日志重构/双语/eBPF 状态）

---

## 〇、交付总览

| Commit | 内容 |
|---|---|
| `61eaa40` | P10 DVFS 感知 + P12 apply-scene + P14 tuning + P11 libthreadctl + 日志重构（i18n/精简/debug 流/eBPF 状态） |

**当前**：119 测试（core 102 + daemon 13 + libthreadctl 4）、零警告、release 1.1MB。

---

## 一、日志体系重构（用户指令）

### 1.1 i18n 双语（中英；其他语言 → 英文）

- `crates/core/src/i18n.rs`：`is_zh()`（getprop persist.sys.locale → ro.product.locale → LANG/LC_ALL，单次缓存）+ `t(zh, en)`
- 用户可见日志双语：启动块（拓扑/配置/eBPF 状态/IPC/版本）、警告（降级/权限/回收失败/governor）、错误（CLI/加载）、IPC 响应
- debug 工程日志保持英文（原有不动——调试受众）

### 1.2 普通模式精简（减日志开销）

**debug-only 门控**（普通模式不打印）：events/applied、audit 摘要、relock decisions、d3 immediate、adaptive、periodic relock、cleanup、config change rescan、SystemContext、cpuset reclaimed。

**普通模式保留**：启动摘要（拓扑/集群/DVFS/配置/eBPF 状态/白名单/IPC/RT/版本）、一次性警告（降级/失败/不可用）、错误、SIGTERM。

### 1.3 debug 模式详细流（含包/进程/线程名）

```
[debug][ebpf] whitelist keys: ["com.test", "sleep"]
[debug][ebpf] initial scan: pid=14316 pkg=sleep
[debug][engine]   tid=14316 name="" -> rule (cpus=... dir="3-7")
[debug][engine]     apply tid=14316 outcome=Applied
[debug][ebpf] raw event type=2 pid=14351 tid=14351 comm="sleep"
```

### 1.4 eBPF 可用性启动打印

- 可用：`eBPF event source: available (fork/exec/exit tracepoints)`
- 降级：`warning: eBPF unavailable (<原因>) — falling back to /proc polling`

---

## 二、P10 DVFS 感知选核

`SystemContext::read_freq_throttle()`——枚举 cpufreq policy，取最低 `scaling_cur_freq/scaling_max_freq` 比例 → `freq_throttle`（1.0=满频）→ RelockContext → DecisionContext。

决策：`effective_thermal = thermal_pressure.max(1.0 - freq_throttle)`——严重降频视作热压 → Degrade（Relax）。

**真机**：SM8550 检测到 walt/schedutil 等 governor；测试 `freq_throttle_triggers_degrade`。

## 三、P12 场景一键套用

`threadctl apply-scene <game|video|power-save|balanced|default>`：

- 场景 = 引擎参数预设（不碰 app 规则——避免与用户配置冲突）
- 写回配置（`// <<scene: name>>` 标记段）→ 移除旧段 → 追加新段 → reload 热生效
- 真机：`scene applied: game (config v2, re-applied 0 threads)` + 配置写回验证

## 四、P14 系统 tuning

- KDL：`system { governor "schedutil"; io-scheduler "mq-deadline" }`——启动时应用
- IPC：`threadctl tune governor <name>` / `tune iosched <name>`——写 sysfs + 可用列表校验
- 真机：非法 governor 报错并列出可用列表（walt/conservative/powersave/performance/schedhorizon/schedutil）
- **注意**：Android sysfs cpufreq 在 SELinux enforcing 下 root 也写不了（只读挂载）——失败警告正确降级，不崩溃

## 五、P11 libthreadctl 库形态

新 workspace crate（embeddable affinity thread pool）：

```rust
let pool = AffinityPool::new("render", "6-7", 2)?;  // 2 个 worker 绑 6-7 核
pool.spawn(|| { /* 在绑核线程执行 */ })?;           // round-robin 分发
```

- 底层：threadctl-core CpuSet + libc::sched_setaffinity
- 与 daemon 互补：daemon 管理已有进程线程（游戏）；lib 应用内嵌（服务器/中间件/引擎）
- 4 测试（解析/非法输入/任务执行/绑定）

---

## 六、测试矩阵（119 全绿）

| crate | 测试 | 本轮新增 |
|---|---|---|
| core | 102 | i18n ×1 + P10 ×1 + scene ×3 + tune ×1 |
| daemon | 13 | apply-scene ×1 + tune ×2 |
| libthreadctl | 4 | 新 crate 全量 |
| **合计** | **119** | **+9** |

零警告。

---

## 七、遗留与下一步

| 项 | 计划 |
|---|---|
| P9（eBPF 内核态决策——用户指令排除） | 暂缓（调研 eBPF setaffinity helper 可行性） |
| 中文设备真机验证（当前设备英文——i18n 逻辑由单测保证） | 用户侧 |
| libthreadctl 发布（crates.io / 示例仓库） | 可选 |
| 场景表扩展（自定义场景） | P9.1 |

---

## 八、请两位审

### Claude（代码·架构）
1. P10 `effective_thermal = max(thermal, 1.0 - freq_throttle)` 组合——降频与热的冗余惩罚（两者相关）是否合理？还是应加权？
2. apply-scene 写回配置文件（标记段）——与 inotify 热加载的竞态（写回期间热加载线程触发 reload）？
3. tune 的 sysfs 失败路径（Android SELinux enforcing 写不了）——失败警告每 CPU/设备一次（apply_governor 首个失败即返回）是否够友好？
4. libthreadctl 的 spawn(&self) round-robin 无背压——任务积压时内存增长，是否需要有界队列？

### ChatGPT（仅文档）
5. 日志分级（普通/debug）文档是否清晰？双语范围（哪些消息双语、哪些英文）是否明确？
6. P9 交付叙事与 P8.1 路线（P10-P14）一致性？
