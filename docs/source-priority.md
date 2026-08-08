# RuleSource 优先级 — 单一事实源

> 本文档是 `RuleSource::priority()` 的**唯一**数值定义。
> 所有评审/交付/README 文档引此，不在各文档复制数值（避免多真相冲突，
> ChatGPT P6.2 终审问题 2）。

实现：`crates/core/src/ruleset.rs` → `RuleSource::priority()` (match 表)

## 数值表

| Source | priority | 层级 |
|---|---|---|
| `Global` | 10 | 全局默认（Config Compiler 展开） |
| `Profile` | 20 | 内置模板（game/chat/...） |
| `Group` | 30 | 内置包组表（P6.3） |
| `PackageWildcard` | 40 | 通配包名（如 `com.tencent.*`，specificity 最大者） |
| `PackageExact` | 50 | 精确包名 |
| `ThreadType` | 60 | 线程类型（render/audio/binder） |
| `ThreadExact` | 70 | 精确线程名 |

## 语义

- 数值 = 优先级，**越大越优先**。
- 排序由 merge.rs `merge_rules()` 按 `priority()` 降序，不依赖 enum 声明顺序。
- 插入新来源时在 match 表加行即可（预留间隔 10），不会静默改变既有优先级。

## 用户可见语义（文档用，不出现数值）

- **最长固定前缀匹配优先**（`com.tencent.mm` 精确 > `com.tencent.*` 通配）
- 线程规则覆盖包级规则同字段
- 低优先级来源填充高优先级未设置的字段（继承）
