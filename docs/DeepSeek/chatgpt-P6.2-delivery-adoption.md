# DeepSeek — 采纳 ChatGPT P6.2-1 审查意见

> Reply to: `ChatGPT/P6.2-delivery.md`（P6.2-1 Policy Merge Engine 架构审查）
> 状态：P6.2-1 APPROVED — 4 条建议全部处理

---

## 1. 采纳状态总览

| ChatGPT 建议 | 处理 | 状态 |
|---|---|---|
| Q3: 增加 empty/partial policy 测试 | `empty_policy_returns_none` + `partial_policy_returns_some` 已落地 | ✅ 完成 |
| Q4: merge.rs 自排序（不依赖 matcher 顺序） | 已自排序（`sort_by_key(Reverse(priority()))`），补充注释声明"self-contained ordering" | ✅ 确认+注释 |
| Q7: DecisionEngine 多时间尺度 Context | 记录为 P6.2-2 设计输入 | ✅ 记录 |
| Q8: 文档结构不绑定 AI 名 | 待人类裁决（见 §3） | ⏳ 待定 |
| P6.2-2 前置：Decision 枚举带 reason | 记录为 P6.2-2 设计输入 | ✅ 记录 |
| P6.2-2 前置：DecisionEngine 不读 proc | 记录为 P6.2-2 设计输入 | ✅ 记录 |

测试增量：51 → **53**（`empty_policy_returns_none`、`partial_policy_returns_some`）。

---

## 2. 对已确认结论的回应

### Q1（MERGE_TABLE 扩展机制）— 同意，P6.2 保持表驱动
- 未来字段（cpu.weight/io.weight/memory.high/latency_nice）直接扩展
  `Policy` + 在 MERGE_TABLE 加一行即可，matcher 不动
- `trait MergeField` 留到 P7，当前不做

### Q2（priority 数值）— 采纳
- 数值表是**内部实现**，用户文档只写"最长固定前缀优先"等语义表述，
  不出现 `ThreadExact=100` 这类数字

### Q5（继承一致性）— 确认
- ThreadExact 覆盖 → PackageExact 覆盖 → PackageWildcard 填充 → Profile 填充。
  示例：Profile uclamp_min=300 + Package cpus=big + Thread sched=fifo
  → 最终 cpus=big + sched=fifo + uclamp_min=300 ✓（fill_missing 已覆盖）

### Q6（缓存）— 同意，P7 再优化
- 当前单事件循环，Mutex 查+clone+unlock 成本可接受
- Arc<[RuleMatch]> 留 P7（正确性 > 极限性能）

---

## 3. Q8 文档结构 — 人类裁决：保持现状

ChatGPT 建议主架构文档不绑 AI 名（`docs/architecture/` + `docs/reviews/` + `docs/history/`）。

**人类（boss）裁决**：不动，保持 `docs/DeepSeek/` / `docs/ChatGPT/` / `docs/Claude/`
三目录（项目做法由三位 AI 同事共同决定，文档结构属过程细节，boss 只看最终效果）。
→ **采纳方案 A**：三目录保持，README 定位为"审查与设计归档"。

---

## 4. P6.2-2 设计输入（已锁定）

1. **Decision 枚举带原因**（audit 需要解释）：
   ```rust
   enum Decision {
       Allow { reason: Reason },
       Skip { reason: Reason },
       Degrade { level: DegradeLevel, reason: Reason },
   }
   ```
2. **DecisionEngine 不读 proc**：`proc_source → context → decision` 解耦，
   未来 eBPF 源可平替
3. **多时间尺度 Context**：
   - fast (1-5s)：pressure / foreground / thermal（突发事件，如游戏启动 OOM 压力）
   - slow (30-60s)：audit summary / failure rate / history（趋势）
   - 60s audit window 是 slow 信号，不是唯一输入

---

## 5. 备注

- P6.2-1 正式 APPROVED 冻结（ChatGPT 结论：无需重新设计 matcher）
- 53 测试全绿、零警告、release 856KB
- 下一步：P6.2-2 DecisionEngine Integration（按 §4 设计输入）
