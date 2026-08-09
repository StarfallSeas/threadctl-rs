两份文档读完。以下是我的独立审查意见。

---

# P7 规划审查报告

> 审查对象：P7 规划书（DeepSeek V4 Flash）+ ChatGPT 5.5 Mini 审查意见  
> 角色：架构 · 代码  
> 本报告：新增发现 + 对 ChatGPT 意见的评价

---

## 一、整体判断

P7 方向正确，non-goals 边界坚守得好。有 3 项架构前置缺口（不解决 P7.1 无法启动），ChatGPT 审查有 2 处判断需要修正。

---

## 二、新发现：架构缺口（P7.1 前置）

### 🔴 ARCH-1 `EventSource` trait 尚不存在，P7.1 有隐性重构前提

规划书说"零架构改动，复用现有管道"——但现有代码里 `ProcSource` 的 `collect()` 是自由方法，不是 trait 实现：

```rust
// 现状（daemon/src/proc_source.rs）
impl ProcSource {
    pub fn collect(&mut self, ...) -> Vec<ProcessEvent> { ... }
}
```

没有 `EventSource` trait，也没有 `Box<dyn EventSource>` 的注入点。eBPF 事件源要能与 `ProcSource` 互为降级，需要先完成：

```rust
trait EventSource {
    fn collect(&mut self, rules: &ConfigSnapshot, events: &mut Vec<ProcessEvent>);
}
impl EventSource for ProcSource { ... }
impl EventSource for EbpfSource { ... }
```

再把 `main.rs` 的 `let mut source = ProcSource::new(...)` 改为 `Box<dyn EventSource>`。这是 **P7.1 的显性前置任务**，工作量约 50-80 行，但影响 daemon 主循环结构，需要明确写进 P7.1 milestone。

---

### 🔴 ARCH-2 eBPF `sched_process_fork` 同时捕获进程 fork 与线程 clone，需显式分流

Linux `sched_process_fork` tracepoint 在 `copy_process()` 完成时触发，无论是：
- `fork()`/`clone()` 创建新进程 → 需要 Fork 事件
- `pthread_create()` 内部的 `clone(CLONE_VM)` → 需要 ThreadClone 事件

当前 `ProcessEvent` 区分 `Fork` 和 `ThreadClone`，区分依据是 `pid == tid`（线程克隆时 `tgid != pid`）。eBPF 需在内核态通过 `task->tgid != task->pid` 做分流，发不同事件，否则所有 clone 都以 Fork 进入 engine，`apply_single_tid` 路径会被跳过，导致新线程规则不生效——而这正是 eBPF 相对 proc 轮询最大的改善点（2s 新线程延迟）。

规划书提到"Fork/Exec/Exit 三个 tracepoint"，但完全没有提及线程 clone 检测，是一个叙述遗漏。

---

### 🟠 ARCH-3 B1（自适应 relock）与 D3（cpuset 归属对抗）的双触发竞争

规划书把 B1 和 D3 分列但描述为"互补"，实际存在交互问题：

```
D3 检测到 cpuset 归属被 AMS 改变
→ 立即触发 relock（D3，事件驱动）
→ 同时 B1 检测到 downgraded 率上升
→ 缩短周期到 3s
→ D3 刚 relock 完 2s，B1 又 relock
```

两者需要共享一个"最近一次 relock 的时间戳"并遵守 cooldown，否则在 AMS 对抗激烈时，每次覆盖都会触发 2 次 relock（一次即时 + 一次周期）。ChatGPT 提到 B1 需要防震荡，但没有指出 B1+D3 的具体交互问题。建议在 B1/D3 共同设计一个 `RelockGuard{ last_at, cooldown_ms }` 状态，所有 relock 入口检查。

---

## 三、中等优先级新发现

### 🟠 IMPL-1 eBPF 构建链复杂度被严重低估

规划书预估工作量"~400 行内核态 + ~200 行用户态"，但 eBPF 的主要成本在构建系统，不在代码行数：

