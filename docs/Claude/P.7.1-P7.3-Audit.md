全部代码 + P7.2/P7.3 文档逐行读完。深度审查模式结果如下。

---

# P7.2 + P7.3 深度审查报告

> 新增文件：`relock.rs` · `debug.rs` · `ipc.rs` · 更新 `ebpf_source.rs` · `main.rs`  
> 测试：98 项确认  
> **本轮发现 1 个高严重度逻辑 bug + 7 项中低问题**

---

## 一、高严重度 Bug

### 🔴 BUG-H1 `relock.rs::observe_ratio` — 单阈值同时控制"收缩信号"与"稳定判定"，导致 AMS 持续低强度覆盖（<50%）时 relock 周期反向延长到 300s

这是 B1 自适应算法的核心设计缺陷。

**问题根源：** `observe_ratio(ratio)` 将浮点 ratio 通过单个阈值 `>= 0.5` 转成 bool，然后同一个 bool 既用于"是否达到缩短门槛"，又用于"窗口内是否有覆盖（决定是否累计稳定时长）"。

```rust
pub fn observe_ratio(&mut self, ratio: f64) {
    self.observe(ratio >= COVER_RATIO_SHRINK);  // 0.3 → false
}

fn observe(&mut self, covered: bool) {
    // ...
    if ratio >= COVER_RATIO_SHRINK { /* 缩短 */ }
    else if covered_n == 0 {          // ← 0.3 的 false 样本使 covered_n = 0
        stable_secs += SAMPLE_INTERVAL_SECS;  // ← 错误累积稳定时长!
        if stable_secs >= STABLE_SECS_EXTEND { rank += 1; }  // ← 延长到 300s!
    }
}
```

**仿真验证（完整输出）：**

```
AMS 持续覆盖 30% 进程（ratio=0.3，每 2s 一次）：

sample 15:  rank=2 (60s)   stable_secs=2
sample 30:  rank=2 (60s)   stable_secs=32
sample 45:  rank=3 (300s)  stable_secs=2   ← 90s 后 relock 间隔变成 300s！
```

**后果：** AMS 持续把 30% 进程移出我们的 cpuset，threadctl 把 relock 周期从 60s 延长到 300s，相当于主动降低对抗强度。这与设计目标完全相反。

实际 Android 场景下，AMS 覆盖率经常在 20%-40% 之间（部分系统进程被允许移出），触发这个 bug 的条件非常普通。

**根本原因：** 变量命名混淆了两个概念：
- `sample_coverage` 里的 `covered` = "被 AMS 移出"（不好的事）
- `observe(covered)` 的 bool = "是否检测到覆盖"（两处语义等价，但与"稳定"语义相反）

"稳定"应该定义为**当前采样 ratio == 0.0**，而不是"窗口内 covered_n == 0"（后者因为 ratio=0.3 映射到 false 而被错误满足）。

**修复：**

```rust
pub fn observe_ratio(&mut self, ratio: f64) {
    // 收缩信号：需要多数进程被移出（>= COVER_RATIO_SHRINK）
    let majority_covered = ratio >= COVER_RATIO_SHRINK;
    self.samples.push_back(majority_covered);
    if self.samples.len() > WINDOW_SAMPLES { self.samples.pop_front(); }
    if self.samples.len() < WINDOW_SAMPLES { return; }

    let covered_n = self.samples.iter().filter(|&&c| c).count();
    let window_ratio = covered_n as f64 / self.samples.len() as f64;

    if window_ratio >= COVER_RATIO_SHRINK {
        self.stable_secs = 0;
        self.rank = self.rank.saturating_sub(1);
    } else if ratio == 0.0 {
        // ← 关键修复：只有当前采样也是零覆盖，才积累稳定时长
        self.stable_secs += SAMPLE_INTERVAL_SECS;
        if self.stable_secs >= STABLE_SECS_EXTEND {
            self.stable_secs = 0;
            if self.rank < INTERVALS_SECS.len() - 1 { self.rank += 1; }
        }
    } else {
        // 任何非零覆盖（即使 < 50%）都应重置稳定计时，不延长周期
        self.stable_secs = 0;
    }
}
```

同时 `observe(bool)` 保留用于纯布尔测试场景，`observe_ratio` 改为正确的双语义处理。

需要补充回归测试：
```rust
#[test]
fn sub_threshold_coverage_does_not_extend() {
    let mut a = AdaptiveRelock::new();
    // 30% 持续覆盖——不应延长周期，也不应缩短
    for _ in 0..60 {
        a.observe_ratio(0.3);  // < 0.5 but > 0
    }
    assert_eq!(a.interval_secs(), 60, "sub-threshold coverage must not extend interval");
}
```

---

## 二、中严重度问题

