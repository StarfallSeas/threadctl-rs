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
            TaskIntent::Interactive => 50,
            // NEW-H1 (Claude): BLS 权重 30→50，使 evaluate().to_action() 与 decide()
            // 一致（BLS+Normal → Steer；BLS+Critical → Observe）
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_steers_until_pressure() {
        // NEW-H1 终版语义：Interactive 正常/中度压力下 Steer，
        // Critical 压力下压力感知降级 Observe（evaluate 路径）
        let e = DecisionEngine::default();
        assert_eq!(e.decide(TaskIntent::Interactive, PressureLevel::Normal), ActionLevel::Steer);
        assert_eq!(e.decide(TaskIntent::Interactive, PressureLevel::Moderate), ActionLevel::Observe);
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
        assert_eq!(s.total, 50); // 50+0+0
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
}
