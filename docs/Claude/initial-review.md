现在读取配置文件和 KDL 示例：

全面深度审查完毕，整理输出：

---

# threadctl-rs P5/P6.1 深度审查报告

---

## 一、配置文件设计问题

你的判断正确，问题有两层。

### 1.1 `[[rule]]` 包名重复

当前 `threadctl.toml` 模板：

```toml
[[rule]]
pkg = "com.miui.home"
cpus = "0-6"

[[rule]]
pkg = "com.miui.home"
thread = "RenderThread"
cpus = "7"
sched = "fifo:60"
nice = -10

[[rule]]
pkg = "com.miui.home"
thread = "com.miui.home"
cpus = "3-6"
sched = "fifo:60"
nice = -10
```

`com.miui.home` 出现3次，`com.coolapk.market` 出现3次。包越多、线程越多，重复越严重。这是 v1 遗留格式的设计缺陷，`ConfigModel::from_toml` 已经能正确合并，但模板本身应该只用新格式。

**对应动作**：`threadctl.toml` 模板废弃 `[[rule]]`，全面迁移到 `[app]` 格式：

```toml
[app."com.miui.home"]
cpus = "0-6"

[app."com.miui.home".threads.RenderThread]
cpus = "7"
sched = "fifo:60"
nice = -10
```

或用 KDL（更简洁）：

```kdl
app "com.miui.home" {
    default { cpus "0-6" }
    thread "RenderThread" { cpus "7"; sched "fifo"; priority 60; nice -10 }
}
```

### 1.2 线程名书写——发现一个严重 Bug

`thread = "com.miui.home"` 的含义是：线程名 comm 等于 `com.miui.home`（精确 fnmatch）。Android 主线程的 comm 确实等于包名，这个写法对 MIUI home 有效（14字节，未超 TASK_COMM_LEN=15）。

但 `threadctl.toml` 中的这条规则是**已知死规则**：

```toml
[[rule]]
pkg = "com.coolapk.market"
thread = ".coolapk.market"   # ← 永远不会命中
```

**根因**：`com.coolapk.market` 有 20 字节，超过 `TASK_COMM_LEN=16`（含 NUL），内核把 comm 截断为 `com.coolapk.mar`（前15字节）。`fnmatch(".coolapk.market", "com.coolapk.mar")` = false。这条规则永远命中不了，是静默错误。

**正确写法**：
```toml
thread = "com.coolapk.mar"   # 截断后的实际 comm 值
```

或者用 `thread-type "main"` 别名（匹配 `*main*`，但酷安主线程 comm 里没有 "main"，不适用）。

**更根本的问题**：用户不应该需要知道 `TASK_COMM_LEN` 截断规则。建议在 config 校验阶段，当 `thread` 是精确名且长度超过 15 时打印警告：

```
警告: rule #6 thread "com.coolapk.market" 超过 15 字符，内核 comm 会被截断为 "com.coolapk.mar"
```

### 1.3 三种格式共存的维护负担

当前支持：`[[rule]]`（TOML 旧式）+ `[app]`（TOML 新式）+ KDL。混用时有一个 Bug：

`from_toml` 里 `[[rule]]` 先处理，`[app]` 后处理。同一 pkg 混用时，`[app]` 的 `default_policy` 是**全量替换**（不是 `merge_policy` 合并）：

```rust
if ac.cpus.is_some() || ac.sched.is_some() {
    entry.default_policy = PolicyModel { ... };  // 覆盖，非合并
}
```

如果 `[[rule]]` 设了 `cpus="0-6"` 和 `sched="fifo:60"`，而 `[app]` 只设了 `cpus="0-3"`，则 sched 丢失。实际场景不多，但应加注释或改为 `merge_policy`。

**建议**：在文档和模板中明确声明 `[[rule]]` 为"兼容旧格式，不建议新配置使用"，引导用户只用一种格式。

---

## 二、后台实时绑定问题

你的质疑方向正确，但结论需要分层。

### 2.1 relock 在 Android 上存在的理由

**AMS 会覆盖你设的 affinity**。Android 的 Task Profiles（ActivityManagerService）在应用切换时把进程从 top-app cpuset 迁移到 foreground/background cpuset，这个迁移会覆盖 `sched_setaffinity` 的效果（因为 cpuset 是硬边界，setaffinity 结果被 cpuset 的 effective_cpus 取交集）。不重锁就等于你的规则在应用切后立即失效。

**所以 relock 本身是 Android 上的刚需。**

### 2.2 但是不区分前后台是问题