### 🟠 BUG-M1 `handle_ipc::Reload` 重复了热加载路径逻辑，且使用陈旧的 `cfg` 参数变量

IPC `reload` 命令和主循环热加载路径是相同的三步序列，但分布在两处：

```rust
// handle_ipc (IpcRequest::Reload):
let cfg = store.current();          // 新 cfg（内部局部变量）
source.on_config_changed(&cfg);
tracker.retain_interested(&pkg_set(&cfg));
engine::relock_all(tracker, &cfg, topo, now_secs(), ...DecisionEngine::default()...);
//                                                   ↑ 注意：用的是 default()，不是 decision_engine!

// 主循环 hot-reload 路径:
source.on_config_changed(&cfg);
t.retain_interested(&pkg_set(&cfg));
engine::relock_all(&mut t, &cfg, &topo, now, &build_relock_ctx(&last_sys), &decision_engine, &backend);
//                                              ↑ 使用真实压力上下文和 decision_engine
```

两处存在两个实质差异：
1. IPC Reload 用 `DecisionEngine::default()`（默认配置），主循环用配置出的 `decision_engine`（含 migrate_action/pressure_sensitive）
2. IPC Reload 用 `now_secs()`（一个单独的函数调用），主循环用循环变量 `now`

**修复**：抽取 `fn do_reload(store, source, tracker, cfg_fn, decision_engine, topo, backend, now) -> String` 公共函数，两处路径都调用它。

---

### 🟠 BUG-M2 D3 即时 relock 与周期 relock 共享 1s 冷却门，边界场景周期 relock 可被延迟 ≤1s

主循环设置：

```rust
relock_guard.set_cooldown(1000);  // D3 使用，也被周期共享
```

场景：D3 在 t=2.9s 触发（ratio>0），guard.try_lock() 通过，`last_at=2.9`；
周期 relock 在 t=3.0s 检查：`now - last_lock = 3.0 - 2.9 = 0.1s < lock_interval(3s)` → 被 `last_lock` 跳过，不是被 guard 挡住。

实际上 `last_lock` 优化已经处理了大多数情况，guard 主要防止 D3 自身在 1s 内重复触发。DeepSeek Q2 的活锁分析是正确的：observe_ratio 每 2s 独立调用，窗口每 30s 满，周期不受影响。

这不是严重 bug，但共享 guard 的设计可以更清晰：D3 应有自己的防风暴 guard，周期 relock 不应受冷却约束。

---

### 🟠 BUG-M3 `IpcRequest::Apply(pid)` 的 pid 参数被完全忽略

```rust
IpcRequest::Apply(pid) => {
    let n = engine::relock_all(tracker, cfg, ...);  // pid 完全未使用
    writeln!(out, "apply {pid}: 全量重应用完成 (applied {n} threads)");
}
```

响应消息中提及 pid 但实际执行的是全量 relock。用户执行 `threadctl apply 12345` 期望只重应用 pid 12345，实际上把所有进程都重应用了一遍。文档中未说明。

**建议**：至少在响应消息中注明"（当前版本全量重应用，忽略 pid 参数）"，或 P8 实现单 pid 精确重应用。

---

## 三、低严重度问题

### 🟡 LOW-1 `sample_coverage` 内命名反直觉：`covered` = "被 AMS 移走"

```rust
for pid in &pids {
    if !is_in_our_cpuset(*pid, base) {
        covered += 1;  // ← 实际含义: "escaped / evicted / moved_out"
    }
}
```

`covered` 通常意味着"被我们覆盖到"，但这里是"被 AMS 覆盖走了"。这个命名惯例是 BUG-H1 产生的催化剂——将"被覆盖走"的 ratio 传入 `observe_ratio` 时，很容易误以为 "low coverage = stable"。

建议改名 `covered` → `evicted`，`sample_coverage` → `sample_eviction_ratio`。

---

### 🟡 LOW-2 测试注释过时：`adaptive_window_not_full_no_adjust` 说 "<6 采样"

```rust
// 窗口未满（<6 采样）→ 持续覆盖也不调整
for _ in 0..3 {
    a.observe(true);
}
```

`WINDOW_SAMPLES = 15`，注释说 `<6` 是旧设计残留（曾设计过 6 样本 × 2s = 12s 窗口）。测试本身是正确的（3 < 15），注释应改为"窗口未满（<15 采样，30s）"。

---

### 🟡 LOW-3 `owner_is_ours("/threadctl", "/dev/cpuset/threadctl")` 误判 BASE_CPUSET 根路径为"我们的"

```rust
fn owner_is_ours(owner: &str, base: &str) -> bool {
    let rel = base.trim_start_matches("/dev/cpuset");  // = "/threadctl"
    owner == rel  // ← "/threadctl" == "/threadctl" → true
    ...
}
```

