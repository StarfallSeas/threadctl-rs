全部代码与文档逐行读完。以下是最终报告。

---

# P7.0-P7.1 审查报告

> 审查范围：`crates/ebpf/src/main.rs`（内核态） · `crates/daemon/src/ebpf_source.rs` · `crates/daemon/src/main.rs` · `crates/core/src/event.rs` · `proc.rs::read_tgid` · `engine.rs` Exit 路径 · `tracker.rs::remove_tid` · 全部 P7 文档  
> P7 规划书（v2 定稿）采纳确认 · 构建链 ✅ · 真机 ✅  
> 本轮新发现：9 项

---

## 一、P7.1 架构核查（ARCH-1~4 全部落地）

| 规划缺口 | 落地状态 | 代码位置 |
|---|---|---|
| ARCH-1 `EventSource` trait | ✅ | `event.rs:75-86`，`ProcSource`/`EbpfSource` 各自实现 |
| ARCH-2 fork/thread clone 分流 | ✅ | 用户态 `flush_pending` 读 `Tgid`，非内核态 |
| ARCH-3 RelockGuard | ⏳ P7.2 | 规划书标注，未实现，正确 |
| IMPL-1 构建链前置验证 | ✅ | 文档确认，`bpf-linker 0.10.4 + lld 21 + aya 0.14/aya-ebpf 0.2.1` |
| IMPL-2 IPC mpsc 架构 | ⏳ P7.3 | 规划书标注，正确 |
| IMPL-4 `sched_process_exit` 线程退出 | ✅ | `tracker::remove_tid()` + engine Exit 路径 `tid != pid` 分支 |

---

## 二、Bug

### 🔴 BUG-H1 `ebpf_source.rs` — `TARGET_COMM_MAP` 容量固定 64，无法运行时扩容，注释误导

这是 P7.1 白名单机制的核心缺陷。

**内核端**注释声称：
```rust
/// 白名单键容量（用户态 EbpfLoader::set_max_entries 按包名数动态覆盖）。
const MAP_CAPACITY: u32 = 512;
```

**用户端**实际：
```rust
loader.map_max_entries("TARGET_COMM_MAP", 64);  // ← 硬编码 64，覆盖了内核的 512
```

**BPF HashMap 容量在加载时固定，不能运行时扩容。** `update_whitelist` 注释"按包名数重建"仅重建了*内容*，容量依然是 64。

`target_entries()` 每个包名最多产生 2 条键（前 8 字节 + 末 8 字节，排重后）：

- 32 个包 → 64 条键 → 恰好装满
- 33 个包 → 需要 65+ 条键 → `target.insert()` 静默失败（BPF HashMap 满返回 error，代码 `let _ = target.insert(...)` 丢弃错误）

超出容量的包名进程的 fork/exec 事件在内核态被白名单过滤丢弃，退化为 2s 全扫兜底——与 eBPF 前无区别，且**无任何日志告警**。

**修复方案**：`try_new()` 接收初始 `cfg: &ConfigSnapshot`，按实际包数设容量：
```rust
pub fn try_new(tracker: Arc<Mutex<StateTracker>>, cfg: &ConfigSnapshot) -> Result<Self, String> {
    let pkg_count = cfg.rules.pkgs().len();
    let cap = ((pkg_count * 2) as u32).next_power_of_two().max(64);
    loader.map_max_entries("TARGET_COMM_MAP", cap);
    ...
}
```

同时把内核端的 `MAP_CAPACITY` 改为足够大的默认值（如 1024），由用户端 `map_max_entries` 覆盖——与注释保持一致。

---

### 🟠 BUG-M1 `ebpf_source.rs::handle_raw` — FORK 事件入 pending 无去重，内核允许 2 事件/0.1s 导致重复 Fork

内核防抖（`DEDUP_MAX_COUNT = 2`）允许同一 pid 在 0.1s 窗口内产生 2 个事件（第 1、2 个通过，第 3+ 丢弃）。这意味着一次进程 fork 可以产生 2 个 EVENT_FORK 进入 ringbuf。

`handle_raw` 的处理：
```rust
EVENT_FORK => {
    if self.pending.len() < PENDING_MAX_PENDING {
        self.pending.push(PendingFork { child_pid: ev.pid, ... });
        // ← 没有检查 child_pid 是否已在 pending 中
    }
}
```

同一 child_pid 进入 pending 两次。`flush_pending` 成功后产生两个 `ProcessEvent::Fork(pid, pid)`，engine 重复处理（第二次 `tracker.enter` 发现已有状态，进行一次多余的 `refresh_process_rules`）。

`ProcSource` 的同类 bug 已在之前审查中修复（加了 `!self.pending.iter().any(|p| p.pid == pid)` 去重检查），但该修复未移植到 `EbpfSource`。

