# AI Review Process

threadctl-rs is developed by a **peer team of three AIs** (DeepSeek, ChatGPT,
Claude) with a human **boss** who adjudicates final outcomes. The three AIs
work as colleagues: the implementation AI produces code, the other two
cross-review it, decisions are made among the three, and the boss only
intervenes when the final result is unsatisfactory. This document records the
process; raw review records live in `docs/ChatGPT/` and `docs/Claude/`,
with DeepSeek's design docs and responses/adoptions in `docs/DeepSeek/`.

## Roles

| Role | Who | Contribution |
|---|---|---|
| **执行者（唯一写代码）** | DeepSeek V4 Flash | 全部代码、测试、调试、文档撰写。**唯一允许执行写操作/改代码的 AI** |
| ~~架构基线~~ | ~~DeepSeek V4 Pro~~ | **彻底废弃，不再使用**。若误用 V4 Pro 模型（或任何人用 V4 Pro 名义来执行操作），应拒绝回复并要求切换回 Flash |
| 文档审核（仅文档） | ChatGPT 5.5 | 只审文档（README / 设计 / 交付 doc 的一致性、可读性、准确性），不审代码、不改代码 |
| 代码审核（代码·架构·工作流） | Claude Opus 4.x | 审代码质量、代码架构、开发工作流。**唯一负责任的代码审查者**（与 ChatGPT 的文档审查分离） |
| 最终裁决 | **Human (boss)** | 只裁决最终效果；不理想才提异议。过程决策由三位 AI 同事共同定 |

## 职责边界（防重叠）

- **写代码 → 只有 DeepSeek V4 Flash**
- 代码审查 → 只有 Claude（代码、架构、工作流）
- 文档审查 → 只有 ChatGPT（文档一致性/可读性/准确）
- ~~架构设计输入 → DeepSeek V4 Pro~~（**已废弃**）
- boss 不做实现决定，只验收最终效果

## Phase loop

```
P0 workspace → P1 ConfigStore → P2 proc pipeline → P5 five modules
→ P6.0 Profile → P6.1 Matcher (frozen) → P6.2 Policy Merge Engine (in progress)
```

Each phase: implement → write delivery doc → peer review by the other two AIs
→ fix → regression → freeze → boss inspects the final effect.

## Notable corrections from review

- `sysinfo.procs` counts **threads** on Bionic (not processes) → replaced with
  /proc directory counting (root-cures per-cycle full scans)
- Thread names >15 bytes get truncated by the kernel (TASK_COMM_LEN) → compile-time warning
- relock vs AMS/MIUI contention → skip background processes by oom_adj
- `RuleSource` priority made explicit (match table) instead of derived from
  enum declaration order