当前 `relock_all` 遍历**所有被跟踪进程**，不管它们是否在前台。这导致：

- 后台的微信（oom_adj=100）每 60s 被重新 setaffinity 到 big 核——但后台微信根本不需要跑 big 核，让 Linux scheduler 自然决定反而更省电
- 大量无效 setaffinity 系统调用（getaffinity 短路能过滤一部分，但写 cpuset/tasks 仍每次都发生）

**`foreground.rs` 和 `decision.rs` 已经有正确的基础设施，但未接入 relock：**

```rust
// 当前 relock_all（engine.rs），不区分前后台：
for pid in pids {
    refresh_process_rules(tracker, pid, &pkg, cfg, now, rt_allowed, topo);
}
```

**建议做法**：

```rust
pub fn relock_all(...) -> usize {
    for pid in pids {
        let intent = TaskIntent::from_sources(
            read_oom_adj(pid),
            is_foreground_uid(pkg_uid(pid)),
            ThreadHint::Unknown,
        );
        let level = engine.decide(intent, current_pressure);
        if level == ActionLevel::Observe {
            continue;  // 后台低压力下跳过 relock
        }
        refresh_process_rules(...);
    }
}
```

**默认配置应体现这个语义**：`lock_interval = 60`（P4 已改，但 threadctl.toml 还是 5s，与文档不一致）。

---

## 三、DeepSeek 提问的回答

### P2 的 5 个问题

**Q1: engine.rs refresh_process_rules `mem::replace` 借用结构**

`mem::replace` 取出缓存、归还、`mark_scanned` 短借用——这个模式在 Rust NLL 下是安全的，没有释放失效风险。但有一个潜在问题：如果 `list_tids` 为空（进程在取出缓存后恰好退出），后面的 `enter(pid, ...)` 仍然会重新创建一个空状态并归还缓存。这个路径不造成 UB，只是轻微浪费。

**Q2: cpuset_refs 计数边角 case**

你提出的场景：同目录被两个进程引用，一个退出时归零→rmdir，另一个进程后续写入失败。这**是真实的边角 case**：

- pid1 引用 "0-3"，refcount=1
- pid2 引用 "0-3"，refcount=2
- pid1 退出 → refcount=1，**目录保留**，正确

但反过来：
- pid1 独占引用 "0-3"，refcount=1
- pid2 尚未 `register_dirs`（只发了 Fork 事件还没 apply）
- pid1 退出 → refcount=0 → **rmdir 目录**
- pid2 随后 apply_affinity → 写 `/dev/cpuset/threadctl/0-3/tasks` → **失败**，降级为无 cpuset 通道

这个 case 在理论上是可能的，但实际上 pid1 退出后 pid2 马上会触发 apply 并重建目录（`ensure_cpuset_dir`），所以窗口极短。低风险，可接受，但值得在注释中说明。

**Q3: Fork 后立即创建新线程的事件漏发**

Fork 后 engine 全扫（更新 applied_tids），下轮 poll 增量检测差集。如果线程在 Fork 事件处理完成到下轮 poll 之间创建并退出，会漏。这是可接受的边界情况，不影响稳态。

**Q4: sysinfo.procs 变化阈值**

Audit 的 H3 已指出并说明已修复（"仅增加时全扫"），但当前 `proc_source.rs` 的源码是：

```rust
let need_full = self.scan_all || proc_total != self.last_proc_total || proc_total == -1;
```

**H3 修复未完整落地**：`proc_total != last_proc_total` 仍然触发任意变化（增加或减少）全扫，而不是"仅增加时"。减少（线程退出）应该走 Exit 检测路径，不需要全扫。

**正确修复**：

```rust
let need_full = self.scan_all
    || proc_total == -1
    || proc_total > self.last_proc_total + FULL_SCAN_THRESHOLD;
const FULL_SCAN_THRESHOLD: i32 = 5;  // 容忍小幅波动
```

**Q5: engine 层单测 mock 方式**

推荐：抽象出 `ProcReader` trait：

```rust
pub trait ProcReader {
    fn list_tids(&self, pid: i32) -> Vec<i32>;
    fn read_cmdline(&self, pid: i32) -> Option<String>;
    fn read_thread_name(&self, tid: i32) -> Option<String>;
}
```

测试时注入 `FakeProcReader` 返回预设数据，无需真实 /proc。

### P3 的 5 个问题

**Q1: cgroup v2 支持时机**

