# P7 规划书 v2（定稿）— eBPF 事件源 · 算法增强 · 系统架构

> 作者：DeepSeek V4 Flash（执行者）
> 审核：ChatGPT（文档/架构）· Claude（架构/代码）——意见已齐，裁决完成
> 日期：2026-08-09
> 状态：**定稿（v2）——三 AI 意见一致，可开始构建**
> 基准：P6.3 完成（73 测试 / 867KB / v2.0.0-dev / 四 SoC 适配）

---

## 〇、审查裁决记录（三 AI 合议）

**依据**：`docs/ChatGPT/P7-plan.md` + `docs/Claude/P7-plan.md`（新大版本三审制首轮）。

### 一致意见（全部采纳）

| # | 意见 | 落地 |
|---|---|---|
| 1 | eBPF 性能表述降调（"亚毫秒"→"事件发现延迟降低"） | 见 A3 |
| 2 | sched_switch → P7.5 实验性，不进默认功能 | 见 A2/优先级 |
| 3 | 自适应 relock 需防震荡（cooldown + 观察窗口 ≥30s） | 见 B1 |
| 4 | DecisionEngine 保持 gate 语义（Allow/Skip/Degrade），不进优化器 | 见 B3 |
| 5 | 冻结感知改名"外部生命周期状态感知"（非冻结功能） | 见 D2 |
| 6 | dry-run 输出"输入规则→来源→合并结果"格式 | 见 C2 |
| 7 | D1 屏幕状态经 DecisionEngine 接入（不直接改策略） | 见 D1 |
| 8 | B2 DVFS 定位 validation/warning 层（不改用户配置） | 见 B2 |

### 分歧裁决（执行者定夺）

| 分歧 | ChatGPT | Claude | 裁决 |
|---|---|---|---|
| IPC 优先级 | P7.1 提前 | 保持 P7.3 | **采纳 Claude**——eBPF 是功能正确性问题（新线程 2s 无策略），IPC 是可调试性问题；P7.1 聚焦单一主线 |
| P7/P8 拆分 | 拆分 | 维持 P7.1-P7.5 | **采纳 Claude**——milestone 已分阶段独立交付，强行拆增加版本语义成本 |

### Claude 独有架构缺口（全部采纳）

| # | 缺口 | 落地 |
|---|---|---|
| ARCH-1 | `EventSource` trait 不存在（ProcSource 是自由方法）→ P7.1 显性前置 | 见 A2/milestone |
| ARCH-2 | `sched_process_fork` 同时捕获进程 fork 与线程 clone，需 `tgid!=pid` 分流 | 见 A2 |
| ARCH-3 | B1 与 D3 双触发竞争 → 共享 `RelockGuard{last_at, cooldown_ms}` | 见 B1 |
| IMPL-1 | eBPF 构建链复杂（bpf-linker/LLVM/BTF）→ P7.1 第一步前置验证 | 见 A4/milestone |
| IMPL-2 | IPC 架构决策：独立线程 + mpsc channel（与 hot-reload 一致） | 见 C1 |
| IMPL-4 | `sched_process_exit` 兼处理线程退出 → 即时清 applied_tids，修 TID 复用 | 见 A2 |

**裁决结论：三位 AI 意见一致（含执行者裁决），P7 规划定稿，可开始构建。**

---

## 一、A. eBPF 内核态事件源（主线，P7.1）

### A1. 现状与痛点

- 当前 ProcSource：/proc 轮询（scan-interval 2s）→ fork/exec 事件延迟 **0-2s**
- 新线程产生到策略应用的最坏延迟 2s（抖音类高频线程创建场景明显）
- eBPF crate 是空壳（10 行 main.rs）

### A2. 设计

