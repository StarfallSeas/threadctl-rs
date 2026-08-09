# P7.1 调研 — 既有实现 eBPF 实现拆解与设计启示

> 调研人：DeepSeek V4 Flash
> 日期：2026-08-09
> 目的：P7.1（eBPF 事件源）写代码前的参考调研——前代工程 的
> aya-ebpf（内核态）+ ebpf_mode.rs（用户态）是 threadctl 前身的实战验证。
> 状态：调研完成，**未开始写代码**（按 boss 指示）

---

## 一、既有实现 eBPF 架构全景

```text
内核态（aya-ebpf，aya-ebpf 0.1，no_std，~180 行）：
  sched_process_fork tracepoint → 事件（child_pid + child_comm）
  sched_process_exec tracepoint → 事件（current pid/tgid + comm）
     ↓ 白名单过滤（TARGET_COMM_MAP：包名 8 字节片段，滑动窗口匹配）
     ↓ 防抖（DEDUP_MAP：每 pid 0.1s 窗口内最多 2 事件）
     ↓ 写入 EVENTS ringbuf（256KB）

用户态（既有实现/src/ebpf_mode.rs，aya 0.13.1，~400 行）：
  check_ebpf_support()  → BTF + 二进制存在检查
  try_init_ebpf()       → 加载 → attach 2 tracepoint → ringbuf → 消费线程
  reader 线程           → ring_buf.next() 阻塞，None 时 sleep 50ms
                          → mpsc channel → 主循环 handle_ebpf_event
  事件处理              → read_cmdline(pid) 匹配包名 → 进程缓存
                          → refresh_process_rules + 当前 tid apply
  失败回退              → 每步 eprintln + 回退 /proc 轮询

配置更新（periodic_full_scan）：清缓存 → 全量扫 /proc 发现已运行进程
周期刷新（refresh_cached_processes）：kill(pid,0) 清死进程 + 重扫
```

---

## 二、七个可复用设计（P7.1 采用）

### 1. 白名单内核态过滤（省 ringbuf/用户态压力）

- 内核 `TARGET_COMM_MAP: HashMap<[u8;8], u32>`，键 = 包名**前 8 字节 + 末 8 字节**
- 内核态对 comm 做滑动窗口（pos 0..=8）取 8 字节子串匹配——任意位置命中即上报
- 用户态 `build_target_entries()`：每个包名 1-2 个键，排序去重
- **价值**：非目标进程（系统/其他 app）的 fork/exec 全部内核态丢弃，ringbuf 只收
  白名单进程事件——这是高负载下 eBPF 可行的关键
- **P7.1 复用**：`TARGET_COMM_MAP` 机制保留；但 comm 匹配是"进程名"近似
  （15 字符裁剪），**用户态仍须 read_cmdline 精确匹配包名**（双保险）

### 2. 内核态防抖（DEDUP_MAP）

- `LruHashMap<u32, DedupEntry{last_ns, count}>`，0.1s 窗口每 pid 最多 2 事件
- **价值**：高频 fork 风暴（游戏引擎批量建线程）不冲击 ringbuf 与用户态
- **P7.1 复用**：保留；但注意——0.1s 窗口对 threadctl 的**线程 clone 分流**
  （ARCH-2）需调参：线程高频创建场景 2 事件/0.1s 可能不够，需校准

### 3. reader 线程 + mpsc 通道

- 独立线程阻塞 `ring_buf.next()`，None 时 sleep 50ms → mpsc → 主循环
- **价值**：与 threadctl 主循环结构兼容（主循环保持单线程可变状态所有权）
- **P7.1 复用**：完全一致——reader 线程发 mpsc，主循环 poll 消费
- **延迟现实**：50ms poll 间隔 → 事件发现延迟 ~50ms 级，**非亚毫秒**
  （印证 ChatGPT/Claude 三审结论：表述为"near-real-time 事件发现"）

### 4. 降级链（每步可回退）

```text
check_ebpf_support → try_init_ebpf → attach → ringbuf → reader
   ↓失败             ↓失败           ↓失败     ↓失败      ↓失败
 回退 proc          回退 proc      回退 proc  回退 proc  回退 proc
```