- `aya-ebpf` 需要独立的 `bpf` 编译目标（`--target bpfel-unknown-none`），需要安装 `bpf-linker`
- `bpf-linker` 依赖特定 LLVM 版本，在 Termux 环境里安装路径不标准
- 内核态代码需要目标设备的 BTF（BPF Type Format）支持，即 `/sys/kernel/btf/vmlinux`
- SM8550（内核 5.15）有 BTF，但 SM8650/SM8750 需要独立验证

当前的 `Cargo.toml` workspace 和 `threadctl-ebpf` 空壳 crate 对这些构建依赖完全空白。P7.1 milestone 至少需要专门一个"构建链验证"步骤，在写第一行 eBPF 代码之前确认 Termux 能产出有效的 `.bpf.o`。

---

### 🟠 IMPL-2 IPC 监听线程的主循环集成方式未设计

规划书说"daemon 侧监听线程"，但现有 `main.rs` 的主循环是单线程阻塞 `poll`：

```rust
loop {
    let events = source.collect(...);
    handle_events(...);
    // relock / cleanup / foreground refresh
}
```

IPC 请求（如 `threadctl apply <pid>`）需要修改 `tracker` 状态，这是主循环持有的可变状态。若 IPC 在独立线程，需要通过 channel 把请求发回主循环执行，增加 `Arc<Mutex<>>` 或 `mpsc::channel`。若主循环改为 epoll（同时监听 ProcSource fd + ipc socket fd），代码改动更大。

这是 C1 IPC 的**架构决策**，规划书没有指定，需要在 P7.3 milestone 前明确。建议：mpsc channel 方案更简单（独立线程处理 socket I/O → 发命令 → 主循环执行），与 hot-reload 线程的现有模式一致。

---

### 🟡 IMPL-3 "2s → 亚毫秒"表述仅针对事件发现，不是端到端策略生效

即使 eBPF ringbuf 在内核事件发生后 <1ms 通知用户态，端到端延迟包括：

| 阶段 | 延迟 |
|---|---|
| ringbuf → 用户态 poll | 1-10ms（取决于消费线程 poll 间隔）|
| read_cmdline（Zygote pending 路径）| 100-1000ms（pending 退避逻辑）|
| engine::handle_events | 1-5ms |
| apply_thread syscalls × N 线程 | 1-5ms/线程 |

规划书"事件延迟 2s → 亚毫秒"的表述应明确为**事件发现延迟**，不是策略生效延迟。对于 Zygote fork 场景（最常见），pending 队列决定的延迟（100-1400ms）远大于 eBPF 节省的 2s。eBPF 对**非 Zygote 进程**（如直接 exec 的 native daemon）延迟改善最明显。ChatGPT 也指出了这一点，我完全同意。

---

### 🟡 IMPL-4 线程 exit 事件未提及，但这是修复 TID 复用 bug 的机会

`sched_process_exit` 在进程内的**线程退出时同样触发**（Linux 任务退出均走同一路径）。这提供了一个此前无法处理的能力：即时感知线程退出，从 `applied_tids` 移除对应 TID。这能彻底修复之前审查报告中发现的 TID 复用 bug（线程 A 退出 → 新线程 B 复用同一 TID → `applied_tids` 仍含旧 TID → 线程 B 被漏掉，等 2s 全扫收敛）。

规划书没有提到这个价值，建议在 A2 设计节补充"sched_process_exit 兼处理线程退出 → 即时更新 applied_tids，消除 TID 复用窗口"。

---

## 四、对 ChatGPT 审查意见的评价

### ✅ 同意的部分

| ChatGPT 意见 | 评价 |
|---|---|
| sched_switch → P7.5 实验性 | 正确。sched_switch 在高负载 Android 上 >100K events/s，ringbuf 和消费线程压力都是真实风险 |
| adaptive relock 需防震荡 cooldown | 正确，但我补充了 B1+D3 的具体交互问题（见 ARCH-3） |
| dry-run 高价值低成本 | 完全同意，且输出格式建议（`com.tencent.mm → exact+wildcard+final`）很好，应该采纳 |
| eBPF 性能宣传降调 | 正确，见 IMPL-3 |
| D1 屏幕状态→通过 DecisionEngine 接入，不直接改策略 | 正确，保持 DecisionEngine 作为唯一策略门控 |
| D2 改名为"外部生命周期状态感知" | 好建议，避免语义污染（threadctl 不冻结） |
| DecisionEngine 保持 gate 语义，不进优化器 | 核心原则，必须守住 |