P3 实现检测骨架、P4 完整实现是合理节奏。但要注意：当前 `create_cpuset_dir` 写的是 v1 格式的 `cpus` 和 `mems` 文件，v2 对应的是 `cpuset.cpus` 和 `cpuset.mems`，接口完全不同。P4 时需要在 `init_cpu_topo` 中检测路径，`create_cpuset_dir` 需要分支。

**Q2: tracing 体积**

release LTO 后 +50KB，完全可接受。P4 引入，现在所有 eprintln! 换成 tracing 宏。

**Q3: sched_switch 采样频率**

每进程每秒 1 次（1Hz）对于纠正场景足够。如果只是 Observe（默认），采样密度甚至可以更低（100ms 到 1s 都可以，重要的是 debounce 时间）。建议：采样 200ms，debounce 窗口 1s，避免调度抖动产生大量无意义事件。

**Q4: IPC 鉴权**

接受本地 socket 作为信任边界，不鉴权。若需要后续加，`SO_PEERCRED` 检查 UID=0 是最简单的方案，约 10 行代码。

**Q5: aya 版本**

Cargo.toml 没在本次提交里（只看到 core/daemon 的），无法确认。建议锁到 aya 0.12 或 0.13。

### P4 的 5 个问题

**Q1: signal_hook vs libc sigaction**

用 `signal_hook`。理由：signal_hook 用 self-pipe trick，安全地把信号异步化为 channel 接收，与 Rust 的异步/多线程模型兼容，避免在信号处理函数里做不安全操作。纯 libc sigaction 需要更多 unsafe 和 pipe 管理代码，增益不明显。

**Q2: tracing + Android logcat**

P4 引入 tracing + tracing-subscriber，P5 加 `tracing-android` crate（条件编译）。+50KB 可接受。

**Q3: IPC reload 与 inotify 防抖**

不需要防抖。IPC 触发 `store.reload()` 直接改 snapshot，不触及文件。inotify 关心的是文件 mtime 变化，两者独立，不会相互干扰。

**Q4: 并行 cpuset/tasks 写入并发安全**

内核保证对 `/dev/cpuset/*/tasks` 的每次 `write()` 是原子的（写一个 PID 整数）。多线程并发写入不同 tid 是安全的，无文件位置竞争（append 模式）。

**Q5: telemetry 测试隔离**

`audit.rs` 已将所有全局状态测试合并为一个 `ring_buffer_and_summary` 测试（带 `reset()`），这是正确的处理方式。对于 `WARNED_EPERM_TIDS` 等 policy.rs 里的全局 HashSet，测试时没有隔离机制，建议加 `#[cfg(test)]` 的 `clear_warned_sets()` 函数供测试用。

### P5 的 4 个问题

**Q1: MigrateAction::Suggest 触发条件**

建议：`thermal_pressure > 0.5` 且 `intent != Frozen` 时触发 Suggest（用 uclamp/cpuset 软引导）。不需要对任何系统状态变化都响应，只在热压力明显时引导任务到合适集群，让 scheduler 在约束范围内自由选择。

**Q2: uclamp 数值 profile**

| profile | uclamp_min | uclamp_max | 语义 |
|---|---|---|---|
| game/render | 700 | 1024 | 不限频，保 70% 底线 |
| audio | 400 | 700 | 中频稳定 |
| launcher | 600 | 1024 | 响应要快 |
| background | 0 | 300 | 省电优先 |
| power-save | 0 | 200 | 极度省电 |

**Q3: eBPF UID 过滤**

用用户态传入 UID 列表。流程：`foreground.rs::refresh_foreground_uids()` → 更新 `FOREGROUND_UIDS` → `EbpfSource::on_config_changed()` 读取并更新内核 BPF map。内核不应该负责 UID 探测，那是用户态的工作。

**Q4: 规则审计**

`audit.rs` 已实现，`policy.rs::apply_affinity` 每次调用都写入 `AuditEntry`。闭环路径：`apply` → `audit::record` → `audit::summary_windowed(60)` → 决策引擎调整。但 **decision.rs 还没读取 audit summary**，`Adjust` 环节是空的。P6.2 接入。

---

## 四、新发现的代码 Bug 和设计问题

### 🔴 Bug 1：H3 修复不完整（proc_source.rs）

如上述 P2 Q4 分析，`proc_total != last_proc_total` 仍触发任意变化的全扫，不只是增加。线程频繁退出（减少）时也会每轮全扫，H3 修复目标未达成。

```rust
// 现在（错误）
let need_full = self.scan_all || proc_total != self.last_proc_total || proc_total == -1;

// 修复
let need_full = self.scan_all
    || proc_total == -1
    || proc_total > self.last_proc_total + 5;  // 仅增加超阈值时全扫
```