- 每步 eprintln 说明失败原因 + 回退——用户可诊断
- **P7.1 复用**：完全一致；threadctl 有 `CapabilitySet` 模式可挂 ebpf 探测

### 5. 进程缓存 + initial_scan_done 防抖

- `process_cache: HashMap<i32, ProcessScanState{initial_scan_done, last_scan_time}>`
- 首次事件全线程刷新（refresh_process_rules），后续按 sleep_interval 节流
- **P7.1 复用**：对应 threadctl 已有 tracker/THREAD_SCAN_TTL 机制，语义等价

### 6. EXEC 重置扫描状态

- `sched_process_exec` → `initial_scan_done=false` + 清 tid 缓存（exec 替换映像）
- **P7.1 复用**：对应 threadctl engine 的 `EventKind::Exec → tracker.remove`（M3 修复），
  语义一致

### 7. 配置更新全量扫描

- `periodic_full_scan`：配置更新时清缓存 + 全量扫 /proc（发现已运行的白名单进程）
- **P7.1 复用**：threadctl 热加载已有 `retain_interested + relock_all`，语义等价

---

## 三、五个 P7.1 差距/需改点

### 1. 版本：aya 0.13.1 / aya-ebpf 0.1（2023）→ 当前最新（2026）

- 既有实现 锁的是旧版 API：`#[map] static` 宏、`RingBuf::with_byte_size`、
  `bpf.program_mut(name)`、`loader.set_max_entries`
- 新版 aya（0.14+）API 变动：map 定义宏、RingBuf API、加载器接口都可能不同
- **P7.1 第 0 步必须确认**：当前 aya/aya-ebpf 最新版本 + 迁移差异
  （IMPL-1 构建链验证的一部分）

### 2. 缺少 sched_process_exit（threadctl 需要）

- 既有实现 靠 `refresh_cached_processes` 的 `kill(pid,0)` 周期清理死进程
- threadctl P7.1 要 `sched_process_exit` → 即时清理（IMPL-4：线程退出清
  applied_tids，修 TID 复用）——**新增 tracepoint，格式需确认**
  （sched_process_exit 参数：comm[16], pid, tgid, prio——pid 即退出任务 pid，
  线程退出时 pid=tid）

### 3. 无线程 clone 分流（threadctl ARCH-2 需要）

- 既有实现 把 `sched_process_fork` 的所有事件都当进程 fork（pid=child_pid）
- threadctl 需要 `tgid != pid → ThreadClone` 分流——**内核态判断**：
  fork tracepoint 参数只有 child_pid（无 tgid），需读 task 结构或
  用 `bpf_get_current_pid_tgid()`？——**sched_process_fork 触发时当前任务
  是父进程**（copy_process 在 fork 上下文中），current 是父。child 的 tgid
  无法直接从 tracepoint 参数拿——需内核态辅助：`bpf_probe_read_kernel`
  读 task_struct，或**用户态分流**（事件带 child_pid，用户态读
  /proc/<child_pid>/status 的 Tgid 判断？fork 时 /proc 可能未就绪）
  → **设计决策点**：内核态读 task_struct（复杂）vs 用户态延迟判断（pending
  退避后 /proc 就绪再读 Tgid，与 Zygote pending 天然合并）
  → 倾向：**用户态分流**（fork 事件进 pending 队列，cmdline 就绪后读
  Tgid：Tgid==Pid → Fork，Tgid!=Pid → ThreadClone）——复用现有 pending，
  零内核态复杂度

### 4. comm 匹配 vs 包名匹配的语义差异

- 既有实现 白名单匹配 comm（进程名，15 字符裁剪）
- threadctl 匹配 /proc cmdline（完整包名）
- **差异场景**：`com.ss.android.ugc.aweme`（23 字符）→ comm 被裁成
  `com.ss.android.ug`——8 字节滑动窗口能否命中？既有实现 的末 8 字节键
  （"ugc.aweme"）滑动窗口匹配 comm 的 `"droid.ugc.aweme"`？——**comm 与
  包名可能完全不同**（Android 进程名可以自由设置，如抖音主进程 comm
  = "droid.ugc.aweme"）
