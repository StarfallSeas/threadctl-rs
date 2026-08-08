//! Decision Engine — policy decision layer (ChatGPT 5.5 final confirmation).
//!
//! Observe → Analyze → Decide → Act → **Measure** → Adjust
//!
//! Synthesizes TaskIntent + SystemContext + UserProfile → ActionLevel + TaskScore.

use crate::system_context::PressureLevel;

/// 任务意图（P5.3：多源推断，不再仅依赖 oom_score_adj）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskIntent {
    Interactive,
    BackgroundLatencySensitive,
    Background,
    Frozen,
}

impl TaskIntent {
    pub fn from_oom_adj(oom_adj: i32) -> Self {
        // M5 修复：Android 语义 500=后台、200=感知服务
        match oom_adj {
            ..0 => Self::Interactive,
            0..=200 => Self::Interactive,
            201..=500 => Self::BackgroundLatencySensitive,
            501..=900 => Self::Background,
            _ => Self::Frozen,
        }
    }

    /// 多源增强推断：oom_adj + 前台状态 + 线程类型提示。
    pub fn from_sources(oom_adj: i32, is_foreground: bool, thread_hint: ThreadHint) -> Self {
        if is_foreground || oom_adj < 0 { return Self::Interactive; }
        if thread_hint == ThreadHint::LatencySensitive && oom_adj <= 700 {
            return Self::BackgroundLatencySensitive;
        }
        Self::from_oom_adj(oom_adj)
    }
}

/// 线程类型提示（从线程名启发式推断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadHint {
    Unknown,
    LatencySensitive,   // audio, binder, render
    ComputeIntensive,
}

impl ThreadHint {
    pub fn from_thread_name(name: &str) -> Self {
        let lower = name.to_lowercase();
        if lower.contains("audio") || lower.contains("binder:") || lower.contains("renderthread") || lower.contains("gl") {
            Self::LatencySensitive
        } else if lower.contains("worker") || lower.contains("pool") {
            Self::ComputeIntensive
        } else {
            Self::Unknown
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionLevel {
    Observe,
    Steer,
    Force,
}

/// 决策权重得分（P5 新增）。
/// 数值越大越倾向于干预。
#[derive(Debug, Clone, Copy)]
pub struct TaskScore {
    pub intent_weight: i32,     // TaskIntent 基础分
    pub pressure_mod: i32,      // 内存压力调节
    pub thermal_mod: i32,       // 温度调节
    pub total: i32,
}

impl TaskScore {
    pub fn to_action(self, force: bool) -> ActionLevel {
        if force { return ActionLevel::Force; }
        match self.total {
            n if n >= 40 => ActionLevel::Steer,
            _ => ActionLevel::Observe,
        }
    }
}

/// 决策引擎。
pub struct DecisionEngine {
    pub force_affinity_enabled: bool,
    pub pressure_sensitive: bool,
}

impl Default for DecisionEngine {
    fn default() -> Self {
        Self { force_affinity_enabled: false, pressure_sensitive: true }
    }
}

impl DecisionEngine {
    /// 决策入口（NEW-H1 终版：decide 完全等价 evaluate(thermal=0).to_action，
    /// 消除双路径不一致——Interactive 在高压下降级 Observe、Background 不再
    /// 无条件干预，都是压力感知的正确语义）。
    pub fn decide(&self, intent: TaskIntent, pressure: PressureLevel) -> ActionLevel {
        if self.force_affinity_enabled {
            return ActionLevel::Force;
        }
        self.evaluate(intent, pressure, 0.0).to_action(false)
    }

    /// 带权重的详细决策（P5.4）。
    /// `thermal_pressure`：冷却设备使用率 0.0~1.0（M7 修复，替代硬编码温度阈值）。
    pub fn evaluate(&self, intent: TaskIntent, pressure: PressureLevel, thermal_pressure: f64) -> TaskScore {
        let intent_weight = match intent {
            // 分析项-L3 (Claude)：Interactive 50→60，Moderate 压力下降级门槛提高
            //（60−15=45>40→仍 Steer），仅在 Critical 降级（60−40=20<40→Observe）
            TaskIntent::Interactive => 60,
            TaskIntent::BackgroundLatencySensitive => 50,
            TaskIntent::Background => 10,
            TaskIntent::Frozen => 0,
        };
        let pressure_mod = if self.pressure_sensitive {
            match pressure {
                PressureLevel::Normal => 0,
                PressureLevel::Moderate => -15,
                PressureLevel::Critical => -40,
            }
        } else { 0 };
        // M7 修复：冷却设备使用率阈值（0.8=重度热限制, 0.5=中度）
        let thermal_mod = if thermal_pressure > 0.8 { -20 }
            else if thermal_pressure > 0.5 { -10 }
            else { 0 };
        let total = (intent_weight + pressure_mod + thermal_mod).max(0);
        TaskScore { intent_weight, pressure_mod, thermal_mod, total }
    }

    pub fn migrate_action(&self) -> MigrateAction {
        if self.force_affinity_enabled { MigrateAction::Force } else { MigrateAction::Observe }
    }
}

/// CpuMigrate 策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MigrateAction {
    #[default]
    Observe,
    Suggest,
    Force,
}

// ────────────────────────────────────────────────────────────────
// P6.2-2: 正式决策接口（ChatGPT P6.2 审查：Decision 带原因，
// DecisionEngine 不读 proc，多时间尺度 Context）
// ────────────────────────────────────────────────────────────────

/// 决策原因（audit 可解释：Measure → Adjust 闭环需要知道"为什么"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// 前台交互进程，正常干预
    ForegroundInteractive,
    /// 延迟敏感后台（Binder 服务、音频线程），允许干预
    LatencySensitive,
    /// 后台进程（不干预，省电 + 交还调度器）
    Background,
    /// 冻结进程（永不干预）
    Frozen,
    /// 热压力（冷却设备使用率高，避免多余唤醒/迁移）
    ThermalPressure,
    /// 内存压力（Critical，压力感知降级）
    MemoryPressure,
    /// 审计失败率高（重应用大概率再失败，先观察记录）
    AuditFailureRate,
    /// 用户显式强制（force_affinity）
    ForceAffinity,
}

/// 降级级别（relock 场景：Skip 与 Degrade 都跳过本轮重应用，区别在记录）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DegradeLevel {
    /// 本轮跳过，下轮再试
    SkipOnce,
    /// 观察（持续记录，不干预）
    Observe,
    /// 降低干预强度（relock 层 = 暂不重应用）
    Relax,
}

