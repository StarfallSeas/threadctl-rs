# AI-Workflow — 协作开发流程（AI Collaboration Model）

> Status: Active
> Applies to: threadctl-rs 全部阶段（P0 起）
> 对应公开表述：README「AI Collaboration」节

---

## 1. 角色与责任边界

| 角色 | 模型 | 职责 | 不负责 |
|---|---|---|---|
| 实现者 | DeepSeek V4 Flash | 架构设计、代码实现、工程推进、文档撰写 | 最终决策 |
| 代码审查 | Claude | 代码级架构审查、缺陷发现、安全/正确性把关 | 文档措辞 |
| 文档/规范审查 | ChatGPT | 文档审计、工程规范、公开呈现 | 代码实现 |
| 维护者（人类） | — | 需求方向、架构裁决、版本发布、真机验证 | — |

**责任边界原则**：
- AI 提供工程能力（设计、实现、审查、文档）
- 人类保留决策权（需求是否采纳、架构是否改动、版本是否发布）
- AI 生成的任何决策**不因 AI 生成而免除验证**——源码审查、编译、硬件实测是强制门槛

---

## 2. 版本开发流程（P7 起：大版本三审制）

```
1. DeepSeek 撰写大版本文档（规划书 v1，ADR 风格）
2. ChatGPT 审 DeepSeek 文档（架构/文档方向）
3. Claude 审 ChatGPT 文档 + DeepSeek 文档 + 代码（架构缺口/实现细节）
4. DeepSeek 综合裁决：一致意见必采纳；分歧由执行者定夺（维护者可异议）
5. 裁决后定稿（规划书 v2）→ 三 AI 意见一致 → 开始构建
6. 构建按里程碑交付，每里程碑独立审
```

> P6 及以前为阶段细分制（Px.x），P7 起废除细分，每个大版本三审一致后构建。
> 详见 `docs/ai-review-process.md`。

---

## 3. 文档目录指引（公共工程文档 vs AI 协作记录）

```
README.md / README.en.md      公开入口（技术介绍 + AI 协作 + 限制）
docs/
├── ai-review-process.md      协作流程规范
├── AI-workflow.md            本文档（角色/流程/目录指引）
├── matcher.md                Package matcher 设计（工程文档）
├── repo-overview.md          仓库结构总览（工程文档）
├── source-priority.md        来源优先级（工程文档）
├── DeepSeek/                 实现者的规划/交付/审查回复（AI 协作记录）
├── Claude/                   Claude 代码审查记录（AI 协作记录）
└── ChatGPT/                  ChatGPT 文档审查记录（AI 协作记录）
```

**原则**：
- 工程文档（README/architecture/matcher 等）服务开发者——技术、简洁、无"AI 味"
- AI 协作记录（docs/DeepSeek|Claude|ChatGPT）服务审计者——保留过程、权衡、拒绝方案
- 新文档采用 ADR 风格头（Author/Reviewers/Status/Date/References）

---

## 4. 文档规范（ChatGPT 审计标准）

1. **AI 主导定位**：允许 "AI-driven / AI-assisted" 表述；避免"完全无需人工/100% AI"
2. **责任边界**：明确人类保留需求/决策/验证权
3. **可信度**：公开文档声明 AI 决策需验证（见 README Limitations）
4. **格式**：工程文档用 ADR/RFC 风格；AI 过程细节（"X 建议/Y 发现"）留在协作记录
5. **禁止**：营销化措辞（revolutionary/perfect/zero overhead/AI scheduler）

---

## 5. 当前审查状态

| 里程碑 | Claude | ChatGPT | 状态 |
|---|---|---|---|
| P7.1 eBPF 事件源 | ✅ 审查 9 项已修复 | ✅ 文档审计已落地 | Delivered（78 测试） |
| P7.2 自适应 relock / EXIT 过滤 | 待审 | 待审 | Planning |
| P7.3 IPC CLI / dry-run | 待审 | 待审 | Planning |
