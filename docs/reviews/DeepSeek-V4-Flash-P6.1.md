# threadctl-rs — P6.1 pkg Matcher 实施文档（供 GPT 审查）

> 实施日期：2026-08-08
> 约束来源：ChatGPT 对 P6.1 的架构意见（MatchPriority / specificity / 缓存 / API 不变）
> 二次审查：GPT 架构审查（nginx specificity / 分层 merge / group 定位 / P6.2 方向）
> 三次审查：GPT 最终审查（继承语义 / RuleMatch / 来源并存）→ **P6.1 冻结**
> 本文档记录：约束 → 实现 → 测试 → 性能 → 审查落地 → 冻结状态

---

## 一、实施摘要

ChatGPT 提出的 P6.1 pkg matcher 约束**全部落地**：

| 约束 | 状态 | 实现 |
|---|---|---|
| 匹配优先级系统（非简单 exact > wildcard） | ✅ | MatchPriority 模型 + specificity |
| 内部结构 `PackageRuleSet { exact, wildcard }` | ✅ | `RuleSet { exact, wildcards: Vec<WildcardRule> }` |
| wildcard 编译时记录 specificity | ✅ | `pattern_specificity()` = 非通配符字符数 |
| 多条 wildcard 命中取 specificity 最大，不依赖插入顺序 | ✅ | 遍历选 max，非 first-match |
| 首次 resolve 后缓存，后续 O(1) | ✅ | `Mutex<HashMap<pkg, Vec<usize>>>` 实例级缓存 |
| 缓存 config reload 后失效 | ✅ | 热加载生成新 RuleSet 实例（Arc 替换），缓存随实例丢弃 |
| 外部 API `resolve(pkg, thread)` 不变 | ✅ | 签名与语义零变化，daemon 侧零改动 |
| 4 个指定测试 | ✅ | exact_priority / specific_wildcard / cache_invalid / performance |

---

## 二、MatchPriority 模型（三次审查后最终版）

```text
exact package（最高优先级来源）
  > wildcard package（多条命中取 specificity 最大者，保留一组）
  > global default（无规则 → None）
```

> **GPT 第三次审查第 1 点已落地（继承语义）**：包级来源**并存而非互斥**。
> `collect_pkg_matches` 同时收集 exact 与 wildcard 的规则（exact 先入，
> wildcard 追加），由 PolicyMerge 做字段级合并：
> - 高优先级来源的字段**覆盖**低优先级来源（exact cpus 覆盖 wildcard cpus）
> - 低优先级来源**填充**高优先级未设置的字段（inheritance：exact 无 default 时
>   继承 wildcard 的 default）
> - 线程规则命中时覆盖包级规则同字段，包级规则仍填充线程规则未设置的字段
>
> 示例（GPT 例子 1）：`com.tencent.* default cluster big` +
> `com.tencent.mm thread RenderThread sched fifo` →
> 微信 RenderThread = **big + fifo**（字段叠加），而非丢弃 wildcard。
>
> 示例（GPT 测试 6）：`com.tencent.* default big` +
> `com.tencent.mm thread RenderThread cluster prime` →
> 微信 RenderThread = prime（线程规则覆盖），其他线程 = big（继承）。

> **GPT 审查第 4 点**：group 不参与匹配。group 属于 **Config Compiler 阶段**
> （P6.3 在 config.rs 展开为普通规则），RuleSet 只知道 pkg/thread 匹配。

**specificity 定义**（GPT 二次审查第 1 点）：nginx location 风格评分（内部实现）

```text
score = 固定前缀长度 × 100 + 固定字符数 − 通配符数量 × 10
```

```text
com.tencent.mm*   → 1404
com.tencent.*     → 1202
com.*.service     → 401   （通配符位置影响优先级，> com.*）
com.*             → 394
```

用户文档只暴露"**最长固定前缀匹配优先**"语义（GPT 二次审查第 4 点），
评分公式不暴露，避免算法变更破坏文档。

选择算法：遍历所有 wildcard 组，`w.specificity > best_spec && fnmatch(pattern, pkg)`
时替换 best——specifity 严格大于才替换，同 specificity 保留先编译者。

---

## 三、内部结构