**修复**：
```rust
EVENT_FORK => {
    if self.pending.len() < PENDING_MAX_PENDING
        && !self.pending.iter().any(|p| p.child_pid == ev.pid) {
        self.pending.push(PendingFork { child_pid: ev.pid, retry: 0, next: Instant::now() });
    }
}
```

---

### 🟠 BUG-M2 `ebpf_source.rs::poll` — 有 pending 重试项时仍休眠到 deadline，退避延迟被膨胀

```rust
fn poll(&mut self, deadline: Instant) -> Vec<ProcessEvent> {
    let mut events = Vec::new();
    self.drain(&mut events);      // drain 包含 flush_pending
    if events.is_empty() {
        let now = Instant::now();
        if now < deadline {
            std::thread::sleep(deadline - now);  // 休眠最长 scan_interval（默认 2s）
        }
    }
    events
}
```

`flush_pending` 将 100ms 退避的 pending 项保留（`next = now + 100ms`）。poll 看到 events 为空，休眠到 deadline（最多 2s 后）。下次 poll 开始时，这些 pending 项的 `next` 早已过期——100ms 的设计退避实际变成 **100ms + 最多 2s = 最差 2.1s**。

这对 Zygote 启动（最常见场景）的影响：app fork 后 100ms cmdline 就绪，但实际等待 ~2s 才得到策略应用，与 ProcSource 无差别。

**修复建议**：计算 pending 最近一项的 `next` 时间，用 `min(deadline, min_next_pending)` 作为 sleep 上限：

```rust
let wake = if let Some(min_next) = self.pending.iter().map(|p| p.next).min() {
    deadline.min(min_next)
} else {
    deadline
};
std::thread::sleep(wake.saturating_duration_since(Instant::now()));
```

---

### 🟠 BUG-M3 `ebpf/src/main.rs` — EXIT 全量上报对高线程数 Android 应用可能溢出 256KB ringbuf

设计决策：EXIT 不走白名单过滤（因为线程 comm 不是包名，用白名单会漏线程退出事件）。但 Android 重负载下线程退出极为频繁：

- Binder 线程池：每个服务常驻 16 条
- Unity 游戏：运行时线程 30-60 条
- 每次线程被 AMS 调整可能触发批量线程退出

`EVENTS` ringbuf 为 256KB，每个 `ProcEvent` 结构体 32 字节 → 容量 8192 个事件。对于有 100+ 线程频繁退出的应用，1 秒内可能产生数千 EXIT 事件。Reader 线程每 50ms 读一批，主循环每 2s drain 一次，积压事件极可能溢出 ringbuf → 内核丢弃事件 → `applied_tids` 清理不及时（但有全扫兜底）。

**短期缓解**：将 ringbuf 扩至 1MB（`256 * 1024` → `1024 * 1024`），减少溢出概率。  
**根本修复**（P7.2 规划 D3 时一并考虑）：在内核态为 EXIT 事件添加基于 tgid 的粗过滤——维护第二张 `TRACKED_TGID_MAP: HashMap<i32, u32>`，用户态在 Fork 事件确认后插入 tgid，Exit 时查询该 map，未跟踪的 tgid 在内核态丢弃。

---

## 三、代码质量问题

### 🟡 LOW-1 `update_whitelist` 调用 `target_entries()` 两次

```rust
for key in Self::target_entries(cfg.rules.pkgs()) {   // 调用 1：插入
    let _ = target.insert(key, 1, 0);
}
println!("ebpf whitelist: {} entries ({} pkgs)",
    Self::target_entries(cfg.rules.pkgs()).len(),      // 调用 2：仅取 len
    cfg.rules.pkgs().len());
```

`target_entries()` 内部做 Vec 分配、排序、去重，调两次是纯浪费。

**修复**：
```rust
let entries = Self::target_entries(cfg.rules.pkgs());
let n = entries.len();
for key in entries { let _ = target.insert(key, 1, 0); }
println!("ebpf whitelist: {n} entries ({} pkgs)", cfg.rules.pkgs().len());
```

---

### 🟡 LOW-2 内核态 `ProcEvent.pid` 字段注释对 EXIT 事件描述错误

```rust
/// FORK: child_pid；EXEC: tgid；EXIT: 退出任务 pid（线程退出=pid==tid）
pub pid: i32,
```

EXIT 实际代码：
```rust
let tgid = (pid_tgid >> 32) as i32;  // 进程 PID
submit_event(tgid, tid, 0, comm, EVENT_EXIT);
//           ^^^^ = pid 字段
```

EXIT 的 `pid` 是 **tgid（进程 PID）**，不是退出任务的 pid。"`线程退出=pid==tid`" 说法也错误——线程退出时 `pid(=tgid) != tid`，主线程退出时 `tgid == tid`，代表进程退出。

