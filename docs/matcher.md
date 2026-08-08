# Package Matcher & Policy Merge — Design

> Module: `crates/core/src/ruleset.rs`
> Status: frozen (P6.1)
>
> This document describes what the matcher does and why. It is written for
> contributors — if you plan to modify `RuleSet`, read this first.

---

## 1. Motivation

Rules arrive from many sources (exact package, wildcard package, future
profile/group expansion, thread-type aliases). The core problem is:

> Given `(package, thread_name)`, which rules apply, and what final policy wins?

Early drafts answered this with a single "exact > wildcard" selection. That
breaks down once profile/group sources arrive: an exact package rule and a
wildcard package rule may set **different fields**, and the user expects both
to contribute (an exact `sched` plus a wildcard `cpus`).

The solution is to split the problem into two independent stages:

```
PackageMatcher  → finds candidate sources          (matching)
PolicyMerge     → decides the final policy         (merging)
```

**Matcher's job: find sources. It never decides the final policy.**

---

## 2. Design overview

```text
RuleSet (compiled)
├── exact:      HashMap<String, Vec<usize>>        // exact package → rule idxs
├── wildcards:  Vec<WildcardRule>                  // pattern groups
└── cache:      Mutex<HashMap<String, Vec<RuleMatch>>>

resolve(pkg, thread):
  1. collect_pkg_matches(pkg) → Vec<RuleMatch>     // PackageMatcher
  2. filter thread rules by fnmatch                 // ThreadMatcher
  3. merge_by_priority(...) → Option<Policy>        // PolicyMerge
```

### RuleMatch / RuleSource

```rust
pub struct RuleMatch {
    pub index: usize,          // index into RuleSet.rules
    pub source: RuleSource,    // where the rule came from
}

pub enum RuleSource {
    Global,           // P6.2 reserved
    Profile,          // P6.2 reserved
    Group,            // P6.2 reserved
    PackageWildcard,  // wildcard package (highest-specificity group)
    PackageExact,     // exact package
    ThreadType,       // P6.2 reserved
    ThreadExact,      // reserved
}
```

Priority is **explicit** via `RuleSource::priority()` (a match table, 10..70).
It is deliberately **not** derived from enum declaration order — inserting a
variant mid-list must never silently change priorities.

---

## 3. Matching rules

### 3.1 Package level

`collect_pkg_matches(pkg)` returns **all** candidate sources for a package:

- exact rules for `pkg`, if any
- the **highest-specificity** wildcard group that matches `pkg`, if any

Both can be present — they are sources, not alternatives:

```text
input:  com.tencent.mm
output: [ PackageWildcard(com.tencent.*), PackageExact(com.tencent.mm) ]
```

### 3.2 Wildcard specificity

When multiple wildcard patterns match, the most specific one wins.
Specificity uses a **longest-fixed-prefix** strategy. Internal scoring:

```text
score = fixed_prefix_length × 100 + literal_char_count − wildcard_count × 10
```

```text
com.tencent.mm*   → 1404
com.tencent.*     → 1202
com.*.service     →  401
com.*             →  394
```

Users only need to understand: **the longer the fixed prefix, the higher the
priority.** The formula is an internal detail and may change.

### 3.3 Thread level

Within the matched sources, thread rules (non-empty `thread`) are tested with
POSIX fnmatch against the thread name. If any thread rule hits, the thread-rule
set is the primary input to merge; otherwise the package-level rules are.

### 3.4 Cache

`collect_pkg_matches` caches per-package results:

```rust
cache: Mutex<HashMap<String, Vec<RuleMatch>>>
```

- First resolve scans exact/wildcard; subsequent lookups are O(1)
- **Empty results are cached too** — prevents unknown-package scan storms
- The cache is **instance-scoped**: hot-reload creates a new `RuleSet`, so the
  cache invalidates naturally (never global, no manual clearing)
- Lifecycle assumption: Android long-running daemon, active packages < a few
  thousand. A future desktop mode can add LRU / TTL / capacity.

---

## 4. Merge semantics

`merge_by_priority` is a **field-level merge model**. Each field has its own
merge strategy — this is not CSS cascade:

| Field | Strategy |
|---|---|
| `cpus` | OR-merge within the same source group; highest-priority **group** wins across sources (a lower-priority source fills only if the higher left `cpus` unset) |
| `sched` | highest-priority source that has a value wins (inheritance: exact without `sched` inherits the wildcard's `sched`) |
| `nice` | first effective value by priority |
| `uclamp` | constraint merge (P6.2) |

Thread rules override package rules on the same field; package rules still
fill gaps the thread rules left unset.

```text
exact: cpus 0-1            wildcard: cpus 4-7
→ com.tencent.mm cpus = 0-1   (exact overrides)

exact: (no cpus), sched fifo    wildcard: cpus 3-6
→ com.tencent.mm RenderThread = cpus 3-6 + sched fifo   (fields combine)
```

`cpuset_dir` is derived from the **final merged** CPU range, never from a
single rule.

---

## 5. Performance

Benchmark environment: Snapdragon 8 Gen 2 (ARM64), Android, Rust release build.

| Scenario | Result |
|---|---|
| Compile 1000 wildcard rules | < 1 ms (incl. CString pre-compilation) |
| Cold resolve (1000-wildcard scan) | < 1 ms |
| Hot resolve (cache hit) | ≈ 6 µs/op |
| Cache growth | 1 entry per distinct package observed |

Cache entry size ≈ package name string + small `Vec<RuleMatch>`.

---

## 6. Compatibility

- Public API unchanged since P2: `RuleSet::resolve(pkg, thread)`,
  `is_interested(pkg)`, `has_thread_rules(pkg)` — the daemon does not need to
  know about matching internals
- `[[rule]]` TOML legacy behavior preserved (same-package rules OR-merge cpus,
  first sched/nice wins)
- groups/profiles are expanded in the **Config Compiler phase** (P6.2/6.3);
  `RuleSet` never learns about high-level semantics