- **P7.1 结论**：内核态白名单用 comm 只是**粗过滤**（减少无关事件），
  精确匹配永远在用户态 read_cmdline（现有逻辑）；白名单键 = 配置包名的
  comm 近似（前 8 + 末 8）——无法覆盖 comm≠包名的场景 → 这些进程的
  fork 事件会丢（白名单过滤掉了）→ **补救**：定期全扫兜底（现有 TTL）
  + 白名单键**额外包含已知 comm 别名**（用户可配置）

### 5. 事件结构布局

- 既有实现 `ProcEvent{pid, tid, child_pid, comm[16], event_type}` `#[repr(C)]`
- threadctl P7.1 需要扩展：加 `tgid`（用户态分流用）或保持 + 用户态读
- 版本对齐：`#[repr(C)]` 布局必须内核/用户态一致（编译期校验或文档固化）

---

## 四、P7.1 设计修订（基于调研）

```text
内核态（threadctl-ebpf，新版 aya-ebpf）：
  sched_process_fork  → ForkRaw 事件（child_pid + child_comm + event_type）
  sched_process_exec  → ExecRaw 事件（pid + comm + event_type）
  sched_process_exit  → ExitRaw 事件（pid + event_type）   ← 新增
  白名单 TARGET_COMM_MAP（复用 既有实现 8 字节滑动窗口，粗过滤）
  防抖 DEDUP_MAP（复用，0.1s 窗口，参数可调）
  EVENTS ringbuf（256KB，复用）

用户态（threadctl daemon，EbpfSource 实现 EventSource trait）：
  reader 线程 → mpsc → 主循环
  fork 事件 → pending 队列（复用 Zygote pending）→ cmdline 就绪后
    读 /proc/<pid>/status Tgid：Tgid==Pid → Fork；Tgid!=Pid → ThreadClone
  exec 事件 → Exec（tracker.remove + refresh，现有语义）
  exit 事件 → 即时清 applied_tids / tracker（IMPL-4，修 TID 复用）
  精确匹配：用户态 read_cmdline（白名单 comm 只是粗过滤）
  降级链：每步失败 → 回退 ProcSource（复用 既有实现 模式）

构建链（P7.1 第 0 步）：
  aya/aya-ebpf 最新版 + bpf-linker + bpfel-unknown-none target + BTF 确认
```

**与 P7 规划书的关系**：本次调研确认规划书 A2/A3/A4 全部成立，并新增两个
**设计决策点**：
1. **线程分流在用户态做**（pending 后读 Tgid），非内核态——零内核复杂度，
   与 Zygote pending 合并（回应 ARCH-2，修订"内核态分流"表述）
2. **白名单 comm 是粗过滤 + 用户态 cmdline 精确匹配**（双保险），
   无法覆盖 comm≠包名的进程 → 全扫兜底（回应 IMPL-1 的 ringbuf 压力担忧）

---

## 五、风险确认清单（P7.1 第 0 步实测项）

| # | 风险 | 确认方式 | 影响 |
|---|---|---|---|
| 1 | 新版 aya/aya-ebpf API 迁移 | 查 crates.io 最新版 + 编译样例 | 高（代码量） |
| 2 | bpf-linker/LLVM 在 Termux 可装 | pkg install + 编译最小 .bpf.o | 高（阻塞） |
| 3 | SM8550 BTF 可用 | /sys/kernel/btf/vmlinux（root） | 高（阻塞） |
| 4 | sched_process_exit 格式 | 读 tracing/events format（root） | 中 |
| 5 | sched_process_fork 参数偏移（kernel 5.15 vs 新版） | 读 format 对照 | 中 |
| 6 | comm 匹配覆盖（抖音类 comm≠包名） | 真机观察 comm | 低（全扫兜底） |

---

## 六、结论

既有实现 eBPF 是 threadctl P7.1 的**成熟参照**：白名单粗过滤 + 防抖 + reader
线程 + 降级链七个模式直接复用；差异点（exit/线程分流/包名语义）已明确，
其中线程分流修订为**用户态 pending 后读 Tgid**（更简单可靠）。

**下一步（P7.1 第 0 步，未开始代码）**：
1. 确认新版 aya/aya-ebpf 可用性与 API 差异
2. Termux 装 bpf-linker 产出最小 .bpf.o
3. 确认 SM8550 BTF + 两个 tracepoint 格式
4. 通过后进入 EventSource trait 提取 + eBPF 实现