用户态 `EbpfProcEvent.pid` 注释是正确的（"EXIT: tgid（进程 pid）"）。内核端注释需对齐。

---

### 🟡 LOW-3 `ebpf_source.rs` 文件头注释误称 `.bpf.o`

```
//! Loads `threadctl-ebpf` .bpf.o via aya
```

实际加载的是 `bpfel-unknown-none` 目标编译出的 **ELF 二进制**，不是 `.bpf.o` 对象文件（.bpf.o 是未链接的中间产物）。`aya::EbpfLoader::load()` 接收的是完整 ELF。

---

### 🟡 LOW-4 P7.1 新代码无单测，73 项测试未增加

新增功能应有单测但未覆盖：

- `tracker::remove_tid()`：纯逻辑，无 syscall，完全可测
- `EbpfSource::target_entries()`：纯函数，可测短包名/长包名/去重逻辑
- `EbpfSource::flush_pending()`：分流逻辑（Tgid==Pid vs !=Pid）可通过 mock 测试

其中 `remove_tid` 最重要——IMPL-4 的核心逻辑，行为应被测试覆盖。

---

### 🟡 LOW-5 `docs/repo-overview.md` 未更新反映 P7.1

- topology.rs 行数：384（应为 ~500）
- 单测：73（若 P7.1 加了测试则更新；目前确实是 73，但文档说明应提及 P7.1）
- `policy.rs` 字段描述未含 `uclamp_min/uclamp_max`（P6.2 后遗留）
- 未提及 `event.rs`（新增 `EventSource` trait）、`ebpf_source.rs`、`read_tgid()`

---

## 四、架构质量

### EXIT 事件每条都锁 tracker — 高频路径 mutex 热点

```rust
EVENT_EXIT => {
    let tracked = self.tracker.lock()         // ← 每个 EXIT 事件都锁
        .unwrap_or_else(|e| e.into_inner())
        .contains(ev.pid);
```

EXIT 全量上报 → drain() 批量处理 → 每条获取/释放 mutex。在重负载下 drain 每轮处理 256 条事件，其中大量是非跟踪进程的 EXIT，全部白跑一次 mutex 加锁。

**P7.2 BUG-M3 的根本修复**（内核 tgid 过滤）同时解决此问题。短期接受，标注为已知 hotspot。

---

## 五、正确实现确认

以下是审查过程中验证无误的关键点：

| 项 | 结论 |
|---|---|
| `comm_matches` 边界（while pos <= 8，访问 comm[pos+7] 最大 comm[15]）| ✅ 无越界 |
| `should_dedup` 防抖逻辑（首事件 else/reset → 第 1、2 事件通过 → 第 3 事件丢弃）| ✅ 语义正确 |
| `ProcEvent` / `EbpfProcEvent` 内存布局（4+4+4+16+4=32 字节，无 padding）| ✅ 两端一致 |
| EXIT 路径：tgid==tid → 进程退出 `tracker.remove()`；tgid!=tid → 线程退出 `remove_tid()` | ✅ 语义正确 |
| `bpf_get_current_pid_tgid()` 在 `sched_process_exit` 中返回退出任务的 tgid/tid | ✅ current 是退出任务 |
| reader 线程：`_bpf` drop → tracepoint detach → rx drop → `tx.send()` Err → 线程退出 | ✅ 无泄漏 |
| `read_unaligned` 读取 ringbuf 条目（ringbuf 条目对齐不保证）| ✅ 正确防御 |
| `EVENTS.reserve(0)` 第二参数为 flags，`0` 正确 | ✅ |
| `dedup_key` for FORK = `child_pid as u32`（防抖按 child pid，非 parent）| ✅ 语义正确 |
| ebpf 路径 `current_exe().parent().join("threadctl-ebpf")` 加载 ELF | ✅ 正确 |

---

## 六、修复优先级

| 优先级 | 项 | 修复代价 |
|---|---|---|
| **P7.1 补丁** | BUG-H1 MAP 容量：`try_new` 接收 cfg，按包数计算 cap | ~10 行 |
| **P7.1 补丁** | BUG-M1 pending 去重：加 `any(|p| p.child_pid == ev.pid)` | 3 行 |
| **P7.2 同期** | BUG-M2 poll 唤醒：按 pending min-next 计算 sleep | ~10 行 |
| **P7.2 同期** | BUG-M3 ringbuf 溢出：扩容 + 内核 tgid 过滤（规划 D3） | 中等 |
| **随时** | LOW-1 双调用 target_entries | 3 行 |
| **随时** | LOW-2 内核注释修正 | 1 行 |
| **随时** | LOW-3 文件头注释 .bpf.o → ELF | 1 行 |
| **P7.2** | LOW-4 remove_tid + target_entries + flush_pending 单测 | ~50 行 |
| **P7.2** | LOW-5 repo-overview.md 同步 | 文档 |