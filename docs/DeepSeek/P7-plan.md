# P7 规划书 — eBPF 事件源 · 算法增强 · 系统架构

> 作者：DeepSeek V4 Flash（执行者）
> 审核待办：Claude（代码·架构）· ChatGPT（文档）
> 日期：2026-08-09
> 状态：规划阶段（未实现）· 请审
> 基准：P6.3 完成（73 测试 / 867KB / v2.0.0-dev / 四 SoC 适配）

---

## 〇、方向总览

P7 在"不替代调度器 / 不杀进程 / 不冻结"原则内，分四个领域：

```text
A. eBPF 内核态事件源     ← 主线（原 P7 预留，解决 2s 轮询延迟）
B. 算法增强             ← 自适应 relock / DVFS 域感知绑核 / 决策强化
C. 系统架构             ← IPC CLI / dry-run / 能力降级链统一
D. Android 生态         ← 屏幕状态 / 冻结感知 / AMS 覆盖对抗
```

每个方向独立可交付，A 是唯一主线，B/C/D 按价值排序插队。

---

## 一、A. eBPF 内核态事件源（主线）

### A1. 现状与痛点

- 当前 ProcSource：/proc 轮询（scan-interval 2s）→ fork/exec 事件延迟 **0-2s**
- 新线程产生到策略应用的最坏延迟 2s（抖音类高频线程创建场景明显）
- eBPF crate 是空壳（10 行 main.rs）

### A2. 设计

```text
内核态（aya-ebpf，no_std）：
  sched_process_fork   tracepoint → 产 Fork 事件（pid + ppid + comm）
  sched_process_exec   tracepoint → 产 Exec 事件（pid + 新 comm）
  sched_process_exit   tracepoint → 产 Exit 事件（pid，触发 tracker 清理）
  sched_switch         tracepoint → 产 Migrate 事件（prev_pid + 目标 CPU，
                                      P5 migrate-action observe/suggest/force 落地）

ringbuf → 用户态（aya + tokio/线程）：
  RingBuf → 事件队列 → engine::handle_events（复用现有管道，零架构改动）

降级链：
  eBPF 加载失败（无权限/内核禁 BPF）→ 自动回退 ProcSource（现有路径）
  事件丢失（ringbuf 溢出）→ 周期全扫兜底（现有 TTL 机制）
```

### A3. 关键点

- **复用现有事件管道**：`ProcessEvent{pid, tid, pkg, kind}` 已定义——eBPF 事件
  只需补 comm，pkg 由 engine 回退读 /proc（现有 `read_cmdline` 逻辑已支持）
- Zygote pending 队列**天然兼容**：fork 事件到达时 cmdline 可能未就绪——
  现有 pending/退避逻辑直接复用
- 依赖：`aya` + `aya-ebpf`（aya-ebpf 已有实战经验）
- 工作量：~400 行内核态 + ~200 行用户态 + 测试

### A4. 不做

- ❌ 内核态直接写 cpuset/affinity（eBPF 不能安全做，事件回用户态处理）
- ❌ 替代 ProcSource（proc 保留为降级路径）

---

## 二、B. 算法增强（不替代调度器原则内）

原则：**策略内容由用户显式定义（集群/范围不变），算法只优化"如何执行策略"**。

### B1. 自适应 relock 间隔（高价值）

**痛点**：固定 `lock-interval 60`——Android AMS 每 1-5s 可能覆盖 cpuset 归属，
60s 周期对抗太慢；但高频 relock（1s）在无覆盖时浪费电。

**设计**：动态调整周期
```text
信号：audit downgraded/cpuset_write_failed 率 + 事件触发的 apply 中
      "移入后线程不在我们 cpuset" 的比例（检测系统覆盖频率）
逻辑：覆盖频繁 → 缩短（60s → 10s → 3s 下限）
      连续稳定 → 延长（10s → 60s → 300s 上限）
约束：仅在事件路径观察到覆盖时加速，无覆盖时退回长周期省电
```
- 工作量：~80 行 + 单测（模拟覆盖率输入）

### B2. DVFS 域感知绑核（P6.3 直接延伸）

**现状**：P6.3 M2 已探测 `dvfs_domains`，但绑核仍按 capacity 集群。

**设计**：同档位内优先选择**完整 DVFS 域**：
```text
用户配 cluster "big"（SM8550: 3-6）→ 目标 3-6 已是一个完整 DVFS 域 ✓ 现状即可
用户配 cpus "5-7"（跨域：5-6 一个域 + 7 一个域）→ 保持用户显式范围（不改）
用途：文档化 + 日志提示"该范围跨 DVFS 域，同域同频更优"（不自动改用户配置）
```
- 价值：主要是**提示与文档**，不改变用户策略——控制 scope
- 工作量：~40 行（日志 + 校验函数）

### B3. DecisionEngine 强化

- **foreground 三源接入**（BUG-M1，P6.4 遗留）：`from_sources(oom_adj, is_foreground_uid, thread_hint)`——真实前台判定（当前是 oom_adj 代理）
- **thermal 趋势**：`thermal_pressure` 一阶导数（升温中 vs 平稳）→ Degrade 更激进
- **relock debounce**：前台切换瞬间（UID 刷新 30s 周期内）避免全量 relock 风暴
- 工作量：~120 行 + 测试

### B4. 空闲核启发式（可选，P7 后期）

- 同档位内选 idle 率高的核（/proc/stat，root 可读）——满足策略前提下的微优化
- 风险：与 Android EAS/schedutil 交互复杂，收益边际 → **P7 末评估，不做承诺**

---

## 三、C. 系统架构

### C1. IPC CLI（高价值，可用性短板）

**现状**：`ipc-socket` 配置字段存在但**零实现**——daemon 无法查询/控制。

