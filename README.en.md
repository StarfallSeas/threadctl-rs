# threadctl-rs — Android/Linux Task Policy Engine

**English | [中文](./README.md)**

> Lets Android/Linux users define per-app and per-thread scheduling policies
> (CPU affinity, sched policy, priority) and keeps them enforced at runtime —
> fighting Android AMS/cgroup-level affinity overrides.
>
> Think of it as **a systemd policy engine for Android application threads**:
> systemd manages services, this manages how threads run.

> ## What threadctl-rs is not
>
> - does **not** kill apps
> - does **not** freeze processes
> - does **not** replace Android LMKD (memory management)
> - does **not** replace the Linux scheduler
>
> It only applies explicit user-defined thread policies (affinity / sched / uclamp).

```text
Application Threads
        ↓
threadctl daemon
        ↓
Config Compiler → RuleSet → Policy Merge → Kernel Action
```

![license](https://img.shields.io/badge/license-GPL--3.0-blue.svg)
![rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)
![tests](https://img.shields.io/badge/tests-73%20passing-green.svg)
![platform](https://img.shields.io/badge/platform-Linux%20%7C%20Android-lightgrey.svg)

---

## Table of Contents

- [Why threadctl-rs?](#why-threadctl-rs)
- [Features](#features)
- [Quick Start](#quick-start)
  - [Linux](#linux)
  - [Android](#android)
- [Configuration](#configuration)
- [Architecture](#architecture)
- [Safety](#safety)
- [Testing & Quality](#testing--quality)
- [Roadmap](#roadmap)
- [FAQ](#faq)
- [Limitations](#limitations)
- [License](#license)

---

## Why threadctl-rs?

| | taskset / pinning tools | threadctl-rs |
|---|---|---|
| Scope | single process | per-app / per-thread matching |
| Lifetime | one-shot | enforced continuously at runtime |
| vs system overrides | none (lost once AMS moves cpuset) | relock auto-recovers |
| Foreground awareness | none | power-save in background, restore on foreground |
| Config model | CLI arguments | declarative rules (inheritance/override) |

Ordinary pinning tools answer "pin this process to these cores, once".
threadctl-rs answers "**how should each thread of this app run in this
scenario**" — and keeps it that way.

### Config inheritance model

```text
Global defaults
  ↓
Profile templates (game / chat / ...)
  ↓
Wildcard package rules (com.tencent.*)
  ↓
Exact package rules (com.tencent.mm)
  ↓
Thread rules (RenderThread)
```

Rules stack top-down: more specific overrides less specific, unset fields
inherit upward — **describe only the differences, never repeat the full config**.

## Features

- **Dual config formats**: KDL (recommended) + TOML (`[app]` new style / `[[rule]]` legacy)
- **Profile abstraction**: 7 built-in scenario templates, enabled with one line. Note: **a profile is a policy template, not bound to a specific app** — `profile "game"` fits any game, not "Yuanshen-specific tuning":

  ```kdl
  app "com.miHoYo.Yuanshen" { profile "game" }   // render on strongest core + freq protection
  app "com.tencent.mm" { profile "chat" }         // smooth rendering, clear audio
  ```

- **Package wildcards**: `com.tencent.*` longest-fixed-prefix matching (like nginx location priority), most specific pattern wins
- **Inheritance semantics**: exact rules override wildcard rules; low-priority sources fill gaps — e.g. a wildcard rule provides the default CPU cluster, an exact rule only overrides the render thread's cores

  ```kdl
  app "com.tencent.*" { default { cluster "big" } }        // Tencent apps default to performance cores
  app "com.tencent.mm" {                                   // exact WeChat rule
      thread "RenderThread" { cluster "prime"; sched "fifo" }
  }
  // WeChat RenderThread = prime + fifo; other threads inherit big
  ```

- **Thread matching**: fnmatch thread-name patterns + built-in thread-type aliases (render / audio / binder / main)
- **Capability detection**: uclamp > schedtune > cpuset > affinity priority chain
- **Three-layer filtering**: online → cgroup allowed ∩ → setaffinity (avoids invalid CPU masks before applying affinity)
- **Hot reload**: inotify-first, poll fallback, versioned snapshots (keeps old config on failure)
- **Audit feedback loop**: Observe → Decide → Act → Measure → Adjust
- **Relock**: periodic re-locking against AMS overrides; automatically skips background/cached processes (power saving)
- **Auto degradation**: eBPF unavailable → /proc polling
- **Low overhead**: event-driven; no full /proc scan unless the process count changes

---

## Quick Start

### Linux

```bash
cargo build --release -p threadctl
./target/release/threadctl -c examples/threadctl.kdl
```

### Android

```bash
# 1. Push the binary (Magisk modules: /data/adb/threadctl/)
adb push target/release/threadctl /data/adb/threadctl/
adb shell "chmod 755 /data/adb/threadctl/threadctl"

# 2. Push a config (edit package names to your apps)
adb push examples/user-mode.kdl /data/adb/threadctl/threadctl.kdl

# 3. Start as root
adb shell "su -c '/data/adb/threadctl/threadctl -c /data/adb/threadctl/threadctl.kdl'"
```

> Magisk users: set `lock-interval` to 60s (against AMS overrides); use absolute paths.

**First run (30 seconds)**:

```bash
cp examples/user-mode.kdl threadctl.kdl
# edit threadctl.kdl, change package names to your apps
./target/release/threadctl -c threadctl.kdl
```

### User mode (recommended)

```kdl
// One line per app — just change the package name
app "com.miHoYo.Yuanshen" { profile "game" }
app "com.tencent.mm" { profile "chat" }
app "com.miui.home" { profile "launcher" }
```

Built-in profiles: `game` / `chat` / `video` / `launcher` / `audio` / `balanced` / `power-save`

### Advanced mode

```kdl
app "com.example.game" {
    default { cluster "big" }                    // default for all unnamed threads
    thread "UnityMain" { cluster "prime"; sched "fifo"; priority 60 }
    thread-type "render" { cluster "big" }       // built-in alias for render threads
}
```

- `cluster` accepts `little` / `mid` / `big` / `prime` (auto-detected from cpu_capacity: 3-group SoCs have no mid, SM8650 has mid, all-big SoCs only big/prime); **numeric ranges are auto-detected as cpus** (`cluster "0-6"` ≡ `cpus "0-6"`)
- A cluster missing on the device → **same-tier fallback + warning** (little/mid → big, big/prime → prime; never silently binds wrong cores)
- Thread names longer than 15 bytes get truncated by the kernel — the daemon warns at startup

---

## Configuration

- `examples/threadctl.kdl` — full KDL example
- `examples/user-mode.kdl` — user-mode template (just change package names)
- `crates/core/config/threadctl.toml` — TOML default template

## Developer docs

- `docs/matcher.md` — package matcher & policy merge design
- `docs/ai-review-process.md` — development & review process
- `docs/DeepSeek/` — architecture & phase design + responses/adoptions
- `docs/ChatGPT/` — ChatGPT raw reviews
- `docs/Claude/` — Claude raw reviews

---

## AI Collaboration

threadctl-rs is an **AI-driven software engineering experiment**: multiple
AI models collaboratively produce architecture, implementation, review and
documentation. Humans own direction, final decisions and real-device validation.

| Role | Model | Responsibility |
|---|---|---|
| Implementation | DeepSeek V4 Flash | architecture, code, engineering |
| Code review | Claude | code-level architecture review, defect finding |
| Docs/spec review | ChatGPT | documentation audit, engineering standards |
| Human (maintainer) | — | requirements, final decisions, device validation |

> Responsibility boundary: AI provides engineering capability, the human
> keeps decision authority — whether to adopt a proposal, change architecture,
> or ship a release. See `docs/AI-workflow.md` for the full process.

---

## Architecture

### Layers

```text
┌───────────────────────────────────────────────────────────┐
│  threadctl (daemon, bin)                                   │
│  CLI / hot-reload loop / SystemContext sampling / audit    │
├───────────────────────────────────────────────────────────┤
│  threadctl-core (lib, pure logic, zero aya deps, testable) │
│                                                           │
│  Config Compiler      KDL/TOML → ConfigModel AST           │
│  Rule Compiler        → RuleSet: exact + wildcard coexist  │
│  ThreadMatcher        fnmatch thread-name hit set          │
│  Policy Merge Layer   merge_by_priority: field-level merge │
│                       (field-level merge core is done;     │
│                        decision-driven integration P6.2)    │
│  Kernel Action        online∩allowed → setaffinity         │
│                       + cpuset + sched/nice + uclamp        │
│                                                           │
│  Support: store(hot-reload) tracker(state) audit(loop)     │
│           system_context(pressure) decision(decide)        │
│           capability(chain)                               │
├───────────────────────────────────────────────────────────┤
│  threadctl-ebpf (kernel side, no_std)                      │
│  fork/exec migration + sched_switch sampling (P7)          │
└───────────────────────────────────────────────────────────┘
```

### Core principles

1. **Matching decoupled from merging**: `RuleSet` emits `RuleMatch{index, source}`; the merge layer decides the final policy
2. **groups/profiles belong to the compile phase**: expanded in the Config Compiler; the rule engine never learns high-level semantics
3. **Sources coexist, not mutually exclusive**: exact overrides wildcard fields; low-priority sources fill gaps
4. **Error visibility**: rules are never silently dropped — invalid cluster names, over-long thread names, cpuset write failures all warn

---

## Architecture Invariants

**threadctl-rs will not**:

- replace Android LMKD (memory management)
- freeze applications or kill background processes
- migrate tasks automatically
- replace the Linux scheduler

**threadctl-rs provides**:

- thread affinity control
- scheduler attribute control (sched / nice)
- uclamp constraints
- policy-based execution hints
- observability (audit / telemetry)

> User space provides **constraints**, not **replacement policy** — the kernel
> already has wakeup migration, load balancing, PELT, EAS and uclamp with full
> information.

---

## Safety

- **No kernel patch required**: pure user-space syscalls; no kernel modification
- **No system partition modification**: only writes its own cpuset subtree (`/dev/cpuset/threadctl/`)
- **Fail-safe fallback**: a failed policy application (permissions, cgroup limits, thread exit) skips that thread with a warning and never affects others; a config parse failure keeps the old config running
- **Whitelist-scoped**: only explicitly configured packages are affected; everything else keeps system defaults

---

## Testing & Quality

- **46 unit tests**: matcher inheritance semantics, specificity ordering, instance-scoped cache (1000 wildcards × 10000 resolves), audit ring buffer, config merging, profile expansion, hot-reload versioning
- **Zero warnings**: `cargo check --workspace` 0 warnings, 0 errors
- **Lightweight**: ~5000 lines of Rust, 852KB release (strip + LTO)
- **Real-device validated**: verified on SM8550 devices (cpuset join successful, zero cgroup degradation)

---

## Roadmap

| Phase | Scope | Status |
|---|---|---|
| P0-P2 | workspace / ConfigStore / proc pipeline | ✅ |
| P5 | five modules (audit / foreground / system_context / capability / decision) | ✅ |
| P6.0 | Profile abstraction + 7 built-in templates | ✅ |
| P6.1 | pkg matcher (MatchPriority / specificity / inheritance / cache) | ✅ frozen |
| P6.2 | Policy Merge Engine: decision-driven relock, source priority matrix, Zygote pending, @@main, tracing | 🔄 |
| P6.3 | group (built-in common package table) | ⏳ |
| P7 | eBPF kernel side: fork/exec migration + sched_switch sampling | ⏳ |
| P8 | Magisk production package (module.prop / service.sh / update channel) | ⏳ |

---

## FAQ

**Q: How is this different from ordinary "CPU pinning" tools (taskset / app pinning)?**
A: Ordinary tools set affinity once. threadctl-rs is a **runtime policy engine**:
declarative config keeps matching, auto-recovers after Android AMS/cgroup overrides
(relock), and adjusts policy strength by foreground state and system pressure.

**Q: Does it require root?**
A: Yes. Setting CPU affinity (sched_setaffinity) and RT scheduling (fifo/rr) needs
root or CAP_SYS_NICE. On Android, run it as a Magisk module.

**Q: Will it hurt battery life?**
A: Designed power-first: background/cached processes skip relock (let the Linux
scheduler decide), and the `power-save` profile uses only efficiency cores.

**Q: Will it make games slower?**
A: No — it does not force all threads onto big cores. Only matched threads
(e.g. the render thread) are constrained; unmatched threads keep system
scheduling. The `game` profile actually pins the render thread to the strongest
core with frequency protection — usually a net win.

**Q: Does it support non-Xiaomi devices?**
A: Yes. Cluster detection is based on `cpu_capacity` (standard sysfs), not vendor
interfaces. MIUI/HyperOS `/dev/cpuset` layout is verified compatible.

**Q: When will eBPF mode be available?**
A: P7. The current /proc polling mode covers all features; eBPF targets lower-latency
fork/exec events and sched_switch migration observation.

---

## Limitations

- This repository documents AI-assisted development; **AI-generated decisions
  may contain mistakes** — every architectural decision requires source review,
  compilation and hardware validation, regardless of being AI-generated.
- Only **whitelisted** packages are constrained; unconfigured processes keep system defaults (no fallback enforcement)
- Thread names >15 bytes get truncated by the kernel — use `thread-type` or check startup warnings
- Android AMS moves cpuset ownership when an app goes background; relock is the countermeasure (background processes are not re-locked, restored on foreground return)
- Multi-process apps (`:service` children) need their own rules

---

## License

GPL-3.0