如果进程被 AMS 移到 `/dev/cpuset/threadctl`（基础 cgroup，不是子目录），该函数返回 `true`，覆盖检测认为"在我们的 cpuset 里"，不触发 relock。实际上这个进程被放在了根 cgroup 而非我们的策略子目录中。

实践中不会频繁发生（我们只向 `BASE_CPUSET/<name>` 写 tasks），但属于逻辑漏洞。

---

### 🟡 LOW-4 `ipc.rs::spawn_ipc_server` 监听线程无关闭机制

SIGTERM 触发主循环退出，但 IPC 监听线程在 `for conn in listener.incoming()` 阻塞，JoinHandle 被直接 drop（detach）。主循环关闭了 ipc_tx 的发送端（drop），监听线程的 `tx.send()` 会返回 Err，但此路径只在接到请求后才执行，如果没有新连接进来，线程永远不退出（直到进程结束）。

和 reader 线程一样，进程退出时操作系统会回收，但对于需要 cpuset 清理的 SIGTERM 路径（P7.3 已修 SIGTERM），监听线程残留是技术债。

---

### 🟡 LOW-5 `debug_log!` 宏在 comm 解析时每事件一次 `from_utf8`（次要）

```rust
debug_log!("ebpf", "raw event type={} pid={} tid={} comm={:?}",
    ev.event_type, ev.pid, ev.tid,
    core::str::from_utf8(&ev.comm).unwrap_or("<bad>"));
```

`debug!` 路径只在 `DEBUG` flag 开启时执行（`AtomicBool::load(Relaxed)` 单读），对生产无影响。在 debug 模式下每事件一次 from_utf8 完全可接受。DeepSeek Q4 答案：无问题。

---

## 四、DeepSeek 问题答复

### P7.2

**Q1（混合窗口 stable_secs 重置语义）：** 本质上 BUG-H1。当 `covered_n == 0` 且实际 ratio > 0 时，`stable_secs` 不应积累。修复见 BUG-H1。

**Q2（D3 与周期活锁分析）：** 无活锁。仿真结论：observe_ratio 每 2s 独立调用，与 D3 是否触发 relock 无关。窗口 15 × 2s = 30s 后，`rank` 调整正确进行。D3 每次 relock 设置 `last_lock = now`，但这只跳过了随后的**周期**检查（正确），不影响 observe 窗口。**BUT**，前提是 BUG-H1 修复后——否则即使 D3 持续每 2s 对抗，30% 覆盖也会使周期被延长到 300s，产生实际的功能退化。

**Q3（TRACKED_TGID_MAP 1024 + 30s 同步）：** 1024 容量对 Android 足够（实际跟踪进程通常 < 100）。30s 同步可接受，极端情况下 cleanup_dead 清理了进程但 map 未及时清除，多余的 EXIT 事件会在用户态被过滤（tracker.contains 检查为 false 直接 drop）。不产生功能错误。若觉得 30s 太长，可缩为 10s，额外代价极小。

**Q4（debug comm from_utf8 开销）：** 可接受，见 LOW-5。

### P7.3

**Q1（单连接串行是否可接受）：** 可接受。CLI 是低频诊断工具，不是高并发服务。若同时两个 CLI 客户端，第二个等待时间 = 第一个请求的处理时间（< 1 主循环周期 ≈ 2s）。

**Q2（reload 重复逻辑）：** 应抽取公共函数（见 BUG-M1）。差异点（`DecisionEngine::default()` vs 真实引擎）尤其危险——IPC reload 的 relock 决策与主循环不一致。

**Q3（apply pid 精确重应用）：** P7.3 阶段全量 relock 合理（功能正确）。P8 时暴露 `engine::refresh_process_rules` 做单 pid 精确重应用即可。但当前应在响应消息中注明。

**Q4（dry-run 规则数）：** 如 `cfg.rules.dry_run_lines()` 输出已含每个包的规则行，用户可自行计数，不需要额外 `rule_count()` API。

---

## 五、修复优先级

| 优先 | 项 | 代价 |
|---|---|---|
| **立即** | BUG-H1 `observe_ratio` 双阈值修复 + 回归测试 | ~20 行 + 1 测试 |
| **P7.4 前** | BUG-M1 抽取 `do_reload()` 消除重复逻辑 | ~30 行 |
| **随时** | LOW-2 测试注释 `<6` → `<15` | 1 行 |
| **P8** | BUG-M3 `apply <pid>` 单进程精确重应用 | 需暴露 engine API |
| **P8** | LOW-1 命名重构 `covered` → `evicted` | 6 处替换 |
| **P8** | LOW-3 `owner_is_ours` 根路径边界 | 2 行 |