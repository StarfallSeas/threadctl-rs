# Claude P6.1 深度审查 — 修复核查报告（供 Claude 再审）

> 核查日期：2026-08-08
> 核查对象：Claude-P6.1.md 全部发现项（NEW-H1/H2/M1-M4 + DESIGN-1/2/3 + L1-L5 + Q1-Q5）
> 核查方式：逐项对照源码验证 + 回归测试 + 全矩阵一致性测试
> 核查结果：48 单测全绿（+2 新增回归测试）、零警告、release 855KB

---

## 一、修复项核查结论（逐项）

### ✅ NEW-H2 — uclamp 全链路（最高严重度，已修复 + 回归测试）

**核查**：`Policy` 新增 `uclamp_min/max` 字段；`ruleset.rs` compile 传递 + merge_by_priority 合并 + fill_missing 填充；`apply_uclamp()` 用 `sched_setattr(SCHED_FLAG_UTIL_CLAMP)` 执行，ESRCH 更新 outcome，失败 warn_once + audit。

**新增回归测试**：`uclamp_flows_through_resolve`（config → compile → resolve 全链路断言 uclamp 不丢）。

**注意点**：`apply_uclamp` 失败不改变主 outcome（亲和性已生效），仅 audit 记录——与亲和性失败严格区分。

### ✅ NEW-H1 — decide/evaluate 不一致（已修复，且比建议更进一步）

**核查**：第一轮只改了 BLS weight 30→50，但核查时发现**残余矛盾**：`decide(Interactive, Critical)` 仍返回 Steer（decide 无压力感知）而 evaluate 返回 Observe。

**终版修复**：按 Claude"只保留一条决策路径"建议，`decide()` 内部直接调用 `evaluate(intent, pressure, 0.0).to_action(false)`——两条路径**结构性等价**，不再可能漂移。

**语义变化**（均为压力感知的正确行为，已更新旧测试）：
- Interactive + Moderate/Critical → Observe（压力降级）
- Background 任何压力 → Observe（权重 10 < 阈值 40，与 relock 后台跳过语义一致）

**新增回归测试**：`decide_matches_evaluate_full_matrix`（4 intent × 3 pressure 全矩阵断言一致）。

### ✅ NEW-M1 — apply_sched 返回值（已修复）

`apply_sched` 返回 `Option<ApplyOutcome>`：ESRCH → Exited；失败 → warn_once + audit（`sched_failed`/`nice_failed`）；setpriority 失败不覆盖主 outcome。

### ✅ NEW-M2 — Downgraded 补 setaffinity（已修复）

交集缩减分支在写 audit 前补调 `effective.set_affinity(tid)`——线程 affinity 与 audit 记录一致，下轮 getaffinity 短路可命中。

### ✅ NEW-M3 — Exec cpuset 目录泄漏（已修复）

Exec 分支改为 `tracker.remove(pid)`（释放全部旧 applied_dirs 引用，归零即 rmdir），refresh 内部 `enter()` 重建状态。

### ✅ DESIGN-3 — kill(pid,0) EPERM 误判（已修复）

新增 `proc::is_alive()`（EPERM = 存活），替换 engine.rs relock_all/cleanup_dead + proc_source.rs 增量路径全部 kill 检查。

### ✅ Q3 — rmdir 后 ensure 缓存残留（已修复）

`policy::forget_cpuset_dir()`（pub(crate)），tracker 回收目录成功后同步清除 `ENSURED_CPUSET_DIRS`。

### ✅ NEW-L2 — nice=0 静默丢弃（已修复）

`unwrap_or(0)==0` → `is_none()`，显式 `nice = 0`（重置为默认）不再被丢弃。

### ✅ NEW-L4 — examples lock_interval 不一致（已修复）

examples/threadctl.toml 5→60，与内嵌默认模板一致。

### ⚠️ NEW-L1 — 核实为误报（已注释澄清，未改行为）

Claude 认为 `inotify_rm_watch` 第二参应为 i32。**核实**：当前所用 libc crate（termux 环境实际版本）签名即 `u32`。原 `as u32` 正确，注释已说明验证过程。

### ⏳ 记录为 P6.2 输入（未修，附理由）

| 项 | 状态 | 理由 |
|---|---|---|
| NEW-M4 TID 复用漏检 | ⏳ P6.2 | 需 TTL 内缩短全扫或 tid_names 失效标记，涉及增量路径设计 |
| DESIGN-1 from_sources 接入 relock | ⏳ P6.2 | P6.2 第一步（已列入路线图，需 pid→uid 映射） |
| DESIGN-2 Zygote 空窗链条 | ⏳ P6.2 | pending 队列 200ms 重读（已列入路线图） |
| Q1 `inherit false` 标记 | ⏳ P6.2 | KDL 语法扩展 + resolve 跳过 fill_missing |
| Q4 cluster fallback 写 audit | ⏳ P6.2 | 需 audit reason 扩展 + summary 展示 |
| Q5 audit per-pkg 统计 | ⏳ P6.2 | `AuditSummary.top_blocked_pkgs` 或实例级 HashMap |
| NEW-L3 base_cpuset_fd → bool | ⏳ 低优先级 | 一个 fd 的占用，无功能影响 |
| NEW-L5 VecDeque | ⏳ 低优先级 | 256 元素 O(n) 移位可接受 |

---

## 二、核查中发现的新问题（Claude 未覆盖）

### 🔍 NEW-F1 — decide 双路径残余矛盾（已随 H1 终版修复）

第一轮修复（BLS 30→50）后核查发现：`decide(Interactive, Critical)` 与 `evaluate` 仍不一致。根因是 decide 的 match 表无压力感知。终版统一为 evaluate 单路径，从结构上消除此类问题。

### 🔍 NEW-F2 — 旧测试断言与压力感知语义冲突（已更新）

`interactive_always_steer` / `background_downgrades_under_pressure` 断言的是旧 decide 语义（无压力感知）。新语义下已重写为 `interactive_steers_until_pressure` / `background_observes`。这暴露了"测试锁定旧行为"的风险——Claude 原报告提到的 `task_score_sums 未覆盖 to_action` 同类问题。

---

## 三、待 Claude 再审的问题

1. **H1 终版语义确认**：decide 统一走 evaluate 后，Interactive 在 Moderate 压力即降级 Observe（50-15=35 < 40）。若产品预期"前台应用始终干预"，需调阈值或权重——请确认压力感知降级是期望行为。
2. **M1 的 BlockedByPerm 归因**：sched_setscheduler 失败统一归 `BlockedByPerm`，但 EINVAL（如无效 policy 参数）也被归入。是否需要区分 errno 映射？
3. **M3 的 remove 重建代价**：Exec 走 remove+enter 重建（重新 read_start_time），Zygote 高频 exec 场景有轻微 I/O 增加。可接受还是需要专用 `reset_dirs(pid)`？
4. **L1 核实结论**：libc crate 签名确认 u32。若 Claude 基于不同 libc 版本审查，请指出版本差异。
5. **P6.2 优先级**：M4（TID 复用）与 DESIGN-1（from_sources 接入）哪个优先？当前路线图把 DESIGN-1 列为 P6.2 第一步。

---

## 四、测试增量

| 新增测试 | 覆盖 |
|---|---|
| `uclamp_flows_through_resolve` | NEW-H2 全链路回归 |
| `decide_matches_evaluate_full_matrix` | NEW-H1 双路径一致性（4×3 矩阵） |
| 重写 `interactive_steers_until_pressure` / `background_observes` | 新 decide 语义 |
