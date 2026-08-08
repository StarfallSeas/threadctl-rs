---

# threadctl-rs Architecture Review Request

## Project

Repository:
https://github.com/StarfallSeas/threadctl-rs

Version:
v2.0.0

Language:
Rust

Target:
Android/Linux userspace thread controller

Status:
P6.1 matcher frozen, preparing P6.2 policy engine.

---

# Background

threadctl-rs 是一个 Android/Linux 用户态线程控制器。

目标不是替代 Linux scheduler，而是在用户空间提供：

- 线程级 CPU affinity 控制
- cpuset 控制
- scheduler policy 调整
- nice / priority 管理
- profile 模板
- 配置热加载
- 运行状态审计

设计原则：

> observe first, intervene carefully.

控制器只在明确配置和条件满足时调整线程行为，不主动与 kernel scheduler 对抗。

---

# Current Architecture

## Workspace

threadctl-rs

├── crates │ ├── core │   ├── config.rs │   ├── ruleset.rs │   ├── policy.rs │   ├── engine.rs │   ├── tracker.rs │   ├── decision.rs │   ├── system_context.rs │   ├── audit.rs │   └── topology.rs │ ├── daemon │   ├── main.rs │   └── proc_source.rs │ └── ebpf └── placeholder

---

# Current Data Flow

/proc events

|
  v

ProcSource

|
  v

Engine

|
  v

StateTracker

|
  v

RuleSet::resolve()

|
  v

Policy

|
  v

policy::apply_thread()

|
  v

kernel syscall

sched_setaffinity sched_setscheduler cpuset tasks

---

# P6.1 Matcher Design

## Rule resolution

Current model:

PackageMatcher

exact package | | wildcard package | | ThreadMatcher | | PolicyMerge

---

## Package matching

Supported:

com.tencent.mm

com.tencent.*

Wildcard priority:

Longest fixed prefix wins.

Internal scoring:

score = fixed_prefix_length * 100 + literal_characters

wildcard_count * 10

Example:

com.tencent.mm*

> 

com.tencent.*

> 

com.*

---

# RuleMatch model

Current implementation:

```rust
enum RuleSource {

    Global,

    Profile,

    Group,

    PackageWildcard,

    PackageExact,

    ThreadType,

    ThreadExact,
}


struct RuleMatch {

    index: usize,

    source: RuleSource,

}

Purpose:

Separate:

matching

priority

merging



---

Policy merge semantics

Current design:

CSS-like inheritance.

Example:

com.tencent.*

default:
    cluster = big


com.tencent.mm

RenderThread:
    sched = fifo

Result:

RenderThread:

cluster = big
sched   = fifo

Meaning:

Higher priority overrides fields, lower priority fills missing fields.


---

Profile system

Profile is not bound to package.

Example:

profile game

profile balanced

profile power-save

Application:

app com.example.game {

    profile game

}

Compiler expands profile into normal rules.


---

Cache

RuleSet owns cache:

HashMap<
    package_name,
    Vec<RuleMatch>
>

Properties:

instance scoped

destroyed on config reload

empty result cached

no global cache



---

Existing modules

policy.rs

Responsible:

affinity

cpuset

scheduler

nice


Features:

online CPU filtering

allowed CPU intersection

EPERM/EINVAL handling

audit recording



---

engine.rs

Responsible:

event processing

relock

cleanup

rule refresh



---

tracker.rs

Responsible:

PID reuse protection


Using:

/proc/<pid>/stat start_time

Also:

thread cache

cpuset reference counting



---

decision.rs

Current status:

Implemented but not fully connected.

Contains:

TaskIntent

ActionLevel

TaskScore

DecisionEngine

Current relock still relies on:

oom_adj

Future:

DecisionEngine
+
PressureLevel
+
Audit history


---

Questions for Review

Please review from a Linux scheduler / systems engineering perspective.


---

1. Matcher design

Is current model reasonable?

Especially:

exact + wildcard coexistence

field-level inheritance

CSS style merge

Potential problems:

hidden rule conflicts?

unexpected inheritance?

configuration complexity?



---

2. Policy engine

Should policy merging remain:

matcher -> merge -> apply

or should it become:

rule source
      |
      v
priority resolver
      |
      v
final policy


---

3. Freeze mechanism

threadctl-rs currently has no real freeze implementation.

Future consideration:

Possible actions:

SIGSTOP
cgroup freezer
Android freezer interface

Question:

Should a thread controller include freeze?

If yes:

what layer should own it?

policy layer?

decision layer?

separate freezer module?



---

4. Decision engine

Current design:

observe
suggest
force

Should automatic migration decisions be introduced?

Example:

memory pressure high

+
background app

+
long idle

=


move / restrict / freeze

Or should threadctl remain only a thread affinity controller?


---

5. Android specific concerns

Please review:

MIUI/HyperOS process behavior

LMKD interaction

cpuset conflicts

cgroup ownership

scheduler interference



---

6. Long term architecture

Would this architecture scale?

Possible future:

P6.2

Policy Merge Engine


P6.3

Config Compiler


P7

uclamp


P8

eBPF event source


---

Review Requirements

Please provide:

1. Architecture problems


2. Design risks


3. Suggested changes


4. Things that should NOT be changed


5. Kernel-level concerns



Avoid rewriting the project.

Focus on architecture review.

---

这个版本适合直接交给 Claude 和 DeepSeek。

重点做了几个调整：

1. **去掉“GPT 已经要求”的语气**
   - 让 Claude / DeepSeek 独立审查，而不是顺着已有结论。

2. **保留争议点**
   - freeze
   - decision engine
   - merge 继承
   - scheduler 干预边界

3. **避免 AI 互相吹捧**
   - 不写“由 Claude/ChatGPT/DeepSeek 联合开发”
   - Github 文档可以保留 AI review 过程，但架构审查文档应该像 Linux RFC。

4. **freeze 单独提出**
   - 因为线程控制器加入 freeze 是最大架构分叉点：
     - 如果只是 affinity controller，不需要 freeze。
     - 如果目标变成 Android 后台资源管理器，需要 freeze，但应该独立模块。

建议下一轮让 Claude 和 DeepSeek 分别回答这个文档，然后再进行一次“维护者视角审查”。