### 🔴 Bug 2：`.coolapk.market` 线程名永不命中

见上文 1.2 节。`threadctl.toml` 中已有的这条规则是死规则，需要修正为 `com.coolapk.mar`，并在 RuleSet::compile 中加长度校验警告。

### 🟠 问题 3：engine.rs `SkippedNoCpus` 被计入 applied

```rust
let outcome = policy::apply_thread(*tid, pkg, &policy, topo, rt_allowed);
if outcome == ApplyOutcome::Exited { continue; }
applied += 1;  // SkippedNoCpus 也被计数
```

`SkippedNoCpus` 是占位规则（空 cpus，只应用 sched），亲和性未变，计入 applied 会误导"应用 N 个线程"的日志。

```rust
// 修复
match outcome {
    ApplyOutcome::Exited => continue,
    ApplyOutcome::SkippedNoCpus => {
        // sched 已应用，但 affinity 跳过，单独计数或记 audit
    }
    _ => { applied += 1; }
}
```

### 🟠 问题 4：thermal_pressure() 与 sample() 独立采集，时间不同步

`decision.evaluate()` 调用 `SystemContext::thermal_pressure()`，这是对 `/sys/class/thermal/cooling_device*` 的实时读取，而 `sample()` 返回的是上次 `AdaptivePoller` 触发时的 snapshot。两者可能相差几秒，决策基于不同时间点的数据。

```rust
// 修复：在 SystemContext 里缓存 thermal_pressure
pub struct SystemContext {
    ...
    pub thermal_pressure: f64,  // 由 sample() 计算
}

// sample() 里
let thermal = Self::thermal_pressure_internal();
Self { ..., thermal_pressure: thermal }
```

### 🟠 问题 5：foreground.rs 用 UID 精度不足

`FOREGROUND_UIDS` 检测"某个 UID 的任意进程是否在 top-app cpuset"，但同一 UID（如 system）有大量进程，部分在前台部分不在。应改为维护 `foreground_pids: HashSet<i32>`，直接以 pid 判断：

```rust
pub fn is_foreground_pid(pid: i32) -> bool { ... }
```

### 🟠 问题 6：merge_by_priority 注释与行为不一致

文档注释说"高优先级来源的字段覆盖低优先级来源"，但实际上 `sched` 是"最高优先级首个有值者生效"（不是覆盖而是跳过）：

```rust
if sched.is_none() && r.policy.sched.is_some() {
    sched = r.policy.sched;  // 首个生效，不是"覆盖"
}
```

如果 exact 规则无 sched，wildcard 规则有 sched，wildcard 的 sched 会被采纳。这是"继承"而非"覆盖"，与注释的"覆盖"语义不符。注释应更精确：

```
- sched/nice：highest-priority source that has a value wins
```

### 🟡 问题 7：profile 在非 Snapdragon 设备上静默跳过

`profile "game"` 的 render 线程规则指定 `cluster "prime"`，在没有 prime 集群的设备（同构 x86 桌面）上，`policy_to_rules` 找不到 cluster，打印 eprintln 警告后**静默跳过整条规则**。用户不知道 game profile 的 render 规则没有生效。

建议：fallback 到 `big` 集群（或 online cpus），而不是跳过：

```rust
let found = clusters.iter().find(|c| ...)
    .map(|c| c.cpus.to_range_string())
    .or_else(|| {
        // fallback：找容量最大的可用集群
        clusters.last().map(|c| c.cpus.to_range_string())
    });
```

### 🟡 问题 8：read_start_time 注释错误

```rust
.nth(19) // starttime: 第 22 字段，0-indexed 是 after_comm 的第 20 个
```

`nth(19)` 是 0-indexed 第 19（即第 20 个），注释说"第 20 个"——歧义（从 0 数还是从 1 数）。代码本身是正确的，注释应写：

```rust
.nth(19) // after ')' 后的第 20 个空白分隔字段（0-indexed 19），对应 /proc/pid/stat 第 22 字段 starttime
```

---

## 五、五模块"未接入"问题的根本评估

`capability.rs`、`system_context.rs`、`decision.rs`、`foreground.rs`、`audit.rs` 五个模块全部是 dead code（M4 问题），但 Audit 报告说"已修复"并验证了 `Capability 启动打印`。

实际查看 `main.rs` 源码，只看到 `CapabilitySet::detect()` 在启动时打印摘要，其余四个模块完全没有在事件循环中使用。**M4 修复是表面的**，只接入了 Capability 的打印，核心的 DecisionEngine + SystemContext 采样 + foreground 更新 + audit 反馈都未真正接入主循环。

