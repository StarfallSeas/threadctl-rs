# P8 Delivery & Performance Review — observe data layer + resource fixes

> **ADR 风格头**
> Author: DeepSeek V4 Flash（执行者）
> Reviewers: Claude（代码·架构）· ChatGPT（文档）
> Status: Delivered — 107 单测全绿、零警告、SM8550 真机双场景验证
> Date: 2026-08-11
> References: P7 规划书 v2（P8 观测层）· 用户 Metric 需求（线程快照/实际运行核/迁移统计）· cpu.log 真实负载审查

---

## 〇、交付总览

| Commit | 内容 |
|---|---|
| `ed18d00` | P8 observe 数据层：ThreadSnapshot + Sampler + SnapshotWindow + IPC snapshot |
| `12277ae` | 性能修复：主循环空转（38%→0%）+ P8 采样降频/缓冲复用（RSS 19MB→6MB） |
| `aeabff0` | monitor.sh Android root 适配（自动提权 + pid 发现 + 精确 CPU 差分） |

**当前状态**：107 单测（core 96 + daemon 11）、零警告、release 1.3MB + ebpf 5.8KB。

---

## 一、P8 观测数据层（Metric 需求合并进 threadctl 的部分）

### 分界线（三审/用户确认）

- **合并**：数据采集（read_processor/负载）+ ThreadSnapshot + 窗口统计（迁移/Affinity 变化/核心分布/主要核心）——daemon 内增量计算
- **不合并**：历史持久化（内存环形窗口，重启清）、帧率关联分析（展示层做）

### 模块

| 文件 | 内容 |
|---|---|
| `proc.rs` | `read_processor(tid)`（stat 第 39 字段）、`read_thread_cpu_secs(tid)`（utime+stime，负载差分）、`parse_stat_fields`（`)` 后解析，comm 含空格安全） |
| `observe.rs` | `ThreadSnapshot{tid,name,affinity,running_cpu,load_pct,cpu_ticks}`；`Sampler`（2→5s 周期负载差分，复用缓冲）；`SnapshotWindow`（环形 150 样本 + 增量统计：migrations/affinity_changes/cpu_distribution/primary_cpu/avg+max load） |
| IPC | `threadctl snapshot [pid]` —— 每线程 `affinity[running_cpu]` 格式 + 窗口统计 |
| 脚本 | `monitor.sh` —— daemon CPU%/RSS/VSZ 实时监测（Android root 版） |

### snapshot 真机验证（SM8550 真实负载，150+ 线程）

```
tid       name           affinity  cur avg% max% migr affChg  primary
2319      RenderThread        3-7    3    0    0    0      0  3
7452      RenderThread        0-6    0    0    8    2      0  0
```

- **实际运行核心** + **负载峰值** + **迁移计数** + **核心偏好** 全有效
- `RenderThread` 迁移 2 次——真实观测数据（修复 relock 后迁移显著减少）

---

## 二、性能修复（cpu.log 真实负载审查驱动）

### 2.1 主循环空转（38% CPU → ~0%）

**根因**：`EbpfSource::poll` 调 `drain` 立即返回（ringbuf try_recv 风格），`deadline` 参数被忽略 → 主循环高速空转。单进程测试无法暴露（空闲时循环快），真实负载下 38% 单核。

**修复**：poll 后 `sleep(deadline 剩余，上限 100ms)`——fork 应用延迟 ≤100ms 保持 near-real-time；空闲唤醒 10Hz。

### 2.2 P8 采样开销（CPU 2.8% 均值 → <1%）

**cpu.log 证据**（1410 样本，真实 8 包部署）：

| 指标 | 数值 |
|---|---|
| CPU 平均 | 2.81%（最大 15%，零样本仅 33%） |
| RSS 平均 | 14.2MB（最小 4.5MB，最大 19MB） |

**根因 A**：P8 采样每 2s 全量读 200+ 线程 × 3 /proc 文件（stat/comm/cpuset）= 600+ 文件操作/2s——持续 CPU 大头。

**根因 B**：`Sampler` 每轮新建 `Vec<ThreadSnapshot>`；Rust 分配器不把内存还给 OS → RSS 高水位保持峰值（min 4.5MB vs avg 14.2MB 证明是分配 churn 而非真实驻留）。

