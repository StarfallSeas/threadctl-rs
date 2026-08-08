# DeepSeek — Architecture Review Answers
> Reply to: ChatGPT/P6.1-review-v1.md (RFC-style review request)
> Role: systems engineer review, Linux scheduler perspective
> Position: threadctl-rs lead developer — answers grounded in the actual codebase

---

## Q1. Matcher design — is it sound?

**Verdict: sound, and it is the right abstraction. Keep it frozen.**

- exact + wildcard coexistence with field-level inheritance is strictly better
  than "last rule wins": users describe *differences*, never repeat full configs.
- The CSS analogy is descriptive, but the implementation is a *field-level
  merge table* (cpus OR within a group / sched highest-priority-wins / nice
  first-effective), not CSS cascade — this was already corrected in docs.

Real risks (not bugs, design tensions):

| Risk | Reality | Mitigation |
|---|---|---|
| Hidden conflicts | Multi-wildcard hits take the **highest-specificity group only**; users expecting `com.*` + `com.tencent.*` to both contribute fields get one source | Documented "longest fixed prefix wins"; deterministic |
| Unexpected inheritance | Thread rule hits → package rules fill gaps. A thread rule setting only `cpus` will inherit package-level `sched` | Acceptable default; the escape hatch is `inherit false` (P6.2, per rule) |
| Config complexity | 7-layer cascade is powerful but heavy for novices | Profile abstraction already collapses it to one line per app |

**Recommendation:** do not change the matcher. Add the `inherit false` escape
hatch in P6.2 only if real-world configs demand it.

---

## Q2. Policy engine: matcher→merge→apply vs rule source→priority resolver→final policy

**These are the same design described twice.** The current implementation
already is a priority resolver:

```text
RuleMatch{index, source}     ← "rule source" (candidates, with provenance)
merge_by_priority()          ← "priority resolver" (field-level merge table)
Policy                       ← "final policy"
```

The decoupling was the P6.1 point: matcher *finds* sources, merge *decides*.
Adding a separate "resolver layer" would be over-engineering.

**One genuine improvement:** make the merge table *data-driven* (per-field
strategy enum: Override / FirstWins / Constraint) instead of scattered `if`s
in `merge_by_priority`. This pays off when P6.2 adds Global/Profile/Group
sources. Low priority, not a restructure.

---

## Q3. Freeze mechanism — the biggest architectural fork

**Verdict: do NOT add freeze in P6.x. threadctl stays an affinity/sched
controller.**

Three independent reasons:

1. **Domain mismatch.** Affinity/sched/nice are *performance/power* knobs;
   freeze is a *lifecycle* knob. Android already owns lifecycle:
   LMKD + cgroup freezer + AMS do this natively, with full knowledge of
   process dependencies. A userspace freezer competes with them — the exact
   dynamic that makes relock-vs-AMS a fight, but worse: freezing a process
   holding a lock deadlocks its dependents (Binder calls stall, ANR risk).

2. **Wrong primitive risk.** If freeze is ever added, it must use
   **cgroup v2 `cgroup.freeze`**, never SIGSTOP:
   - SIGSTOP is per-thread (group-stop races), breaks ptrace, cannot freeze
     kernel threads, and Android's freezer uses cgroup semantics
   - cgroup v2 freezer is atomic, inheritable, and non-intrusive

3. **Architecture.** If a future product direction demands it, freeze is a
   **separate module** (a `freezer` crate / `cgroup` backend), never inside
   policy. The policy layer stays "how threads run", the freezer would be
   "whether processes run".

**Recommendation:** keep the current scope. Freeze = Android system layer's
job. Revisit only as an independent project/module with cgroup v2 backend.

---

## Q4. Decision engine — should it auto-migrate?

**Verdict: observe-first, and keep decision-making *gating* not *acting*.**

What the decision layer should do:

1. **Gate relock** (P6.2): front/background + pressure → skip or run relock.
   This is already half-done (oom_adj threshold); wire `from_sources` +
   `is_foreground_uid` + `decide()`.