这五个模块作为 P5 阶段骨架存在是可以接受的，但 Audit 报告把 M4 标记为"✅ 冒烟输出 Capability"是夸大的——有一行 summary 打印≠接入。

---

## 六、问题优先级汇总

| 优先级 | 问题 | 影响 |
|---|---|---|
| 🔴 | H3 修复不完整，线程退出也触发全扫 | /proc 过度读取，CPU 浪费 |
| 🔴 | `.coolapk.market` 线程名截断，规则永不命中 | 静默功能失效，用户不知道 |
| 🟠 | `SkippedNoCpus` 计入 applied，日志误导 | 可观测性问题 |
| 🟠 | `thermal_pressure()` 与 `sample()` 不同步 | 决策基于过期数据 |
| 🟠 | `foreground.rs` 用 UID 而非 pid，精度不足 | 前台检测误报 |
| 🟠 | relock_all 不区分前后台，后台进程无效 relock | 多余 syscall |
| 🟠 | `[[rule]]` 模板废弃不及时，截断规则保留 | 用户困惑 |
| 🟡 | `merge_by_priority` 注释与"覆盖"语义不符 | 维护理解成本 |
| 🟡 | profile 在无对应集群设备上静默跳过 | 功能无效但无提示 |
| 🟡 | M4 五模块实际只接入了 Capability 打印 | 大量 dead code，审查文档夸大 |

纠正Claude是Android端程序，Claude补充回复:

这个信息改变了很多判断。让我针对 Android 专属重新审查：

---

# Android 专项深度审查

---

## 一、Bionic libc 与 glibc 的核心差异对代码的影响

### `sysinfo.procs` 在 Android/Bionic 上是线程数

H3 bug 的根因在这里。Linux glibc 的 `sysinfo.procs` 返回**进程数**，但 Android Bionic 的 `sysinfo.procs` 返回**任务数（含所有线程）**：

```
SM8550 实测：system_server 有 100+ 线程，微信有 50+ 线程
→ sysinfo.procs ≈ 全设备线程总数（通常 1000+）
→ 线程创建/退出是常态，procs 每轮必变
→ proc_total != last_proc_total 几乎每轮成立
→ H3 等于没修复：全扫每轮触发
```

这是 proc_source 在 Android 上的根本性能问题。正确做法：

```rust
// 不用 sysinfo 的 procs，改读 /proc 目录项计数（真实进程数）
fn count_processes() -> usize {
    fs::read_dir("/proc")
        .map(|d| d.flatten()
            .filter(|e| e.file_name().to_string_lossy()
                .chars().all(|c| c.is_ascii_digit()))
            .count())
        .unwrap_or(0)
}
```

或者直接抛弃进程总数判断，改为**时间驱动**的全扫（每 `scan_interval` 秒一次），增量靠 Exit 事件和 ThreadClone 事件处理。

### Android Zygote fork 模型的空窗

Android 所有 App 都是从 Zygote fork 出来的。fork 刚发生时：
- 新进程 pid 存在
- cmdline 还是 `zygote` 或空（内容填充有延迟）
- 此时 `read_cmdline` 返回 `None` 或返回 `"zygote"`

`is_interested("zygote")` → false，事件被丢弃，这是对的。但 Zygote 本身的 pid 在 proc 全扫时会被反复检查，因为 `/proc/<zygote_pid>/cmdline` 读到 "zygote"，不在白名单，跳过，没有问题。

但有一个边界：fork 发生后、cmdline 改写前，新进程以 Zygote 身份出现，全扫可能错过它。等到下次全扫时 cmdline 已经是真实包名了，才会被发现。**延迟约一个 scan_interval**（2s）。对绑核要求严格的游戏，这 2s 可能已经渲染了几十帧。

**建议**：对新增进程，延迟 200ms 再读 cmdline 做二次确认：

```rust
// Fork 事件处理时：
if pkg.is_none() || pkg.as_deref() == Some("zygote") {
    // 放入待确认队列，200ms 后重读
    pending_pids.push((pid, Instant::now() + Duration::from_millis(200)));
    return;
}
```

---

## 二、Android 线程名 TASK_COMM_LEN 截断是高频问题

### 截断对 Android 的实际影响

Android 包名普遍很长。主线程 comm = 包名前15字节：

