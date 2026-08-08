ChatGPT 5.5 最终架构确认：DeepSeek Adoption Review 审查

收到 DeepSeek V2 adoption 结果。

结论：

P6.1 正式冻结通过 ✅

当前 threadctl-rs 的架构方向已经从“功能堆叠”转向了比较稳定的 Linux 风格分层。

DeepSeek 本次修改基本全部接受，下面给出最终维护者视角确认。


---

1. P6.1 Freeze 状态

确认冻结：

RuleSet matcher

├── exact match
├── wildcard match
├── specificity
├── RuleMatch
├── source tracking
├── package inheritance
└── instance cache

不再修改。

原因：

继续增强 matcher 会导致：

matcher
    |
    |
    policy decision

边界模糊。

现在：

Matcher
    |
    | 产生候选来源
    v

Policy Merge
    |
    | 决定最终字段
    v

Policy

职责清晰。


---

2. inherit false

接受延期。

DeepSeek 处理正确。

当前不要加入：

inherit false

原因：

它不是一个简单开关，而会引入：

inheritance graph

例如：

profile
   ↓
package
   ↓
thread
   ↓
thread-type

一旦允许中断继承，需要定义：

阻断哪一层？

阻断全部还是部分字段？

profile 是否继续继承？

group 是否绕过？


P6.2 不处理。

保留未来需求。


---

3. merge.rs 独立

这是 P6.2 最核心变化。

当前：

ruleset.rs

matcher
+
merge
+
policy resolve

逐渐过重。

拆：

ruleset.rs

负责:

match
source collection


        ↓


merge.rs

负责:

priority
field merge


        ↓


policy.rs

负责:

kernel operation

最终结构：

config compiler

        ↓

RuleSource[]

        ↓

RuleMatcher

        ↓

PolicyMergeEngine

        ↓

EffectivePolicy

        ↓

Backend

这是更接近 Linux subsystem 的设计。


---

4. MergeStrategy

接受。

建议 P6.2 定义：

enum MergeStrategy {

    Override,

    FillMissing,

    BitOr,

    FirstWins,

    Min,

    Max,

}

虽然当前只需要：

字段	策略

cpus	BitOr
sched	Override
nice	FirstWins
uclamp_min	Max
uclamp_max	Min


但是保留扩展能力。

原因：

uclamp 与 scheduler 参数天然不是同一种合并模型。

例如：

uclamp_min

10 + 50

不能 OR

应该：

max(10,50)


---

5. uclamp 修复

这是一个正确的立即修复。

之前风险：

配置：

uclamp_min = 512

但是：

kernel:

CONFIG_UCLAMP_TASK=n

结果：

用户认为生效。

实际：

无效果。

现在：

startup probe

        ↓

capability

        ↓

apply_uclamp

符合 Linux 工具设计。

类似：

cpupower

perf

turbostat

不会假装支持不存在的 kernel feature。


---

6. Backend 抽象

接受。

但是提醒：

不要过早抽象。

建议：

P6.2 只定义 trait：

trait CpuBackend {

    apply_affinity()

}


trait CpusetBackend {

    attach()

}


trait SchedulerBackend {

    set_policy()

}

暂时实现：

AndroidV1Backend

即可。

不要马上实现：

AndroidV2Backend

否则：

trait
+
empty implementation

会增加维护成本。


---

7. DecisionEngine 边界

确认。

最终定位：

DecisionEngine 不是调度器

不是：

AI scheduler

不是：

userspace CFS

不是：

LMKD replacement

而是：

control gate

输入：

TaskIntent

SystemContext

Foreground

Audit

输出：

Allow

Skip

Degrade

例如：

压力正常：

apply affinity

压力严重：

skip relock

这符合 Linux 哲学：

> Don't fight the kernel.




---

8. Freeze 决策最终确认

维持：

threadctl-rs
=
thread controller

不是：

Android process manager

禁止：

SIGSTOP freezer

kill daemon

background killer

auto migration

未来如果需要：

独立：

android-freezer

项目。

不要污染 threadctl。


---

9. 当前架构状态

最终：

Config

                    |
                    v

             Config Compiler

                    |
                    v

              RuleSource[]

                    |
                    v

              Rule Matcher
              (P6.1 frozen)

                    |
                    v

          Policy Merge Engine
               (P6.2)

                    |
                    v

            Decision Engine

                    |
                    v

             Backend Layer

                    |
                    v

              Linux Kernel


---

10. P6.2 开发优先级建议

排序：

P6.2-1

Policy Merge Engine

优先级：

★★★★★

原因：

所有未来能力依赖：

profile

group

uclamp

default



---

P6.2-2

DecisionEngine 接线

优先级：

★★★★☆

替换：

if oom_adj > xxx

为：

decision.evaluate()


---

P6.2-3

Backend trait

优先级：

★★★☆☆

先接口，不扩展实现。


---

P6.2-4

PendingProcess

优先级：

★★★☆☆

解决：

zygote 空窗。


---

最终批准

Architecture Review Status:

P6.1 Matcher        APPROVED
P6.1 RuleSource     APPROVED
P6.1 Cache          APPROVED
P6.1 Inheritance    APPROVED

P6.2 Direction      APPROVED

Freeze feature      REJECTED (scope boundary)

Scheduler AI        REJECTED (scope boundary)

Userspace LMKD      REJECTED (scope boundary)

threadctl-rs 当前最重要的优势：

它没有试图成为 Android 智能调度器，而是保持为一个可预测、低开销、可审计的 Linux 线程控制工具。

下一阶段重点应该从“增加能力”转向“建立可靠的 Policy Merge 和执行边界”。