### ❌ 不同意的部分

**ChatGPT 意见 6：IPC CLI 可能比 eBPF 更影响用户体验，建议提升至 P7.1**

这个判断有误。对 threadctl 的定位（task policy daemon）：

- **eBPF** 解决的是**策略正确性**问题：前 2s 内新线程无策略，在高频线程创建场景（游戏引擎、音频服务）这是功能性缺陷，不是体验问题
- **IPC CLI** 解决的是**可调试性**问题：`threadctl status` 是锦上添花，`threadctl` 不提供这个命令，功能依然正确

eBPF 应保持 P0，IPC 在 P7.3（按规划书原设计）是正确排序。把 IPC 提到 P7.1 会让 P7.1 milestone 变成两个独立方向的并行交付，增加协调成本。

**ChatGPT 意见 11：建议拆 P7/P8**

当前 P7 规划的 P7.1-P7.5 milestone 链已经是分阶段交付设计，每个 milestone 独立可交付。强行拆 P7/P8 会增加版本号语义的讨论成本，价值不大。维持 P7.1-P7.5 的现有结构，每个 milestone 单独 review 即可。

---

## 五、规划书细节修正

**A2 设计节需补充：**

1. eBPF 事件需区分进程 fork（`tgid == pid`）和线程 clone（`tgid != pid`），分别映射到 `ProcessEvent::Fork` 和 `ProcessEvent::ThreadClone`
2. `sched_process_exit` 用于线程退出 → 即时移除 `applied_tids` TID，消除 TID 复用窗口
3. 添加"构建链前置验证"为 P7.1 第一步（BTF 支持确认 + `bpf-linker` 在 Termux 安装验证）

**A3 关键点补充：**

4. `EventSource` trait 提取是 P7.1 的显性依赖，需列入 milestone

**B1 设计节补充：**

5. B1 与 D3 共享 `RelockGuard{ last_at, cooldown_ms }`，所有 relock 入口（周期 + 即时）统一检查 cooldown，防止 B1+D3 双触发
6. B1 自适应策略的调整速率：仅在确认**持续覆盖**（观察窗口 ≥ 30s）后才缩短周期，单次失败不触发

**C1 设计节补充：**

7. 明确 IPC 监听线程架构：独立线程 + `mpsc::channel` → 主循环执行（与 hot-reload 线程模式一致）

---

## 六、总结

| 维度 | 判断 |
|---|---|
| P7 整体方向 | ✅ 批准，边界清晰 |
| A. eBPF 主线 | ✅ 但需补 ARCH-1（trait）和 ARCH-2（thread clone）|
| B1 自适应 relock | ✅ 但需加 B1+D3 cooldown 协调（ARCH-3）|
| B2 DVFS 提示 | ✅ 定位正确（logging only） |
| B3 DecisionEngine | ✅ P6.4 BUG-M1 在此落地 |
| C1 IPC CLI | ✅ P7.3 位置合理，不提前 |
| C2 dry-run | ✅ 采纳 ChatGPT 的输出格式建议 |
| D1 屏幕状态 | ✅ 经 DecisionEngine 接入 |
| D2 冻结感知 | ✅ 采纳 ChatGPT 改名建议 |
| sched_switch | ✅ P7.5 实验性（ChatGPT 正确）|
| eBPF 性能表述 | ⚠️ 改为"事件发现延迟降低"，去掉"亚毫秒" |
| IPC 优先级 | ❌ 不同意 ChatGPT 提升至 P7.1 |
| P7/P8 拆分 | ❌ 不同意 ChatGPT 的拆分建议 |