| 包名 | comm（实际值） | 长度 |
|---|---|---|
| `com.miui.home` | `com.miui.home` | 14✅ |
| `com.coolapk.market` | `com.coolapk.mar` | 15✅（截断）|
| `com.tencent.mm` | `com.tencent.mm` | 15✅ |
| `com.netease.cloudmusic` | `com.netease.clou` | 15（截断）|
| `com.miHoYo.Yuanshen` | `com.miHoYo.Yuans` | 15（截断）|

配置中的 `thread = "com.coolapk.market"` 永远匹配不了，这是**静默的配置错误**。

### 根本解法：识别主线程用 tid==pid，不用线程名

Android 主线程的 tid 等于 pid（POSIX 语义）。只需一个特殊的 thread 匹配模式：

```rust
// 在 RuleConfig 或 PolicyModel 里加字段
pub struct RuleConfig {
    pub thread: String,         // 现有字段：fnmatch 模式
    pub main_thread_only: bool, // 新字段：仅匹配主线程（tid==pid）
}
```

或在 KDL/TOML 里用关键字：

```kdl
thread "@@main" { ... }   // 特殊语法，匹配 tid==pid
```

在 `resolve` 时检查：

```rust
if rule.main_thread_only {
    if tid != pid { continue; }
} else {
    if !fnmatch_c(pat, thread) { continue; }
}
```

在 `engine.rs` 的 `refresh_process_rules` 里把 tid 传进 resolve：

```rust
fn resolve(pkg: &str, thread: &str, tid: i32, pid: i32) -> Option<Policy>
```

### 短期 workaround：compile 时警告截断

```rust
// RuleSet::compile 里
if !rc.thread.contains('*') && rc.thread.len() > 15 {
    eprintln!(
        "警告: rule #{} thread \"{}\" 超过 15 字节，Android 内核会截断 comm 为 \"{}\"",
        i, rc.thread, &rc.thread[..15]
    );
}
```

---

## 三、relock 在 Android 上的正确理解

### 为什么必须有 relock

Android AMS 用 Task Profiles 管理应用的 cpuset：
- App 切到后台 → AMS 写 `/dev/cpuset/background/cgroup.procs`
- 这个写操作把 App 从 `top-app` cpuset 迁出
- cpuset 是硬边界：`sched_setaffinity(0-7)` 被 `background/cpus(0-3)` 截断为 `0-3`
- threadctl 之前设的 affinity 全部失效

**relock 是 Android 上不可省的机制**，你之前的疑问基于"后台不需要 big 核"，这是对的，但 relock 的真正作用是**恢复 cpuset 归属**，不只是 setaffinity。

### 但当前 relock_all 有问题

relock_all 遍历**所有 tracked 进程**，包括已经在后台的进程，无差别重新应用：

```
前台游戏 → relock → 正确：恢复 prime cpuset
后台微信 → relock → 错误：把微信拉回 threadctl cpuset，与 AMS 持续对抗
```

把微信从 AMS 指定的 background cpuset 强制拉回 threadctl 的 big-cluster cpuset，会：
1. 浪费大核频率（后台微信不需要）
2. 与 AMS 持续对抗，5s 间隔 × 几十个后台 App = 不断的 cpuset 争抢
3. 在 MIUI/HyperOS 上可能触发系统自身的进程管理反制

### 正确做法：relock 只处理前台或用户明确要求的进程

`foreground.rs` 已有基础设施，但未接入：

```rust
pub fn relock_all(tracker: &mut StateTracker, cfg: &ConfigSnapshot, ...) {
    for pid in pids {
        let pkg_uid = get_pkg_uid(pid);
        let in_foreground = is_foreground_uid(pkg_uid); // foreground.rs
        
        if !in_foreground && !cfg.rules.force_relock_for(&pkg) {
            continue; // 后台进程跳过 relock
        }
        
        refresh_process_rules(tracker, pid, ...);
    }
}
```

或者更简单：让 `lock_interval` 对前台和后台使用不同值：

```toml
lock_interval = 60          # 后台进程的 relock 间隔（秒）
foreground_lock_interval = 5  # 前台进程的 relock 间隔（秒）
```

---

## 四、cgroup 路径：SM8550/SM8650 的实际情况

SM8550（Snapdragon 8 Gen 2）= Android 13，SM8650（Snapdragon 8 Gen 3）= Android 14。

MIUI/HyperOS 在这两款平台上的 cgroup 实际状态：
- **cgroup v1** 的 `/dev/cpuset` 仍然存在（MIUI 的 AMS 和 Task Profiles 依赖它）
- **cgroup v2** 的 `/sys/fs/cgroup` 也存在（Android 12+ 双挂载）
- 写 `/dev/cpuset/threadctl/*/tasks` 有效