```text
内核态（aya-ebpf，no_std）：
  sched_process_fork   tracepoint → Fork / ThreadClone 事件
                          （ARCH-2：tgid==pid → Fork；tgid!=pid → ThreadClone，
                            线程克隆不进全量 Fork 路径，保 apply_single_tid 生效）
  sched_process_exec   tracepoint → Exec 事件（pid + 新 comm）
  sched_process_exit   tracepoint → Exit 事件（进程 + 线程）
                          （IMPL-4：线程退出即时清 applied_tids，修 TID 复用窗口）
  sched_switch         tracepoint → Migrate 事件（P7.5 实验性，默认关闭——
                          高负载 Android 数十万 events/s，ringbuf/消费线程压力大）

ringbuf → 用户态（aya + 线程）：
  RingBuf → 事件队列 → engine::handle_events（复用现有管道）

降级链：
  eBPF 加载失败（无权限/内核禁 BPF/无 BTF）→ 自动回退 ProcSource（现有路径）
  事件丢失（ringbuf 溢出）→ 周期全扫兜底（现有 TTL 机制）
```

### A3. 关键点

- **ARCH-1 前置：`EventSource` trait 提取**（P7.1 第一步）：
  现有 `ProcSource::collect()` 是自由方法 → 提为
  `trait EventSource { fn collect(...) }`，`ProcSource`/`EbpfSource` 各自实现，
  daemon 持 `Box<dyn EventSource>`——这是 P7.1 的显性依赖，先行完成
- **复用现有事件管道**：`ProcessEvent{pid, tid, pkg, kind}` 已定义——eBPF 事件
  只需补 comm，pkg 由 engine 回退读 /proc（现有 `read_cmdline` 逻辑已支持）
- Zygote pending 队列**天然兼容**：fork 事件到达时 cmdline 可能未就绪——
  现有 pending/退避逻辑直接复用
- **性能表述**（三审一致）：降低事件**发现**延迟（轮询周期级 → 内核通知级，
  near-real-time），非端到端策略生效延迟——Zygote 场景仍受 pending 退避
  （100-1400ms）主导；eBPF 对非 Zygote 的 native daemon 改善最明显
- 依赖：`aya` + `aya-ebpf`

### A4. 构建链前置验证（IMPL-1，P7.1 第 0 步）

eBPF 成本在构建链而非代码行数，写第一行 eBPF 前必须验证：
1. Termux 安装 `bpf-linker`（依赖特定 LLVM 版本）+ `--target bpfel-unknown-none`
2. 目标设备 BTF：`/sys/kernel/btf/vmlinux` 存在（SM8550 5.15 有；SM8650/SM8750 待验）
3. 产出有效 `.bpf.o` + ringbuf 最小可跑样例

验证不通过 → eBPF 延后，P7.1 改为纯 trait 提取 + IPC（见裁决：IPC 可提前为降级预案）。

### A5. 不做

- ❌ 内核态直接写 cpuset/affinity（eBPF 不能安全做，事件回用户态处理）
- ❌ 替代 ProcSource（proc 保留为降级路径）
- ❌ sched_switch 进默认功能（P7.5 实验性，默认关闭）

---

## 二、B. 算法增强（不替代调度器原则内）

原则：**策略内容由用户显式定义（集群/范围不变），算法只优化"如何执行策略"**。

### B1. 自适应 relock 间隔（高价值）

**痛点**：固定 `lock-interval 60`——Android AMS 每 1-5s 可能覆盖 cpuset 归属，
60s 周期对抗太慢；但高频 relock（1s）在无覆盖时浪费电。

**设计**：动态调整周期（三审一致：**必须防震荡**）
```text
信号：audit downgraded/cpuset_write_failed 率 + 事件触发的 apply 中
      "移入后线程不在我们 cpuset" 的比例（检测系统覆盖频率）
逻辑（ARCH-3：与 D3 共享 RelockGuard，所有 relock 入口统一检查）：
  struct RelockGuard { last_at: Instant, cooldown_ms: u64 }
  - 周期 relock（B1）与即时 relock（D3）都先查 guard，cooldown 内不执行
  - 单次覆盖失败不调整——连续观察窗口 ≥30s 确认持续覆盖才缩短
    周期（60s → 10s → 3s 下限）
  - 连续稳定（无覆盖）→ 延长（10s → 60s → 300s 上限）
约束：防 AMS↔threadctl 震荡（检测→修改→检测→修改死循环）
```
- 工作量：~100 行（含 RelockGuard）+ 单测（模拟覆盖率输入 + cooldown 语义）

