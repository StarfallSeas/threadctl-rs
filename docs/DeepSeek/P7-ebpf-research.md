# P7.1 调研 — eBPF 事件源设计模式与工程考量

> 调研人：DeepSeek V4 Flash
> 日期：2026-08-09
> 目的：P7.1（eBPF 事件源）写代码前的设计调研——Linux eBPF tracepoint
> 事件源的通用工程模式（白名单过滤 / 防抖 / ringbuf 消费 / 降级链）。
> 状态：调研完成，**未开始写代码**（按 boss 指示）

---

## 一、eBPF 事件源架构全景（通用模式）

```text
内核态（aya-ebpf，no_std，~180 行）：
  sched_process_fork tracepoint → 事件（child_pid + child_comm）
  sched_process_exec tracepoint → 事件（current pid/tgid + comm）
     ↓ 白名单过滤（TARGET_COMM_MAP：包名 8 字节片段，滑动窗口匹配）
     ↓ 防抖（DEDUP_MAP：每 pid 0.1s 窗口内最多 2 事件）
     ↓ 写入 EVENTS ringbuf（256KB）

用户态（aya，~400 行）：
  check 支持性 → 加载 → attach tracepoint → ringbuf → 消费线程
  reader 线程      → ring_buf.next() 阻塞，None 时 sleep 50ms
                    → mpsc channel → 主循环 handle_events
  事件处理         → read_cmdline(pid) 匹配包名 → 进程缓存
                    → 全线程刷新 + 当前 tid apply
  失败回退         → 每步日志 + 回退 /proc 轮询

配置更新：清缓存 → 全量扫 /proc 发现已运行进程
周期刷新：kill(pid,0) 清死进程 + 重扫
```

---

## 二、七个通用设计模式（P7.1 采用）

### 1. 白名单内核态过滤（省 ringbuf/用户态压力）

- 内核 `TARGET_COMM_MAP: HashMap<[u8;8], u32>`，键 = 包名**前 8 字节 + 末 8 字节**
- 内核态对 comm 做滑动窗口（pos 0..=8）取 8 字节子串匹配——任意位置命中即上报
- 用户态 `build_target_entries()`：每个包名 1-2 个键，排序去重
- **价值**：非目标进程（系统/其他 app）的 fork/exec 全部内核态丢弃，ringbuf 只收
  白名单进程事件——这是高负载下 eBPF 可行的关键
- **P7.1 采用**：`TARGET_COMM_MAP` 机制保留；但 comm 匹配是"进程名"近似
  （15 字符裁剪），**用户态仍须 read_cmdline 精确匹配包名**（双保险）

### 2. 内核态防抖（DEDUP_MAP）

- `LruHashMap<u32, DedupEntry{last_ns, count}>`，0.1s 窗口每 pid 最多 2 事件
- **价值**：高频 fork 风暴（游戏引擎批量建线程）不冲击 ringbuf 与用户态
- **P7.1 采用**：保留；但注意——0.1s 窗口对 threadctl 的**线程 clone 分流**
  需调参：线程高频创建场景 2 事件/0.1s 可能不够，需校准

### 3. reader 线程 + mpsc 通道

- 独立线程阻塞 `ring_buf.next()`，None 时 sleep 50ms → mpsc → 主循环
- **价值**：与 threadctl 主循环结构兼容（主循环保持单线程可变状态所有权）
- **P7.1 采用**：完全一致——reader 线程发 mpsc，主循环 poll 消费
- **延迟现实**：50ms poll 间隔 → 事件发现延迟 ~50ms 级，**非亚毫秒**
  （三审结论：表述为"near-real-time 事件发现"）

### 4. 降级链（每步可回退）

```text
check 支持 → try_load → attach → ringbuf → reader
   ↓失败      ↓失败      ↓失败    ↓失败      ↓失败
 回退 proc   回退 proc  回退 proc 回退 proc  回退 proc
```

- 每步日志说明失败原因 + 回退——用户可诊断
- **P7.1 采用**：完全一致；threadctl 有 `CapabilitySet` 模式可挂 ebpf 探测

### 5. 进程缓存 + initial_scan_done 防抖

- `process_cache: HashMap<i32, ProcessScanState{initial_scan_done, last_scan_time}>`
- 首次事件全线程刷新，后续按扫描间隔节流
- **P7.1 采用**：对应 threadctl 已有 tracker/THREAD_SCAN_TTL 机制，语义等价

### 6. EXEC 重置扫描状态

- `sched_process_exec` → `initial_scan_done=false` + 清 tid 缓存（exec 替换映像）
- **P7.1 采用**：对应 threadctl engine 的 `EventKind::Exec → tracker.remove`，语义一致

### 7. 配置更新全量扫描

- 配置更新时清缓存 + 全量扫 /proc（发现已运行的白名单进程）
- **P7.1 采用**：threadctl 热加载已有 `retain_interested + relock_all`，语义等价

---

## 三、五个工程决策点（P7.1 需改/确认）

### 1. 版本：aya 0.13.1 / aya-ebpf 0.1（2023）→ 当前最新（2026）

- 早期示例锁的是旧版 API：`#[map] static` 宏、`RingBuf::with_byte_size`、
  `bpf.program_mut(name)`、`loader.set_max_entries`
- 新版 aya（0.14+）API 变动：map 定义宏、RingBuf API、加载器接口都可能不同
- **P7.1 已确认**：当前 aya 0.14.0 / aya-ebpf 0.2.1，编译通过
  （`set_max_entries` 已弃用为 `map_max_entries`）

### 2. sched_process_exit（threadctl 新增）

