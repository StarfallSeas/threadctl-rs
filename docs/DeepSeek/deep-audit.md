# 深度审查报告 — bug / 代码质量 / 框架质量

> Author: DeepSeek V4 Flash（自审）
> Date: 2026-08-12 · 107 测试全绿零警告
> 方法：危险模式扫描（unwrap/unsafe/clone/panic 生产路径）→ 逐点深入 → 修复 → 复测

---

## 一、Bug 修复（本轮 4 项）

### 🔴 BUG-1 `observe::SnapshotWindow` 统计泄漏（内存，长期）

`stats` / `recent` HashMap **只增不减**——进程/线程退出后旧 tid 统计永久保留。8 包真实负载、进程频繁重启场景（微信/抖音子进程常驻短生命周期）下 tid 无限累积 → 慢泄漏。

**修复**：`SnapshotWindow::retain(&HashSet<i32>)` 清理；daemon 与 cleanup_dead 同周期（15s）收集存活 tid 同步窗口（线程退出但进程存活也覆盖——不依赖 removed>0）。

### 🟠 BUG-2 `ipc.rs` read_line 无长度上限（防御性）

socket 客户端发无限长行 → `read_line` 无限分配 → 内存膨胀。socket 虽 0750 root-only（信任面小），但防御性缺失。

**修复**：`reader.take(4096)` 行上限。

### 🟠 BUG-3 `ipc.rs` try_clone().expect 生产路径 panic

fd 耗尽时 `try_clone` 失败 → expect panic → **监听线程死**（daemon 剩主循环，IPC 静默失效）。

**修复**：`let-else continue`（连接丢弃，监听线程存活）。

### 🟠 重构 tracker::enter 三处 get_mut().unwrap()

`self.procs.get_mut(&pid).unwrap()` ×3 逻辑安全但维护脆弱（任何分支重排即 panic）。且重构时发现**借用冲突**（持可变引用时 remove_entry）。

**修复**：先只读判断（分支 0/1/2/None），再操作——无借用冲突，unwrap 收敛为受控 expect（"存在"/"刚插入"，均有先验保证）。

---

## 二、已知限制（记录，不改）

| # | 项 | 性质 |
|---|---|---|
| L1 | `audit::summary_windowed(60)` 在 256 条环形容量下，高频事件时 60s 窗口被覆盖 → 统计低估 | 设计限制（容量固定）；audit 全 success 场景无影响 |
| L2 | `topology.rs:113` `end.unwrap()+1`——guard 有 `Some(_)` 保证；CPU 编号不可能 usize::MAX | 理论溢出，实际不可达 |
| L3 | P8 采样 5s——毫秒级迁移在采样间隔内不可见（迁移统计是"采样可见迁移"） | 观测精度声明（P8 文档已注明） |

---

## 三、框架质量评价

### ✅ 强项

| 维度 | 评价 |
|---|---|
| 模块分层 | core（纯逻辑零 aya 依赖）→ daemon（事件源/主循环）→ ebpf（内核态）——单测友好、依赖单向 |
| unsafe | 34 处全在 topology（libc syscall 封装），集中可控，无散落 |
| 错误处理 | Result/Option 为主；unwrap 收敛在受控路径（CString 常量、逻辑先验）；IPC/网络路径本次已全部 let-else |
| 并发模型 | 主循环单线程可变状态所有权 + 辅助线程 mpsc（hot-reload/IPC/eBPF reader）——无共享锁竞争（tracker Mutex 仅事件循环内部短持） |
| 测试 | 107 个，覆盖：解析/匹配/合并/relock 状态机/窗口统计/PID 复用/白名单命令——行为级断言 |
| 热路径 | debug_log 惰性（AtomicBool 单读零开销）；relock getaffinity 短路；P8 采样缓冲复用；主循环 sleep 防空转 |

### ⚠️ 改进项（不阻塞）

1. **中文注释混杂**：日志已英文化，但代码注释仍中英混用——长期维护建议统一（项目定位国际开源）
2. **SnapshotWindow retain 每 15s 全量遍历 tracker**（锁 + 100+ 进程扫描）——开销微秒级可接受，但可优化为增量（只清 removed 的 tid 集合）
3. **engine.rs:69 每次事件 pkg Option<String> clone**（~24B/事件）——事件风暴时可复用缓冲，当前量级可接受

---

## 四、复测结果

```
cargo test --workspace --exclude threadctl-ebpf → 107 passed（96 core + 11 daemon）
cargo check --workspace --exclude threadctl-ebpf → 0 warning 0 error
```

---

## 五、给 Claude / ChatGPT 的补充审查点

1. **BUG-1 retain 方案**：15s 全量存活 tid 收集 + retain——与 cleanup 耦合是否够？是否有更优（引用计数/增量）？
2. **tracker enter 重构**：分支判断（先读后写）模式是否比原 get_mut unwrap 更清晰？`Some(_) => unreachable` 兜底是否可接受？
3. **ipc 防御**：4096 行上限 + try_clone let-else——是否还有 IPC 层其他攻击面（并发连接耗尽 accept 循环？）
4. **audit L1**：60s 窗口低估是否需要解决（环形 256 → 1024？）还是保持设计限制？