### B2. DVFS 域感知绑核（P6.3 直接延伸）

**定位**（三审一致）：**validation/warning 层，不是自动拓扑优化器**。

```text
用户配 cluster "big"（SM8550: 3-6）→ 目标 3-6 已是一个完整 DVFS 域 ✓ 现状即可
用户配 cpus "5-7"（跨域：5-6 一个域 + 7 一个域）→ 保持用户显式范围（不改）
用途：日志提示"该范围跨 DVFS 域，同域同频更优"（不自动改用户配置）
```
- 工作量：~40 行（日志 + 校验函数）

### B3. DecisionEngine 强化

**边界**（三审一致）：保持 gate 语义——只输出 `Allow/Skip/Degrade`，
**不扩展** Move/Boost/Balance/Optimize（否则逐渐变成 scheduler）。

- **foreground 三源接入**（BUG-M1 遗留）：`from_sources(oom_adj, is_foreground_uid, thread_hint)`——真实前台判定（当前是 oom_adj 代理）
- **thermal 趋势**：`thermal_pressure` 一阶导数仅作 **degrade signal**（升温中 → Degrade 更激进），不参与 CPU 选择
- **relock debounce**：前台切换瞬间避免全量 relock 风暴
- 工作量：~120 行 + 测试

### B4. 空闲核启发式（可选，P7 后期）

- 同档位内选 idle 率高的核（/proc/stat，root 可读）——满足策略前提下的微优化
- 风险：与 Android EAS/schedutil 交互复杂，收益边际 → **P7 末评估，不做承诺**

---

## 三、C. 系统架构

### C1. IPC CLI（高价值，可用性短板）

**现状**：`ipc-socket` 配置字段存在但**零实现**——daemon 无法查询/控制。

**架构决策**（IMPL-2，三审采纳）：**独立线程 + mpsc channel**，与 hot-reload 线程模式一致：
```text
daemon 侧：IPC 监听线程（UnixListener，root-only 0750 + 命令白名单）
           → 请求经 mpsc::channel 发回主循环 → 主循环执行（持有 tracker）→ 回写
           主循环保持单线程可变状态所有权（不加额外 Arc<Mutex>）

命令：
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

输出格式（三审采纳 ChatGPT 建议）：
app "com.tencent.mm"
 ├ wildcard: com.tencent.*  (cpus=3-6)
 ├ exact: com.tencent.mm     (RenderThread: sched=fifo:60)
 └ final (RenderThread): cpus=3-6, sched=fifo:60
```
- 用途：改配置后先验证（cluster 名/线程名截断/通配命中/继承结果）
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

### D1. 屏幕状态感知（中等价值）

- 读 `/sys/class/backlight/*/brightness`（>0 = 亮屏）或 `/sys/class/power_supply/*/status`
- **接入方式**（三审一致）：作为 DecisionEngine 输入，**不直接改策略**：
  `ScreenState → TaskIntent/DecisionContext → Allow/Skip/Degrade`
  （如息屏 → 后台应用更倾向 Skip，省电）——决策引擎是唯一策略门控
- 不同 ROM 接口差异 → 多路径探测 + 降级（无接口 = 常亮处理）
- 工作量：~60 行

### D2. 外部生命周期状态感知（改名，非冻结功能）

**改名**（三审一致）：~~冻结进程感知~~ → **external lifecycle state detection**。

- 说明：threadctl **不冻结**（non-goal 延续）；只**检测**外部已冻结/挂起
  的进程（cgroup freezer / vendor 机制），避免对其做无意义的 relock/apply（省 syscall）
- 依赖厂商实现差异 → 先探测后接入；探测不到 = 不感知（行为不变）
- 工作量：~50 行

### D3. AMS 覆盖对抗增强（B1 的补充）

- 检测线程被移出我们 cpuset（`/proc/<tid>/cpuset` 归属变化，in_our_cpuset 已有）
- 事件驱动即时 relock（不等周期）——与 B1 自适应周期互补
- 工作量：~60 行（复用 policy.rs 已有归属验证）