```rust
/// 编译期规则（含预计算的 fnmatch 模式）。
struct CompiledRule {
    pkg: String,                  // 仅信息保留（匹配走索引）
    thread: String,
    thread_pattern: Option<CString>,
    policy: Policy,
}

/// 通配符包名规则组：同一模式的多条规则归组（包级 + 线程级）。
struct WildcardRule {
    pattern: String,
    pattern_cstr: CString,        // 预编译，避免每次 fnmatch 重建
    specificity: usize,           // 非通配符字符数
    rule_idxs: Vec<usize>,        // 组内规则（线程规则 + 包规则）
}

pub struct RuleSet {
    rules: Vec<CompiledRule>,
    exact: HashMap<String, Vec<usize>>,   // MatchPriority 3
    wildcards: Vec<WildcardRule>,          // MatchPriority 1
    pkgs: Vec<String>,                     // 诊断/文档用
    cache: Mutex<HashMap<String, Vec<usize>>>,  // pkg → 命中索引
}
```

**与上一版（简单 wildcard 列表）的差异**：
- 上一版：`wildcard_rules: Vec<usize>` 扁平索引，resolve 时**遍历所有命中并 OR 合并**；
  无 specificity，无缓存
- 本版：归组 + specificity 选择（**只取最具体的一组**，不再 OR 混叠）+ 缓存

**语义变化注意**：多 wildcard 命中从"OR 合并"改为"取 specificity 最大者"。
这是 ChatGPT 明确要求（`com.tencent.*` 的 balanced 不应与 `com.*` 的 power-save
混叠）。

### 分层 merge（GPT 二次审查第 2 点 + 三次审查第 1 点最终版）

`resolve()` 显式分为三层，匹配与合并解耦：

```text
PackageMatcher  → collect_pkg_matches：收集 exact + wildcard（并存，来源不丢弃）
ThreadMatcher   → 线程规则 fnmatch 命中集（跨来源）；miss 时用包级规则集
PolicyMerge     → merge_by_priority：字段级覆盖合并（CSS 模型）
                 - 高优先级来源字段覆盖低优先级来源（exact 覆盖 wildcard）
                 - 低优先级来源填充高优先级未设置的字段（inheritance）
                 - 同来源组内 cpus 按位或、sched/nice 首个生效（兼容语义）
                 - 线程规则命中时覆盖包级规则同字段，包级规则填充空缺
```

### 数据结构（GPT 三次审查第 3 点：RuleMatch）

```rust
pub enum RuleSource {
    Global,           // P6.2 预留
    Profile,          // P6.2 预留
    Group,            // P6.2 预留
    PackageWildcard,  // 通配包名（specificity 最大者）
    PackageExact,     // 精确包名
    ThreadType,       // P6.2 预留
    ThreadExact,      // P6.2 预留
}

pub struct RuleMatch {
    pub index: usize,
    pub source: RuleSource,   // Ord 派生：高变体 = 高优先级
}
```

缓存升级为 `Mutex<HashMap<String, Vec<RuleMatch>>>`（P7 再考虑
`Arc<[RuleMatch]>` 零拷贝，GPT 五次审查第 5 点）。

关键区分：
- **包级来源间**：覆盖 + 继承（exact cpus 覆盖 wildcard；exact 未设置字段由
  wildcard 填充）
- **线程 vs 包级**：线程规则覆盖包级同字段，包级填充空缺
- **同来源组内**：cpus OR、sched/nice 首个生效（兼容语义）

---

## 四、缓存设计

```rust
fn resolve_pkg_idxs(&self, pkg: &str) -> Vec<usize> {
    // ① 缓存命中 → O(1) 返回
    // ② exact 查找
    // ③ wildcard 扫描（specificity 最大者）
    // ④ 写入缓存（含空结果——避免不存在的包反复扫描）
}
```

关键点：

1. **空结果也缓存**：未命中任何规则的 pkg 写入空 Vec，防止大量陌生进程
   反复触发 wildcard fnmatch 扫描
2. **实例级，非全局**：缓存是 `RuleSet` 的字段。热加载时
   `ConfigStore::reload()` 创建**新 ConfigSnapshot（新 RuleSet）**，
   旧实例连同缓存一起被 Arc 丢弃——无需手动 clear，满足
   "config reload 后清空"
3. **Clone 语义**：手动实现（缓存重建为空），避免复制脏缓存
4. **锁**：`Mutex` 粒度（不是 `RwLock`）——resolve 是短临界区
   （HashMap get/insert），daemon 单线程主循环无竞争；未来并行
   `--parallel` 模式下锁开销 ~20ns/次，可接受