**当前代码 hardcode `/dev/cpuset` 对 MIUI/HyperOS 是正确的，短期不需要改。**

但需要注意：MIUI/HyperOS 有自己的 cpuset 子目录结构，比如：

```
/dev/cpuset/
  top-app/
  foreground/
  background/
  system-background/
  restricted/
  camera-daemon/         ← MIUI 私有
  miui-foreground/       ← MIUI 私有
  threadctl/             ← 我们创建的
```

写 `/dev/cpuset/threadctl/<range>/tasks` 会把线程从 AMS 管理的 cpuset（如 top-app）迁移到 threadctl 的 cpuset。这要求：
1. threadctl cpuset 的 `cpus` 包含足够的 CPU 核
2. AMS 不主动把线程写回它自己的 cpuset（这就是 relock 存在的原因）

---

## 五、Android 上不需要的东西

### CAP_SYS_NICE 检查可以简化

Magisk 模块以 root 运行，`euid == 0` 永远为 true，`can_rt_sched()` 永远返回 true。`caps.rs` 的 capget 调用永远不会执行到（因为 geteuid==0 直接返回 true）。

这不是 bug，但应该在注释里说明，避免未来维护者困惑：

```rust
pub fn can_rt_sched() -> bool {
    // Android Magisk 模块以 root 运行，此处永远返回 true。
    // 保留 CAP_SYS_NICE 路径供桌面 Linux 非 root 场景使用。
    if unsafe { libc::geteuid() } == 0 {
        return true;
    }
    has_cap(CAP_SYS_NICE)
}
```

### PSI 在旧 Android 设备上不存在

`/proc/pressure/memory` 需要 kernel 4.20+ 且开启 PSI。SM8550/SM8650 的 kernel 5.10/5.15 有 PSI，但 MIUI 不一定开启了 `CONFIG_PSI`。

当前代码：

```rust
fn read_memory_pressure() -> (PressureLevel, f64) {
    let content = fs::read_to_string("/proc/pressure/memory").unwrap_or_default();
    // 文件不存在时 unwrap_or_default() 返回空字符串
    // for 循环找不到 avg10= → 返回 (Normal, 0.0)
    ...
}
```

PSI 不可用时静默返回 Normal，这是正确的降级，但应该在启动时 log 一次：

```rust
if !Path::new("/proc/pressure/memory").exists() {
    eprintln!("系统感知: PSI 不可用，内存压力感知已禁用");
}
```

---

## 六、Android 特有的进程跟踪问题

### Binder 线程的命名规律

Android 的 Binder 线程名格式：`Binder:12345_1`，其中 12345 是 pid。这意味着**每个进程的 Binder 线程名都是唯一的**，无法用静态 fnmatch 模式匹配。

当前的 `THREAD_TYPE_ALIASES`：

```rust
("binder", &["*Binder:*"]),
```

`*Binder:*` 能匹配所有 Binder 线程，但 Binder 线程通常不需要特殊绑核（它们只处理 IPC，开销极低）。这个 thread-type 对 Android 来说基本没用，可以从默认 profile 里去掉。

### system_server 的特殊处理

`system_server` 是 Android 最重要的进程，包含 AMS、WMS 等核心服务。它的 cmd 是 `system_server`。如果用户把它加入规则（有人会），需要特别注意：

- `system_server` 的 oom_adj = -900（永久存活）
- 强制绑核可能影响 AMS 的调度响应
- **建议**：在 RuleSet::compile 里对 `system_server` 加警告

### 包名 vs 进程名的不一致

部分 Android 进程：

| 情况 | cmdline | 实际包名 |
|---|---|---|
| 普通 App | `com.example.app` | `com.example.app` ✅ |
| 系统 App | `com.android.phone` | `com.android.phone` ✅ |
| Native 守护进程 | `/system/bin/surfaceflinger` | N/A |
| 多进程 App | `com.example.app:service` | 主包 `com.example.app` |

对多进程 App（`:service`、`:push` 子进程），`read_cmdline` 读到的是带冒号的完整进程名，不等于包名。当前的 `rsplit('/')` 处理不了这个：

```rust
// 现有代码
let name = cmdline.rsplit('/').next().unwrap_or(cmdline);
// "com.example.app:service" → "com.example.app:service"
// 匹配规则 pkg="com.example.app" → is_interested → false → 被忽略
```

这可能是期望行为（子进程单独配置），但如果用户想让 `com.example.app` 规则同时覆盖它的 service 子进程，当前设计做不到。

**短期**：文档说明多进程 App 需要单独配规则：
```toml
[app."com.example.app:service"]
cpus = "0-3"
```