---

## 五、优先级矩阵

| 优先级 | 项 | 价值 | 成本 | 依赖 |
|---|---|---|---|---|
| **P0** | A. eBPF fork/exec/exit 事件源（含 EventSource trait 前置） | 事件发现延迟降低 | ~600 行 + 构建链 | aya/BTF |
| **P1** | B1 自适应 relock（含 RelockGuard） | 对抗 AMS 覆盖（真实痛点） | ~100 行 | audit 已有 |
| **P1** | B3 DecisionEngine 强化（foreground 三源/thermal 趋势） | 决策质量 | ~120 行 | foreground 已有 |
| **P2** | C1 IPC CLI | 可用性短板 | ~150 行 | 无（P7.3） |
| **P2** | D3 cpuset 归属对抗（与 B1 共享 guard） | B1 互补 | ~60 行 | in_our_cpuset 已有 |
| **P2** | D1 屏幕状态感知（经 DecisionEngine） | 息屏省电 | ~60 行 | 无 |
| **P2** | C2 dry-run（含来源树输出） | 配置验证 | ~50 行 | 无 |
| **P2** | B2 DVFS 域提示（validation 层） | 日志/文档 | ~40 行 | P6.3 M2 已有 |
| **P3** | C3 能力降级链统一 | 整洁 | ~60 行 | 无 |
| **P3** | D2 外部生命周期状态感知 | 省 syscall | ~50 行 | 厂商探测 |
| **P3-实验** | sched_switch 迁移观察（默认关闭） | P5 补完 | ~300 行 | A 主线后 |
| **评估** | B4 空闲核启发式 / C4 tracing | 边际 | — | P7 末 |

---

## 六、里程碑

| 阶段 | 内容 | 交付 |
|---|---|---|
| P7.1 | **EventSource trait 提取（ARCH-1）→ 构建链验证（IMPL-1）→ eBPF fork/exec/exit 事件源（ARCH-2 分流 + IMPL-4 线程退出）+ 降级链** | 事件发现延迟降低（near-real-time） |
| P7.2 | 自适应 relock + cpuset 归属对抗（B1+D3，共享 RelockGuard） | AMS 覆盖收敛（防震荡） |
| P7.3 | IPC CLI（status/dump/reload/apply，mpsc 架构）+ dry-run（来源树输出） | 可观测/可控制 |
| P7.4 | DecisionEngine 强化（foreground 三源/thermal 趋势）+ 屏幕状态（B3+D1） | 决策质量 |
| P7.5 | sched_switch 迁移观察（实验性，默认关闭）+ 外部生命周期感知（D2）+ 收尾 | P5 补完 |

---

## 七、风险与开放问题

1. **eBPF 内核限制**：Android GKI 内核通常允许 BPF 但可能禁 ringbuf/特定 tracepoint——
   加载失败自动回退 proc（已有模式），无阻塞
2. **构建链**（IMPL-1）：bpf-linker/LLVM 在 Termux 安装路径不标准 + 目标设备 BTF
   支持差异——P7.1 第 0 步前置验证，验证不通过则 eBPF 延后（IPC 提前预案）
3. **自适应 relock 阈值**：覆盖率检测的窗口/阈值需真机校准（B1）
4. **IPC 安全**：Unix socket 权限（root-only 0750）+ 命令白名单——不做任意命令执行
5. **屏幕状态接口差异**：ROM 差异大，多路径探测失败 = 常亮处理（保守）
6. **不替代调度器**：B4 空闲核启发式若与 EAS 冲突即放弃（评估项）

---

## 八、本次不做的（明确范围外）

- ❌ 内核态 cpuset/affinity 写入（eBPF 只发事件）
- ❌ 替代 ProcSource（proc 为降级路径）
- ❌ tracing 日志系统（保持 println，release 大小优先）
- ❌ 任何新策略语义（cluster/cpus/sched 语法不变）
- ❌ 调度器替代 / 冻结 / 杀进程（non-goals 延续）