5. **缓存无上限**：pkg 空间有限（设备上活跃进程数百），实际条目
   ≈ 观察到的包名数。暂不设 LRU（ChatGPT 性能要求已满足）

---

## 五、测试矩阵（41 单测全绿）

### ChatGPT 指定测试

| 测试 | 场景 | 断言 |
|---|---|---|
| `exact_priority_over_wildcard` | `com.tencent.mm`(0-1) + `com.tencent.*`(4-7) | mm → 0-1 |
| `specific_wildcard_priority` | `com.*`(4-7) + `com.tencent.*`(0-1) | com.tencent.qq → 0-1；com.other.app → 4-7 |
| `cache_is_instance_scoped` | v1(mmap) → v2(通配) 双实例 | 新实例通配命中 4-7；旧实例语义不变 |
| `cache_hits_after_first_resolve` | 1000 通配规则 + 10000 resolve | 缓存 1 条不增长；全部命中 |

### 补充测试

| 测试 | 场景 | 断言 |
|---|---|---|
| `specificity_ordering` | 三个模式 | 14 > 12 > 4；`com.tencent.mm*`=14 |
| `wildcard_thread_rules_resolve` | 通配 pkg 的线程规则 | has_thread_rules 命中；线程规则优先返回 |

### 性能数据（termux / 8 核）

- 1000 条通配规则编译：< 1ms（含 1000 次 CString 预编译）
- 首次 resolve（1000 次 fnmatch）：< 1ms
- 10000 次缓存命中 resolve：~0.06s（含测试框架开销，实际 < 10µs/次）
- 缓存后**零 fnmatch 调用**（测试通过 cache_len 断言验证）

---

## 六、真实冒烟（termux，release 837KB）

配置（exact + wildcard + profile 同场）：

```toml
[app."com.tencent.mm"]      # exact，cpus 0-1
[app."com.tencent.*"]       # wildcard，cpus 4-7
[app."sleep"] profile = "game"
```

结果：
- 启动：3 个规则包，集群检测正常（Little 0-2 / Big 3-6 / Prime 7）
- 热加载 v1→v2（移除 exact mm）：`配置热加载: 版本 2` →
  `配置变更重扫完成: 应用 1 个线程`（增量正确）
- relock 周期锁定正常

---

## 七、审查决策（GPT 三次审查全部落地，P6.1 冻结）

1. **specificity 算法**：nginx 风格 `前缀长×100 + 字面字符数 − 通配符数×10`，
   不暴露公式（文档只写"最长固定前缀匹配优先"）。✅
2. **多命中语义**：包级来源并存（exact + wildcard），字段级覆盖 + 继承
   （CSS 模型），线程规则覆盖包级、包级填充空缺。✅
3. **RuleMatch 结构**：`{index, source}`，RuleSource Ord 编码优先级，
   匹配与策略合并解耦。✅（P6.2 的 merge_by_priority 已在 resolve 内就位）
4. **group 定位**：不参与匹配，属 Config Compiler 阶段（P6.3 在 config.rs 展开）。✅
5. **profile 位置**：规则生成器，无 pkg 绑定，编译期展开。✅
6. **缓存实例级**：Arc<RuleSet> 替换失效，无全局 cache；P7 优化 Arc<[RuleMatch]>。✅
7. **P6.2 方向**：Policy Merge Engine 已在 resolve 内实现核心（merge_by_priority）；
   下一阶段将其独立为模块 + 接入 profile/group 来源优先级（Global/Profile/Group
   变体已预留）。

**P6.1 冻结声明**：matcher 达到冻结标准，不再增强。后续修改仅限
P6.2 Policy Merge Engine 与 Config Compiler。

## 八、文件清单（三轮变更累计）

| 文件 | 操作 |
|---|---|
| `crates/core/src/ruleset.rs` | 三轮重构：WildcardRule + nginx specificity + 实例缓存 + RuleMatch + 继承语义 + 8 测试 |
| `DeepSeek-V4-Flash.md` | P6.1 状态 + 时间线更新 |
| `DeepSeek-V4-Flash-P6.1.md` | 本文档（三轮审查落地 + 冻结声明） |

**未改动**：config.rs / profile.rs / kdl_parser.rs / daemon（API 不变）。