/// 正式决策结果（带原因，ChatGPT P6.2 审查）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow { reason: Reason },
    Skip { reason: Reason },
    Degrade { level: DegradeLevel, reason: Reason },
}

/// 决策上下文（多时间尺度，ChatGPT P6.2 审查）：
/// - fast (1-5s)：pressure / foreground / thermal（突发事件）
/// - slow (30-60s)：audit failure rate（趋势）
/// 由调用方（engine/daemon）组装——DecisionEngine 本身不读 proc。
#[derive(Debug, Clone)]
pub struct DecisionContext {
    pub intent: TaskIntent,
    /// fast：内存压力等级
    pub pressure: PressureLevel,
    /// fast：冷却设备使用率 0.0~1.0
    pub thermal_pressure: f64,
    /// fast：是否前台（engine 用 intent==Interactive 近似，pid→uid 映射后续）
    pub foreground: bool,
    /// slow：审计失败率 0.0~1.0（窗口由调用方决定，默认 60s）
    pub audit_failure_rate: f64,
}

impl DecisionEngine {
    /// 正式决策入口（P6.2-2）：DecisionEngine = gate（Allow/Skip/Degrade），
    /// 不直接返回 Policy（ChatGPT P6.2 约束——避免决策侵入配置层）。
    ///
    /// 语义（relock 场景）：
    /// - Frozen / Background → Skip（永不干预 / 省电交还调度器）
    /// - 热压力 > 0.8 → Degrade(Relax)（避免多余唤醒/迁移）
    /// - 审计失败率 > 50% → Degrade(Observe)（大概率再失败，先观察）
    /// - 内存压力 Critical → Degrade(Relax)（压力感知降级）
    /// - 否则 → Allow（正常干预）
    pub fn decide_ctx(&self, ctx: &DecisionContext) -> Decision {
        if self.force_affinity_enabled {
            return Decision::Allow { reason: Reason::ForceAffinity };
        }
        match ctx.intent {
            TaskIntent::Frozen => return Decision::Skip { reason: Reason::Frozen },
            TaskIntent::Background => return Decision::Skip { reason: Reason::Background },
            // BUG-M3 修复 (Claude)：BLS 单独归因 LatencySensitive，
            // 不再混入 ForegroundInteractive——审计闭环需要区分前台游戏与后台 Binder 服务
            TaskIntent::BackgroundLatencySensitive => {} // 继续评估压力/热/审计
            TaskIntent::Interactive => {}
        }
        if ctx.thermal_pressure > 0.8 {
            return Decision::Degrade { level: DegradeLevel::Relax, reason: Reason::ThermalPressure };
        }
        if ctx.audit_failure_rate > 0.5 {
            return Decision::Degrade { level: DegradeLevel::Observe, reason: Reason::AuditFailureRate };
        }
        if self.pressure_sensitive && ctx.pressure == PressureLevel::Critical {
            return Decision::Degrade { level: DegradeLevel::Relax, reason: Reason::MemoryPressure };
        }
        // BUG-M3 修复：BLS 归因 LatencySensitive
        let reason = match ctx.intent {
            TaskIntent::BackgroundLatencySensitive => Reason::LatencySensitive,
            _ => Reason::ForegroundInteractive,
        };
        Decision::Allow { reason }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(intent: TaskIntent) -> DecisionContext {
        DecisionContext {
            intent,
            pressure: PressureLevel::Normal,
            thermal_pressure: 0.0,
            foreground: intent == TaskIntent::Interactive,
            audit_failure_rate: 0.0,
        }
    }

    #[test]
    fn interactive_steers_until_pressure() {
        // L3 修正 (Claude)：Interactive 60→Moderate 60-15=45>40→仍 Steer；
        // 仅在 Critical 降级 60-40=20<40→Observe
        let e = DecisionEngine::default();
        assert_eq!(e.decide(TaskIntent::Interactive, PressureLevel::Normal), ActionLevel::Steer);
        assert_eq!(e.decide(TaskIntent::Interactive, PressureLevel::Moderate), ActionLevel::Steer);
        assert_eq!(e.decide(TaskIntent::Interactive, PressureLevel::Critical), ActionLevel::Observe);
    }

    #[test]
    fn background_observes() {
        // NEW-H1 终版语义：Background 权重 10 < 阈值 40，任何压力下都 Observe
        // （与 relock 后台跳过语义一致，不再无条件干预）
        let e = DecisionEngine::default();
        assert_eq!(e.decide(TaskIntent::Background, PressureLevel::Normal), ActionLevel::Observe);
        assert_eq!(e.decide(TaskIntent::Background, PressureLevel::Critical), ActionLevel::Observe);
    }

    #[test]
    fn decide_matches_evaluate_full_matrix() {
        // NEW-H1 regression：decide 与 evaluate().to_action() 全矩阵一致
        let e = DecisionEngine::default();
        let intents = [
            TaskIntent::Interactive,
            TaskIntent::BackgroundLatencySensitive,
            TaskIntent::Background,
            TaskIntent::Frozen,
        ];
        let pressures = [
            PressureLevel::Normal,
            PressureLevel::Moderate,
            PressureLevel::Critical,
        ];
        for intent in intents {
            for pressure in pressures {
                let via_decide = e.decide(intent, pressure);
                let via_evaluate = e.evaluate(intent, pressure, 0.0).to_action(false);
                assert_eq!(
                    via_decide, via_evaluate,
                    "decide/evaluate mismatch: {intent:?} + {pressure:?}"
                );
            }
        }
    }

    #[test]
    fn frozen_never_intervenes() {
        assert_eq!(DecisionEngine::default().decide(TaskIntent::Frozen, PressureLevel::Normal), ActionLevel::Observe);
    }

    #[test]
    fn force_overrides() {
        let e = DecisionEngine { force_affinity_enabled: true, ..Default::default() };
        assert_eq!(e.decide(TaskIntent::Background, PressureLevel::Critical), ActionLevel::Force);
        assert_eq!(e.migrate_action(), MigrateAction::Force);
    }

    #[test]
    fn default_migrate_is_observe() {
        assert_eq!(DecisionEngine::default().migrate_action(), MigrateAction::Observe);
    }

    #[test]
    fn task_score_sums() {
        let e = DecisionEngine::default();
        // M7 修复后：第三参数为冷却设备使用率（0.0~1.0）
        let s = e.evaluate(TaskIntent::Interactive, PressureLevel::Normal, 0.1);
        assert_eq!(s.total, 60, "L3: Interactive weight 50→60");
        let s2 = e.evaluate(TaskIntent::Background, PressureLevel::Critical, 0.9);
        assert_eq!(s2.total, 0); // 10-40-20 → 0 (clamped)
    }

    #[test]
    fn multi_source_intent() {
        // 后台 + 音频线程 → 提升为 BackgroundLatencySensitive
        let intent = TaskIntent::from_sources(500, false, ThreadHint::LatencySensitive);
        assert_eq!(intent, TaskIntent::BackgroundLatencySensitive);
        // 前台忽略 oom_adj → Interactive
        let intent = TaskIntent::from_sources(500, true, ThreadHint::Unknown);
        assert_eq!(intent, TaskIntent::Interactive);
    }

    #[test]
    fn thread_hint_from_name() {
        assert_eq!(ThreadHint::from_thread_name("AudioTrack"), ThreadHint::LatencySensitive);
        assert_eq!(ThreadHint::from_thread_name("Binder:1234_5"), ThreadHint::LatencySensitive);
        assert_eq!(ThreadHint::from_thread_name("RenderThread"), ThreadHint::LatencySensitive);
        assert_eq!(ThreadHint::from_thread_name("GLThread"), ThreadHint::LatencySensitive);
        assert_eq!(ThreadHint::from_thread_name("ThreadPoolWorker"), ThreadHint::ComputeIntensive);
        assert_eq!(ThreadHint::from_thread_name("main"), ThreadHint::Unknown);
    }

    // ── P6.2-2: decide_ctx 测试矩阵 ──

    #[test]
    fn ctx_frozen_and_background_skip() {
        let e = DecisionEngine::default();
        assert_eq!(
            e.decide_ctx(&ctx(TaskIntent::Frozen)),
            Decision::Skip { reason: Reason::Frozen }
        );
        assert_eq!(
            e.decide_ctx(&ctx(TaskIntent::Background)),
            Decision::Skip { reason: Reason::Background }
        );
    }

    #[test]
    fn ctx_thermal_pressure_degrades() {
        let e = DecisionEngine::default();
        let mut c = ctx(TaskIntent::Interactive);
        c.thermal_pressure = 0.9;
        assert_eq!(
            e.decide_ctx(&c),
            Decision::Degrade { level: DegradeLevel::Relax, reason: Reason::ThermalPressure }
        );
    }

    #[test]
    fn ctx_audit_failure_rate_observes() {
        let e = DecisionEngine::default();
        let mut c = ctx(TaskIntent::BackgroundLatencySensitive);
        c.audit_failure_rate = 0.8;
        assert_eq!(
            e.decide_ctx(&c),
            Decision::Degrade { level: DegradeLevel::Observe, reason: Reason::AuditFailureRate }
        );
    }

    #[test]
    fn ctx_memory_pressure_critical_degrades() {
        let e = DecisionEngine::default();
        let mut c = ctx(TaskIntent::Interactive);
        c.pressure = PressureLevel::Critical;
        assert_eq!(
            e.decide_ctx(&c),
            Decision::Degrade { level: DegradeLevel::Relax, reason: Reason::MemoryPressure }
        );
    }

    #[test]
    fn ctx_normal_allows() {
        let e = DecisionEngine::default();
        assert_eq!(
            e.decide_ctx(&ctx(TaskIntent::Interactive)),
            Decision::Allow { reason: Reason::ForegroundInteractive }
        );
        assert_eq!(
            e.decide_ctx(&ctx(TaskIntent::BackgroundLatencySensitive)),
            Decision::Allow { reason: Reason::LatencySensitive },
            "BUG-M3 修复：BLS Allow 归因 LatencySensitive，不再混入 ForegroundInteractive"
        );
    }

    #[test]
    fn ctx_force_overrides() {
        let e = DecisionEngine { force_affinity_enabled: true, ..Default::default() };
        let mut c = ctx(TaskIntent::Frozen);
        c.thermal_pressure = 0.99;
        c.audit_failure_rate = 1.0;
        assert_eq!(
            e.decide_ctx(&c),
            Decision::Allow { reason: Reason::ForceAffinity }
        );
    }

    #[test]
    fn ctx_bls_not_degrades_on_moderate() {
        // BLS + Moderate 压力：不降级（只有 Critical 触发 MemoryPressure）
        let e = DecisionEngine::default();
        let mut c = ctx(TaskIntent::BackgroundLatencySensitive);
        c.pressure = PressureLevel::Moderate;
        assert_eq!(
            e.decide_ctx(&c),
            Decision::Allow { reason: Reason::LatencySensitive },
            "Moderate 压力不应降级，BLS 归因 LatencySensitive（M3 修复）"
        );
    }
}