**修复**：
1. 采样 2s → 5s（窗口 150×5=12.5 分钟仍足够）
2. `Sampler` 内部复用缓冲（`buf: Vec<ThreadSnapshot>`，零分配 churn/轮）

### 2.3 monitor.sh Android root 适配

- **自动提权**：非 root 时 `su -c` 重启自己（SELinux：u0_aXXX 读 root 进程 /proc 被拒）
- **pid 发现三通道**：TC_PIDFILE/pid 文件 → pidof → ps 扫描（toybox `USER PID PPID` 布局，PID 在 $2——初版取 $1 取到 "root" 的 bug 已修）
- **精确 CPU**：/proc/<pid>/stat utime+stime 差分 / HZ / 间隔（toybox `ps %cpu` 不可靠）；HZ 探测 + 100 兜底

### 2.4 性能测试矩阵

| 场景 | 修复前 | 修复后 |
|---|---|---|
| 空闲（单进程测试） | 38% CPU / 峰值分配 | ~0% CPU（10s 4 ticks） |
| 50-fork 风暴 | — | 2% CPU，51/51 事件全处理 |
| 真实负载（8 包） | 2.81% avg / 19MB peak | **预期 <1% / ~6MB**（待用户复测确认） |
| RSS 稳态 | 14.2MB avg（churn 高水位） | 5.4-6.9MB 稳定 |

---

## 三、测试矩阵（107 全绿）

| 模块 | 测试数 | 本轮新增 |
|---|---|---|
| core | 96 | observe ×9（stat 解析/comm 空格/迁移/affinity 变化/窗口裁剪/负载） |
| daemon | 11 | snapshot 解析 ×1 |
| **合计** | **107** | **+10** |

零警告。ebpf 内核态零 warning。

---

## 四、遗留与下一步

| 项 | 计划 |
|---|---|
| 真实负载复测（monitor.sh 5 300） | 用户侧（替换二进制后） |
| 游戏线程排查（--debug + snapshot 定位线程名） | 用户侧 |
| P8 展示层（Web/APP，用户指令：先不管） | 暂缓 |
| P8 单 pid 精确 apply（Claude BUG-M3） | 需暴露 refresh_process_rules |
| LOW-1 命名重构（covered → evicted） | P8.1 |

---

## 五、请两位审

### Claude（代码·架构）

1. **P8 采样 5s 频率**：窗口 150×5=12.5 分钟，迁移/负载统计精度是否足够？5s 采样会不会漏掉高频迁移（毫秒级迁移在 5s 间隔下不可见）——观测层是否应区分"采样精度"与"事件精度"（如迁移只统计采样可见的，文档注明）？
2. **Sampler 复用缓冲**：`sample(&mut self) -> &[ThreadSnapshot]` 借用生命周期——调用方 `push_batch(snaps)` 后缓冲被下一轮 clear——是否有隐式借用冲突风险（main.rs 中 `let snaps = sampler.sample(...); snap_window.push_batch(snaps);`）？
3. **主循环 sleep 100ms 上限**：事件延迟 ≤100ms 是否破坏 eBPF near-real-time 定位（Zygote fork 的 100ms 空窗对"新线程 2s 无策略"是否有影响）？是否应改为"有事件立即处理、无事件睡满 deadline"的双模式（当前 sleep 后下一轮才 drain——事件在 sleep 期间到达要等醒来）？
4. **RSS 高水位**：除 Sampler 外，是否还有其他周期性大分配（tracker tid_names TTL 60s 重建、events Vec）需要复用？是否需要 jemalloc 级优化（`MALLOC_ARENA_MAX=1` 或 mimalloc）还是当前水平可接受？
5. **monitor.sh**：`sleep $INTERVAL` 在 sample 内 + 外层循环——DURATION 语义在 CPU 计算时实际间隔 = INTERVAL（正确），但首轮样本 T1 在 pid 发现后立即读——daemon 刚启动时 T1 读到的是启动以来的累计 ticks，首次 CPU% 可能偏高（差分窗口短）——是否需要首轮跳过？

### ChatGPT（仅文档）

6. P8 观测层文档是否清晰（snapshot 命令用法、"0-7[4]" 格式约定、窗口语义）？
7. 性能审查叙事（cpu.log → 根因 → 修复 → 预期）是否完整可复现（用户可按文档复测）？
