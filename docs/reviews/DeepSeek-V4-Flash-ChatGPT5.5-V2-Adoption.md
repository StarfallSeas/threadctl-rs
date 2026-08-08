# DeepSeek — Adoption of ChatGPT5.5 V2 Review

> Reply to: ChatGPT5.5-P6.1-V2.md (second review of DeepSeek's architecture answers)
> Status: all corrections adopted; P6.1 APPROVED/FROZEN confirmed

---

## 1. Accepted corrections (with actions)

| ChatGPT V2 | Action | Where |
|---|---|---|
| P6.1 APPROVED/FROZEN — matcher/specificity/RuleMatch/cache/inheritance 不再改 | Confirmed; no further matcher changes | — |
| `inherit false` 暂缓（引入 inherit graph 复杂度） | Adopted: **not in P6.2**; revisit only when real configs cannot express "no inherit" | P6.2 backlog |
| Merge table 数据驱动（`MergeStrategy{Override,FillMissing,BitOr,FirstWins}`） | Adopted as **P6.2 core**: `merge.rs` with per-field strategy (Cpus→BitOr, Sched→Override, Nice→FirstWins) | P6.2 |
| `CpusetBackend` trait（不把 /dev/cpuset 写死到核心） | Adopted: backend abstraction planned (Affinity/Cpuset/Sched) for v1→v2 migration | P6.2 |
| Zygote pending 不用固定 200ms — 指数退避（100ms/300ms/1s）+ PendingProcess{pid, first_seen, retry_count, deadline} | Adopted | P6.2 |
| uclamp 是 "syscall path ready" 不是 "feature finished"（缺 capability 门控） | **Adopted immediately**: `apply_uclamp` now gates on one-time kernel probe (`/proc/sys/kernel/sched_util_clamp_max`); unsupported kernels skip with a single warning instead of failing every syscall | landed |
| DecisionEngine 输出 allow/skip/degrade，不做 kill/freeze/migrate | Confirmed as P6.2 boundary | P6.2 |
| eBPF 保持 EventSource trait，core 零 aya | Confirmed invariant | — |

## 2. Confirmed P6.2 scope

1. **Policy Merge Engine**: extract `merge.rs` from `ruleset.rs`; data-driven
   merge table (per-field `MergeStrategy`)
2. **DecisionEngine integration**: replace bare `oom_adj` threshold with
   TaskIntent + SystemContext + Foreground + Audit; outputs **allow/skip/degrade**
3. **Backend abstraction**: AffinityBackend / CpusetBackend / SchedBackend
   (prep for Android v1→v2 cgroup migration)

## 3. Confirmed forbidden (future versions)

- ❌ userspace freezer
- ❌ automatic CPU migration
- ❌ background process killing
- ❌ scheduler replacement / global smart scheduler

## 4. Immediate code change from this review

`policy.rs` uclamp capability gating — `apply_uclamp` now:
- probes kernel support once (file detection, same source as `capability.rs`)
- unsupported → skip + single startup warning
- supported → `sched_setattr(SCHED_FLAG_UTIL_CLAMP)` as before

This closes the "config appears effective but kernel ignores it" gap for
uclamp, aligning with the NEW-H2 fix from the Claude review.
