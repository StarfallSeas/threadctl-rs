# AI Review Process

threadctl-rs is developed through a human-adjudicated multi-AI workflow:
an implementation AI produces code, then architecture/review AIs cross-review
it before it freezes. This document records the process; the raw review logs
live in `docs/ChatGPT/` and `docs/Claude/` (raw AI review records),
with DeepSeek's design docs and responses/adoptions in `docs/DeepSeek/`.

## Roles

| AI | Role | Contribution |
|---|---|---|
| DeepSeek V4 Pro | Architect / Reviewer | Overall architecture, matcher constraints, Policy Merge Engine direction |
| DeepSeek V4 Flash | Lead developer | Implementation, tests, debugging, documentation |
| ChatGPT 5.5 | Architecture review | P5 must-haves (audit loop / multi-source intent / weight model), three-round matcher review, P6.2 direction |
| Claude Opus 4.x | Deep review | General review + Android-specific review (Bionic sysinfo.procs thread count, Zygote window, MIUI freezing, TASK_COMM_LEN) |

## Phase loop

```
P0 workspace → P1 ConfigStore → P2 proc pipeline → P5 five modules
→ P6.0 Profile → P6.1 Matcher (frozen) → P6.2 Policy Merge Engine (in progress)
```

Each phase: implement → write review doc → cross-AI review → fix → regression
→ freeze.

## Notable corrections from review

- `sysinfo.procs` counts **threads** on Bionic (not processes) → replaced with
  /proc directory counting (root-cures per-cycle full scans)
- Thread names >15 bytes get truncated by the kernel (TASK_COMM_LEN) → compile-time warning
- relock vs AMS/MIUI contention → skip background processes by oom_adj
- `RuleSource` priority made explicit (match table) instead of derived from
  enum declaration order
