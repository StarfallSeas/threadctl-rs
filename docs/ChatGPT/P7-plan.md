threadctl-rs P7 规划书审查结果（文档/架构方向）

总体结论

状态：APPROVED WITH REVISION

P7 方向整体符合 threadctl-rs 当前定位：

> 用户态 Task Policy Controller
而不是 scheduler / LMKD / freezer / resource manager。



P6 阶段确定的边界没有被破坏。

但是当前 P7 文档存在几个架构叙事和优先级问题，需要在公开文档中修正，否则容易让项目定位产生偏移。


---

1. P7 总方向审查

A. eBPF 事件源

结论：

✅ 正确，应该作为 P7 主线。

原因：

当前 ProcSource：

/proc scan
↓
发现变化
↓
apply policy

本质是 polling model。

eBPF：

kernel event
↓
ringbuf
↓
EventSource
↓
engine

符合当前架构。

尤其：

EventSource trait
ProcessEvent
engine::handle_events()

这个抽象已经提前准备。

无需改变核心逻辑。

建议修改

文档中：

> 事件延迟 2s → 亚毫秒



建议降低宣传强度。

改为：

reduce event discovery latency from polling interval scale
to near-real-time notification when kernel support exists

原因：

ringbuf 到用户态不是严格实时保证。


---

2. eBPF tracepoint选择

当前：

sched_process_fork
sched_process_exec
sched_process_exit
sched_switch

审查：

fork/exec/exit

✅ 合理。

这是 threadctl 最需要的事件。

sched_switch

⚠️ P7 后期，不建议作为核心路线。

原因：

sched_switch 数据量非常高。

Android 高负载：

可能：

几十万 events/sec

即使只观察：

prev_pid
cpu
next_pid

也需要过滤。

否则：

ringbuf 压力

用户态消费压力

电量影响


建议：

保持：

P7.5 experimental

不要进入默认功能。


---

3. B1 自适应 relock

结论：

✅ 高价值。

这是目前 P7 最实际的增强。

但是：

当前描述：

覆盖频繁 → 60s → 10s → 3s
稳定 → 300s

需要限制。

原因：

relock 本质：

用户态和 Android framework 的竞争。

如果算法过于激进：

可能出现：

AMS修改cpuset
↓
threadctl检测
↓
threadctl修改
↓
AMS修改

形成震荡。

建议增加：

cooldown period
minimum observation window

例如：

首次发现覆盖:
进入 aggressive mode

持续观察30s

确认持续覆盖后缩短周期

不要单次失败立即调整。


---

4. B2 DVFS Domain

结论：

✅ 保留。

但价值定位正确：

它不是优化器。

应该定位：

validation / warning layer

不是：

automatic topology optimizer

当前：

> 不改变用户配置



这个决定正确。


---

5. B3 DecisionEngine

结论：

✅ 必须做。

但是需要保持 P6 定义：

DecisionEngine:

Allow
Skip
Degrade

不要扩展：

Move
Boost
Balance
Optimize

否则会逐渐变成 scheduler。

thermal：

建议：

只作为：

degrade signal

不要参与：

CPU选择。


---

6. C1 IPC CLI

结论：

✅ P7 高价值。

实际上可能比 eBPF 更影响用户体验。

原因：

当前 threadctl：

调试成本高。

需要：

threadctl status

threadctl dump pid

threadctl reload

这是成熟 Linux daemon 必备能力。

建议：

优先级提升。

推荐：

P7.1

或者与 eBPF 并行。


---

7. C2 dry-run

结论：

✅ 强烈建议。

成本低。

收益高。

尤其当前：

配置系统复杂：

profile
group
thread
thread-type
wildcard
inherit
merge

用户需要看到：

最终展开结果。

建议：

输出：

input rule
matched source
merged policy

例如：

com.tencent.mm
 ├ wildcard: com.tencent.*
 ├ exact: com.tencent.mm
 └ final:
     cpus=0-3
     sched=fifo


---

8. C3 CapabilitySet

结论：

✅ 正确。

当前：

能力检测分散：

cpuset
uclamp
RT
ebpf

统一后：

启动日志更清晰。

但是：

不要过度抽象。

保持：

CapabilitySet
    |
    +-- probe

不要：

Capability framework
plugin system


---

9. D1 屏幕状态

结论：

⚠️ 中等价值。

问题：

Android 上：

屏幕状态不是线程策略强相关因素。

更适合作为：

DecisionEngine 输入。

不要直接：

screen off → 修改策略

应该：

ScreenState
↓
TaskIntent
↓
DecisionEngine
↓
Allow/Skip/Degrade


---

10. D2 冻结感知

结论：

⚠️ 谨慎。

文档中：

> 冻结进程感知



不会违反之前 freeze non-goal。

但是容易让用户误解。

建议改名：

external lifecycle state detection

说明：

不是冻结功能。

只是：

detect externally frozen task
avoid useless operations


---

11. 最大风险：P7 范围膨胀

当前 P7：

包含：

eBPF

relock算法

decision

IPC

dry-run

screen

freezer

DVFS

tracing


对于一个 5k~10k 行 Rust 项目：

偏大。

建议拆：

P7 Core

A eBPF
C IPC
B1 relock


P8

DecisionEngine
Android integration

否则版本周期会过长。


---

12. AI 项目文档角度

当前 P7 文档 AI 痕迹：

中等。

优点：

结构清晰

trade-off 明确

non-goal 明确


问题：

部分段落过于 AI RFC 风格：

例如：

高价值
★★★★★
必须
唯一主线

公开仓库建议减少。

内部：

DeepSeek/ChatGPT/Claude 文档可以保留。

README：

不要出现。


---

最终建议

保留

✅ eBPF EventSource
✅ Proc fallback
✅ IPC CLI
✅ dry-run
✅ adaptive relock
✅ DecisionEngine gate

延后

⏸ sched_switch observation
⏸ screen state
⏸ freezer detection
⏸ idle core heuristic

修改

1. 降低 eBPF 性能宣传


2. adaptive relock 增加防震荡机制


3. DecisionEngine 保持 gate，不进入优化器


4. 冻结感知改名


5. P7 拆分范围




---

Final Verdict

P7 architecture:
APPROVED

Implementation:
Proceed with staged delivery

Recommended first milestone:

P7.1:
- eBPF fork/exec/exit EventSource
- ProcSource fallback
- IPC CLI
- dry-run

Do not add:
- scheduler intelligence
- migration logic
- freezer logic