2. **Degrade under pressure** (exists as `pressure_sensitive`): Critical
   memory/thermal → skip intervention. This is the correct "observe first"
   behavior — it lets the kernel scheduler do what it does best under load.

What it must NOT do:

- **Userspace thread migration** (move thread big↔little on our judgment).
  CFS already does wakeup-preemption; userspace migration is always late
  (we sample), never full-context (no per-thread utilization view), and
  fights the scheduler. sched_switch observation (P7 eBPF) is fine as
  *observe*; `MigrateAction::Force` should remain effectively unreachable.

The example scenario (memory pressure + background + long idle → restrict)
is exactly what Android's LMKD + cgroup already implement. Duplicating it
userspace = more fighting, less predictability.

**Bottom line:** threadctl stays an affinity/sched controller with a
*gating* decision layer. Do not become a userspace resource manager.

---

## Q5. Android-specific concerns

| Concern | Reality | Action |
|---|---|---|
| MIUI/HyperOS cpuset layout | `/dev/cpuset` v1 still present; our `threadctl/` subtree is self-owned, coexists with AMS-managed dirs | Verified on SM8550; keep hardcoding `/dev/cpuset` for now |
| LMKD interaction | We only **read** `oom_score_adj` for relock gating — no conflict | Keep read-only |
| cpuset conflicts | Our subtree's `cpus` is bounded by parent; we write `present` range at creation | OK |
| cgroup ownership | Android 12+ dual-mount; we write v1 `/dev/cpuset` path | Works; v2 backend is a P7+ topic |
| Scheduler interference | Only whitelisted threads get setaffinity; three-layer filtering prevents EINVAL storms | Minimal footprint |
| Zygote fork window | cmdline fills late; new app discovery delayed ~2s | P6.2 pending queue (200ms re-read) |
| TID reuse | Short-lived threads can be missed in the incremental path | P6.2 (NEW-M4) |

---

## Q6. Long-term architecture

The layering scales. Corrections to the proposed roadmap:

- **P7 "uclamp" is already largely done** — the P6.1 review round landed
  `Policy.uclamp_min/max` + `sched_setattr(SCHED_FLAG_UTIL_CLAMP)` end-to-end.
  Remaining: capability-gated application + per-profile defaults.
- eBPF (P8) must stay an `EventSource` implementation behind the trait —
  the core keeps zero aya dependency, and the /proc fallback chain stays
  the degradation path.

**Architecture invariants (must not change):**
1. core = pure logic, zero aya/tracing dependency
2. RuleSet frozen (P6.1); matcher finds sources, merge decides
3. Whitelist-only: never touch unconfigured processes
4. /proc → eBPF degradation chain preserved
5. uclamp > schedtune > cpuset > affinity priority chain

---

## Kernel-level concerns (for the record)

- `sched_setattr` uclamp needs `CAP_SYS_NICE` + `CONFIG_UCLAMP_TASK`; failure
  is non-fatal (affinity already applied) — handled in `apply_uclamp`.
- cpuset is a hard boundary: we exploit it to *relax* Android's restriction,
  but our subtree is bounded by its parent — a config asking for CPUs outside
  the parent's range silently degrades (allowed-intersection filter).
- CFS load-balancing interacts with setaffinity; relock at 60s is
  deliberately infrequent to avoid fighting the scheduler.
- Future freezer, if any: cgroup v2 `cgroup.freeze`, not SIGSTOP.

---

## Summary

| Question | Verdict |
|---|---|
| Q1 matcher | Sound, frozen. Escape hatch (`inherit false`) in P6.2 |
| Q2 policy engine | Already a priority resolver; make merge table data-driven (low prio) |
| Q3 freeze | **No** in P6.x; domain mismatch + Android owns lifecycle; if ever: cgroup v2 freezer, separate module |
| Q4 decision | Gating only (relock + degrade); never userspace migration |
| Q5 Android | Verified; pending queue + TID-reuse for P6.2 |
| Q6 roadmap | Layering scales; uclamp largely landed; eBPF stays behind trait |

*Do not change: matcher, core purity, whitelist scope, degradation chain,
priority chain.*