**设计**：Unix socket + 简单 JSON 协议
```text
threadctl status    → 跟踪进程/线程数、audit 摘要、relock 统计
threadctl dump      → 单个 pid 的当前策略/实际掩码/归属 cpuset
threadctl reload    → 触发配置热加载（替代改文件等待）
threadctl apply <pid> → 立即重新应用策略（诊断用）
```
- 复用现有：tracked_summary() / audit::summary() / relock_stats() 全已实现
- 工作量：~150 行（daemon 侧监听线程 + CLI 侧）

### C2. 配置 dry-run（低价值高实用）

```text
threadctl -t -c config.kdl   → 解析 + 展开 + 打印规则（不启动 daemon）
用途：改配置后先验证（cluster 名/线程名截断/通配命中）
```
- 工作量：~50 行（复用 ConfigStore + RuleSet 编译，只加打印出口）

### C3. 能力降级链统一

现状：cpuset/uclamp/RT/eBPF 各自探测各自降级——P7 统一为 CapabilitySet 演进：
```text
CapabilitySet::detect() → {cpuset, uclamp, rt, ebpf} + 启动日志汇总
每个能力独立降级（缺失不影响其他），日志一次打印全貌
```
- 工作量：~60 行

### C4. tracing 结构化日志（P7 末评估）

- 现状：println/eprintln 混合（日志已英文化）
- 可选：`tracing` + 文件输出（release +~100KB）——**与 867KB 目标权衡**，默认不做，
  保持 println 简化；如需文件日志用 shell 重定向即可

---

## 四、D. Android 生态

### D1. 屏幕状态感知（高价值）

- 读 `/sys/class/backlight/*/brightness`（>0 = 亮屏）或
  `/sys/class/power_supply/*/status`
- 用途：息屏 → relock 降频（长周期）+ Skip 后台应用 relock（省电）
- 注意：不同 ROM 接口差异 → 多路径探测 + 降级（无接口 = 常亮处理）
- 工作量：~60 行

### D2. 冻结进程感知（MIUI 等）

- `/dev/cgroup` 冻结状态 / `process freezer`（cgroup v1 freezer 子系统）
- 用途：冻结进程跳过 relock/apply（省 syscall）
- 工作量：~50 行；**依赖厂商实现差异**，先探测后接入

### D3. AMS 覆盖对抗增强（B1 的补充）

- 检测线程被移出我们 cpuset（`/proc/<tid>/cpuset` 归属变化，in_our_cpuset 已有）
- 事件驱动即时 relock（不等周期）——与 B1 自适应周期互补
- 工作量：~60 行（复用 policy.rs 已有归属验证）

---

## 五、优先级矩阵

| 优先级 | 项 | 价值 | 成本 | 依赖 |
|---|---|---|---|---|
| **P0** | A. eBPF fork/exec 事件源 | 事件延迟 2s→亚毫秒 | ~600 行 | aya |
| **P1** | B1 自适应 relock | 对抗 AMS 覆盖（真实痛点） | ~80 行 | audit 已有 |
| **P1** | C1 IPC CLI | 可用性短板 | ~150 行 | 无 |
| **P1** | B3 DecisionEngine 强化 | 决策质量 | ~120 行 | foreground 已有 |
| **P2** | D1 屏幕状态感知 | 息屏省电 | ~60 行 | 无 |
| **P2** | D3 cpuset 归属对抗 | B1 互补 | ~60 行 | in_our_cpuset 已有 |
| **P2** | B2 DVFS 域提示 | 文档/日志 | ~40 行 | P6.3 M2 已有 |
| **P2** | C2 dry-run | 配置验证 | ~50 行 | 无 |
| **P3** | C3 能力降级链统一 | 整洁 | ~60 行 | 无 |
| **P3** | D2 冻结感知 | 厂商依赖 | ~50 行 | 探测 |
| **P3** | A-sched_switch 迁移观察 | P5 补完 | ~300 行 | A 主线后 |
| **评估** | B4 空闲核启发式 / C4 tracing | 边际 | — | P7 末 |

---

## 六、里程碑

| 阶段 | 内容 | 交付 |
|---|---|---|
| P7.1 | eBPF fork/exec/exit 事件源 + 降级链 | 事件延迟 2s→亚毫秒 |
| P7.2 | 自适应 relock + cpuset 归属对抗（B1+D3） | AMS 覆盖收敛 |
| P7.3 | IPC CLI（status/dump/reload/apply）+ dry-run | 可观测/可控制 |
| P7.4 | DecisionEngine 强化 + 屏幕状态（B3+D1） | 决策质量 |
| P7.5 | sched_switch 迁移观察（P5 补完）+ 收尾 | P5 全落地 |

---

## 七、风险与开放问题

1. **eBPF 内核限制**：Android GKI 内核通常允许 BPF 但可能禁 ringbuf/特定 tracepoint——
   加载失败自动回退 proc（已有模式），无阻塞
2. **自适应 relock 阈值**：覆盖率检测的窗口/阈值需真机校准（B1）
3. **IPC 安全**：Unix socket 权限（root-only 0750）+ 命令白名单——不做任意命令执行
4. **屏幕状态接口差异**：ROM 差异大，多路径探测失败 = 常亮处理（保守）
5. **不替代调度器**：B4 空闲核启发式若与 EAS 冲突即放弃（评估项）

---

## 八、本次不做的（明确范围外）

- ❌ 内核态 cpuset/affinity 写入（eBPF 只发事件）
- ❌ 替代 ProcSource（proc 为降级路径）
- ❌ tracing 日志系统（保持 println，release 大小优先）
- ❌ 任何新策略语义（cluster/cpus/sched 语法不变）
- ❌ 调度器替代 / 冻结 / 杀进程（non-goals 延续）
