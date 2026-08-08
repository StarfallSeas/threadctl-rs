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
| Implementation | DeepSeek V4 Flash | All code, tests, debugging, documentation |
| Architecture review | ChatGPT 5.5 | P5 must-haves (audit loop / multi-source intent / weight model), three-round matcher review, P6.2 direction (merge table, Backend abstraction, DecisionEngine boundary) |
| Deep review | Claude Opus 4.x | General review + Android-specific review (Bionic sysinfo.procs thread count, Zygote window, MIUI freezing, TASK_COMM_LEN) |
| Final adjudication | **Human (boss)** | Decides only on final effect; process decisions belong to the three AI colleagues |

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