- 早期示例靠 `kill(pid,0)` 周期清理死进程
- threadctl P7.1 要 `sched_process_exit` → 即时清理（线程退出清 applied_tids，
  修 TID 复用）——**新增 tracepoint**。SM8550 5.15 format 已确认：
  `comm[16]@8, pid@24, prio@28`；但 tracepoint 参数**无 tgid**——
  线程退出时 `/proc/<tid>` 已消失无法事后读 → **内核态用
  `bpf_get_current_pid_tgid()`**（current 即退出任务）带出 tgid/pid

### 3. 线程 clone 分流（用户态做）

- 早期示例把 `sched_process_fork` 的所有事件都当进程 fork（pid=child_pid）
- threadctl 需要 `tgid != pid → ThreadClone` 分流——fork tracepoint 参数
  只有 child_pid（无 child tgid），且触发时 current 是**父进程**：
  → **用户态分流**：fork 事件进 pending 队列，cmdline 就绪后读
  `/proc/<pid>/status` 的 Tgid：`Tgid==Pid → Fork`，`Tgid!=Pid → ThreadClone`
  ——与 Zygote pending 天然合并，零内核态复杂度

### 4. comm 匹配 vs 包名匹配的语义差异

- 白名单匹配 comm（进程名，15 字符裁剪）
- threadctl 匹配 /proc cmdline（完整包名）
- **差异场景**：`com.ss.android.ugc.aweme`（23 字符）→ comm 被裁成
  `com.ss.android.ug`；且 **comm 与包名可能完全不同**（Android 进程名可自由
  设置，如抖音主进程 comm = "droid.ugc.aweme"）
- **P7.1 结论**：内核态白名单用 comm 只是**粗过滤**（减少无关事件），
  精确匹配永远在用户态 read_cmdline；白名单键 = 配置包名的 comm 近似
  （前 8 + 末 8）——无法覆盖 comm≠包名的场景 → 这些进程的 fork 事件会丢
  → **补救**：定期全扫兜底（现有 TTL）+ 白名单键**额外包含已知 comm 别名**

### 5. 事件结构布局

- `ProcEvent{pid, tid, child_pid, comm[16], event_type}` `#[repr(C)]`
- threadctl P7.1：pid/tgid 语义统一（FORK=child_pid；EXEC=tgid；EXIT=tgid）
- 版本对齐：`#[repr(C)]` 布局必须内核/用户态一致（编译期校验或文档固化）

---

## 四、P7.1 设计定稿（基于调研）

```text
内核态（threadctl-ebpf，aya-ebpf 0.2）：
  sched_process_fork  → ForkRaw 事件（child_pid + child_comm + event_type）
  sched_process_exec  → ExecRaw 事件（pid + comm + event_type）
  sched_process_exit  → ExitRaw 事件（tgid + tid，helper 提供）   ← 新增
  白名单 TARGET_COMM_MAP（8 字节滑动窗口，粗过滤）
  防抖 DEDUP_MAP（0.1s 窗口，参数可调）
  EVENTS ringbuf（256KB）

用户态（threadctl daemon，EbpfSource 实现 EventSource trait）：
  reader 线程 → mpsc → 主循环
  fork 事件 → pending 队列（复用 Zygote pending）→ cmdline 就绪后
    读 /proc/<pid>/status Tgid：Tgid==Pid → Fork；Tgid!=Pid → ThreadClone
  exec 事件 → Exec（tracker.remove + refresh，现有语义）
  exit 事件 → 即时清 applied_tids / tracker（修 TID 复用）
  精确匹配：用户态 read_cmdline（白名单 comm 只是粗过滤）
  降级链：每步失败 → 回退 ProcSource

构建链（P7.1 第 0 步，已验证）：
  aya 0.14/aya-ebpf 0.2.1 + bpf-linker 0.10.4 + lld 21
  + rust-src（-Zbuild-std=core，官方 bpf std 组件已停发）
  → .bpf.o 5.8KB 产出成功
```

**两个设计决策**：
1. **线程分流在用户态做**（pending 后读 Tgid），非内核态——零内核复杂度，
   与 Zygote pending 合并
2. **白名单 comm 是粗过滤 + 用户态 cmdline 精确匹配**（双保险），
   无法覆盖 comm≠包名的进程 → 全扫兜底

---

## 五、风险确认清单（P7.1 实测结果）

| # | 风险 | 确认方式 | 结果 |
|---|---|---|---|
| 1 | 新版 aya/aya-ebpf API 迁移 | crates.io + 编译样例 | ✅ 0.14/0.2.1 编译通过 |
| 2 | bpf-linker/LLVM 在 Termux 可装 | pkg install + 编译 .bpf.o | ✅ 产出成功 |
| 3 | 设备 BTF 可用 | /sys/kernel/btf/vmlinux | ⚠️ 本设备缺失——tracepoint raw 读取不依赖 BTF，加载实测成功 |
| 4 | sched_process_exit 格式 | 读 tracing/events format | ✅ 确认（helper 带 tgid） |
| 5 | fork 参数偏移（5.15） | 读 format 对照 | ✅ child_comm@28 / child_pid@44 |
| 6 | comm 匹配覆盖（comm≠包名） | 真机观察 | 全扫兜底 |

---

## 六、结论

P7.1 基于 Linux eBPF tracepoint 事件的通用工程模式设计：白名单粗过滤 +
防抖 + reader 线程 + 降级链七个模式；差异点（exit/线程分流/包名语义）
已在定稿中解决，线程分流为用户态 pending 后读 Tgid（更简单可靠）。

**P7.1 状态**：构建链 ✅、内核态 ✅、用户态 EbpfSource ✅、
真机验证 ✅（BTF 缺失下加载成功，fork 事件驱动即时应用）。