---

## 七、MIUI/HyperOS 特有的对抗

### MIUI 的进程管理层

MIUI/HyperOS 有自己的进程生命周期管理（不同于 AOSP AMS）：
- **MIUI Boost**：前台 App 使用时主动提升 cpuset
- **进程保活**：某些 App 被 MIUI 标记为保活，持续在 foreground cpuset
- **冷冻机制（Frozen）**：MIUI 会把后台 App 进程冻结（SIGSTOP），此时 threadctl 的 kill(pid, 0) 仍然返回 0（进程存在），但 setaffinity 对已冻结进程有效吗？

实测：`SIGSTOP` 的进程仍然可以 `sched_setaffinity`（内核接受，不报错），但 affinity 对冻结进程无意义。

**建议**：在 `refresh_process_rules` 前检查进程状态：

```rust
// 读 /proc/<pid>/status 的 State 字段
fn is_frozen(pid: i32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("State:"))
                .map(|l| l.contains('T')) // T = stopped (SIGSTOP)
        })
        .unwrap_or(false)
}
```

### MIUI 的 cpuset 层次可能与 threadctl 冲突

MIUI 的 AMS 有更激进的 cpuset 管理。在某些 MIUI 版本上，system_server 有额外的线程负责**周期性检查并还原 cpuset 归属**。这意味着 threadctl 的 relock 和 MIUI 的 cpuset 还原可能形成 5s 粒度的拉锯战。

**解决思路**：与其和 MIUI 对抗，不如**通过 MIUI 提供的接口（如果有）设置偏好**，或者把 lock_interval 设置得比 MIUI 的检查周期更短（需要实测 MIUI 的检查频率）。

---

## 八、Android 平台的配置推荐改写

针对 Android/MIUI/HyperOS 的正确 KDL 模板应该是：

```kdl
daemon {
    // Magisk 模块：必须用绝对路径
    pid-file "/data/adb/modules/threadctl/run/threadctl.pid"
    ipc-socket "/data/adb/modules/threadctl/run/threadctl.sock"
    log-level "info"
}

engine {
    mode "proc"              // SM8550/SM8650 可用 ebpf，但先用 proc 验证
    scan-interval 2          // 秒
    lock-interval 60         // 后台进程：60s 一次（不要 5s，那是和 AMS 死磕）
    dead-cleanup-interval 30
    migrate-action "observe" // 不干预 scheduler 的主动迁移
}

// 原神：游戏需要 prime + big，前台才有意义
app "com.miHoYo.Yuanshen" {
    default { cluster "big" }
    thread "UnityMain" {
        cluster "prime"
        sched "fifo"
        priority 10
    }
    thread-type "render" { cluster "big" }
}

// 微信：主要用户场景是前台聊天，后台省电
app "com.tencent.mm" {
    default { cluster "big" }
    thread "RenderThread" {
        cluster "prime"
        sched "fifo"
        priority 60
    }
}

// MIUI 桌面：必须用 cpus，不要用 cluster
// （cluster 名只有 little/big/prime，桌面需要精确范围）
app "com.miui.home" {
    default { cpus "0-6" }
    thread "RenderThread" {
        cpus "7"
        sched "fifo"
        priority 60
        nice -10
    }
    // com.miui.home 主线程（14字节，刚好不截断）
    thread "com.miui.home" {
        cpus "3-6"
        sched "fifo"
        priority 60
    }
}
```

---

## 九、Android 平台问题优先级重排

| 优先级 | 问题 | Android 影响 |
|---|---|---|
| 🔴 | `sysinfo.procs` 统计线程数，全扫每轮触发 | 持续高 CPU 开销，最严重 |
| 🔴 | 线程名截断无警告，`*.coolapk.market` 死规则 | 配置静默失效 |
| 🔴 | relock 不区分前后台，与 AMS/MIUI 持续对抗 | 耗电、拉锯战 |
| 🟠 | Zygote fork 后 cmdline 空窗，新 App 发现延迟 | ~2s 延迟，游戏开屏规则晚生效 |
| 🟠 | 多进程 App `:service` 子进程匹配不到主规则 | 部分子进程不受控 |
| 🟠 | MIUI 冻结进程（SIGSTOP）浪费 relock 调用 | 无用功 |
| 🟡 | 无主线程 `tid==pid` 匹配语法 | 用户需要手动截断包名 |
| 🟡 | PSI 不可用时无启动提示 | 诊断困难 |
| 🟡 | pid_file / ipc_socket 相对路径 | Magisk 启动时 cwd 不确定 |