ChatGPT 5.5 对 DeepSeek 架构审查回复的二次审查

整体评价：

DeepSeek 这次回复质量较高，已经从“代码审查”提升到了“项目边界审查”。大部分结论符合 threadctl-rs 当前定位。

我的判断：

> P6.1 可以冻结，P6.2 不应该继续扩大 matcher，而应该转向 Policy Engine 和 Android 生命周期边界。



不过其中有几个地方需要修正，否则后续开发容易出现架构漂移。


---

1. Matcher 设计

DeepSeek 结论

> sound, keep frozen



✅ 同意。

当前 threadctl-rs 最大风险不是 matcher 不够强，而是 matcher 继续膨胀。

目前已经形成：

Match

↓

RuleMatch

↓

PolicyMerge

↓

Policy

这个边界正确。

尤其这一点：

> matcher finds sources, merge decides



应该成为架构不变量。


---

关于 inherit false

DeepSeek:

> Add inherit false escape hatch in P6.2 only if real-world configs demand it.



这个建议合理，但是需要限制。

不要现在加入。

原因：

一旦加入：

thread "RenderThread" {
    cpus "7"
    inherit false
}

实际上引入：

inherit graph

配置复杂度会上升。

建议：

P6.2 不加入。

观察真实配置。

如果出现：

> 用户无法表达“不继承”



再加入。

当前默认：

thread override
+
package fallback

是 Android 用户最容易理解的模型。


---

2. Policy Merge Engine

DeepSeek：

> 当前已经是 priority resolver



✅ 正确。

但这里有一个重要补充。

现在：

RuleSource
    |
    v
merge_by_priority()

未来：

Global
Profile
Group
Package
Thread
Default

加入后，来源数量会增加。

如果继续：

if source == xxx {
}
else if source == yyy {
}

会快速腐化。

所以：

DeepSeek 提出的：

> data-driven merge table



非常重要。

建议 P6.2:

引入：

enum MergeStrategy {

    Override,

    FillMissing,

    BitOr,

    FirstWins,

}

例如：

PolicyField::Cpus

-> BitOr


PolicyField::Sched

-> Override


PolicyField::Nice

-> FirstWins

这样未来 profile/group 接入不会重构。


---

3. Freeze 设计

这是 DeepSeek 回复中最重要的部分。

结论：

> threadctl 不应该加入 freeze



基本同意。

原因：

thread controller 和 process freezer 是两个领域。

threadctl：

线程如何运行

freezer：

进程是否运行

这是不同生命周期模型。


---

Android 场景

Android 已经存在：

AMS
 |
ActivityManager
 |
LMKD
 |
cgroup freezer

如果 threadctl 做：

background detection

↓

freeze app

会进入：

threadctl
      VS
Android Framework

竞争。

这是危险方向。


---

SIGSTOP

DeepSeek：

> never SIGSTOP



完全正确。

SIGSTOP 最大问题：

它不是资源管理接口。

例如：

线程 A:

holds mutex

被 SIGSTOP。

线程 B:

waiting mutex

结果：

ANR。

所以：

如果未来真的做：

必须：

freezer crate

↓

cgroup.freeze

不能放：

policy.rs


---

4. Decision Engine

这里我认为 DeepSeek 判断非常准确。

当前 threadctl 不应该成为：

Android resource manager

不要做：

memory pressure

+

background

+

idle

=

kill/freeze/migrate

因为 Android 已经有：

LMKD
AMS
Scheduler
Thermal daemon

threadctl 做这些属于重复。


---

正确方向：

DecisionEngine:

不是：

决定杀谁

而是：

决定是否执行控制动作

例如：

正常：

pressure normal

apply affinity

压力：

critical

skip modification

这个方向正确。


---

5. Android 部分

DeepSeek 有几个关键点。

cpuset

> /dev/cpuset v1 still present



这里需要注意。

Android 未来版本逐渐迁移：

cgroup v2

所以：

当前：

v1 backend

可以。

但代码结构必须保持：

CpusetBackend trait

不要把：

/dev/cpuset

写死到核心。


---

Zygote fork window

DeepSeek 提到：

> pending queue 200ms



我认为需要调整。

200ms 对 Android 不一定够。

例如：

App:

zygote fork

↓

process init

↓

ActivityThread

↓

Application attach

可能超过。

建议：

不要固定 200ms。

设计：

PendingProcess {

 pid

 first_seen

 retry_count

 deadline

}

例如：

100ms
300ms
1s

指数退避。


---

6. uclamp

DeepSeek 提到：

> P7 uclamp largely done



这里需要谨慎。

如果源码实际已经：

Policy.uclamp_min/max

sched_setattr

那么属于：

执行层完成。

但是：

真正完整需要：

config

↓

profile

↓

merge

↓

capability

↓

apply

所以不能认为 P7 完成。

更准确：

> uclamp syscall path ready



而不是：

> uclamp feature finished




---

7. eBPF

完全同意：

EventSource trait

保持。

结构：

ProcSource

      |
      |

EbpfSource

      |

EventSource

这是正确的 Linux 风格。

不要：

core dependency aya

否则 Rust kernel tool 会被绑死。


---

最终审查结论

P6.1

状态：

✅ APPROVED / FROZEN

不再修改：

matcher

specificity

RuleMatch

cache

inheritance



---

P6.2 建议范围

只做：

1. Policy Merge Engine

目标：

从：

ruleset.rs

拆出：

merge.rs


---

2. DecisionEngine 接入

目标：

替换：

oom_adj > xxx

成为：

TaskIntent

+

SystemContext

+

Foreground

+

Audit

但输出：

allow / skip / degrade

不要输出：

kill/freeze


---

3. Backend 抽象

准备：

AffinityBackend

CpusetBackend

SchedBackend

为：

v1/v2 Android

迁移。


---

不建议做

未来版本禁止：

❌ userspace freezer

❌ 自动迁移 CPU

❌ 后台杀进程

❌ scheduler replacement

❌ 全局智能调度器


---

给 Claude / DeepSeek 的下一轮问题

建议发送：

> 请审查 threadctl-rs P6.2 Policy Merge Engine 设计，不再讨论 matcher。重点审查来源优先级矩阵、字段 merge 策略、DecisionEngine 接入边界。

