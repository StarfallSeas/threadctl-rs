//! Config model & parsing — KDL only（P8.1：TOML 已移除，不再复用）。
//!
//! Parsing (syntax) and compiling (semantics) are separated:
//! - `ConfigSnapshot`: 编译后不可变快照（版本号/daemon/engine/rules）
//! - `ConfigSnapshot`: compiled immutable snapshot (rule index + engine params)

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

use crate::decision::MigrateAction;
use crate::policy::SchedPolicy;
use crate::ruleset::RuleSet;
use crate::topology::CpuTopology;

/// 引擎模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineMode {
    /// 自动：优先 eBPF，失败降级 /proc
    Auto,
    /// 强制 eBPF
    Ebpf,
    /// 强制 /proc
    Proc,
    /// eBPF 主源 + /proc 低频补漏
    Hybrid,
}

impl Default for EngineMode {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub pid_file: String,
    pub ipc_socket: String,
    pub log_level: String,
}

fn default_pid_file() -> String {
    "./run/threadctl.pid".into()
}
fn default_ipc_socket() -> String {
    "./run/threadctl.sock".into()
}
fn default_log_level() -> String {
    "info".into()
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            pid_file: default_pid_file(),
            ipc_socket: default_ipc_socket(),
            log_level: default_log_level(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub mode: EngineMode,
    pub scan_interval: u64,
    /// 周期重锁定间隔（对 Android cgroup/AMS 覆盖）。
    pub lock_interval: u64,
    pub dead_cleanup_interval: u64,
    /// sched_switch CpuMigrate 保护模式（默认 observe，仅 force_affinity 时设为 Force）。
    pub migrate_action: MigrateAction,
    /// 是否允许系统压力下调策略强度。
    pub pressure_sensitive: bool,
}

fn default_scan_interval() -> u64 {
    2
}
fn default_lock_interval() -> u64 {
    60
}
fn default_dead_cleanup() -> u64 {
    15
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            mode: EngineMode::Auto,
            scan_interval: default_scan_interval(),
            lock_interval: default_lock_interval(),
            dead_cleanup_interval: default_dead_cleanup(),
            migrate_action: MigrateAction::Observe,
            pressure_sensitive: true,
        }
    }
}

/// sched 字段：`"fifo:60"` / `"batch"`。
#[derive(Debug, Clone, Copy)]
pub struct SchedSpec {
    pub policy: SchedPolicy,
    pub prio: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct RuleConfig {
    pub pkg: String,
    pub thread: String,
    /// H2 配套：允许缺省（sched-only 规则 = 白名单占位）
    pub cpus: String,
    pub sched: Option<String>,
    pub nice: Option<i32>,
    pub uclamp_min: Option<u32>,
    pub uclamp_max: Option<u32>,
}

/// 线程级配置（`[app."pkg".threads."name"]` 格式）。
#[derive(Debug, Clone, Default)]
pub struct ThreadConfig {
    pub cpus: Option<String>,
    pub sched: Option<String>,
    pub nice: Option<i32>,
    pub uclamp_min: Option<u32>,
    pub uclamp_max: Option<u32>,
}

/// APP 级配置（`[app."pkg"]` 格式，不含包名重复）。
#[derive(Debug, Clone, Default)]
pub struct AppConfig {
    /// P6.0：内置 profile 名（game/audio/launcher/balanced/power-save）
    pub profile: Option<String>,
    /// 默认 CPU 范围（所有线程）
    pub cpus: Option<String>,
    pub sched: Option<String>,
    pub nice: Option<i32>,
    pub uclamp_min: Option<u32>,
    pub uclamp_max: Option<u32>,
    /// 线程级覆盖
    pub threads: HashMap<String, ThreadConfig>,
}

impl RuleConfig {
    pub fn sched_spec(&self) -> Option<SchedSpec> {
        let s = self.sched.as_deref()?;
        let (name, prio) = match s.split_once(':') {
            Some((n, p)) => (n, p.parse::<i32>().ok()),
            None => (s, None),
        };
        let policy = SchedPolicy::from_str(name)?;
        Some(SchedSpec { policy, prio })
    }
}

/// 编译后的不可变快照 —— 在模块间流通的唯一配置形态。
pub struct ConfigSnapshot {
    /// 版本号，每次成功重载递增。
    pub version: u64,
    pub config_file: String,
    pub daemon: DaemonConfig,
    pub engine: EngineConfig,
    pub rules: RuleSet,
}

impl ConfigSnapshot {
    /// 解析 + 编译配置；解析失败返回错误信息。
    pub fn load(
        config_file: &str,
        topo: &CpuTopology,
        version: u64,
    ) -> Result<Arc<ConfigSnapshot>, String> {
        let content = fs::read_to_string(config_file)
            .map_err(|e| format!("config read failed: {e}"))?;

        // P8.1：仅 KDL（TOML 已移除）——非 .kdl 明确报错提示迁移
        if !config_file.ends_with(".kdl") {
            return Err(format!(
                "仅支持 KDL 配置（{config_file} 不是 .kdl）；TOML 格式已移除，请使用 threadctl.kdl"
            ));
        }
        let (model, daemon, engine) = crate::kdl_parser::parse_kdl(&content)
            .map_err(|e| format!("KDL parse failed: {e}"))?;
        let all_rules = model.to_rules_with_clusters(&topo.clusters);

        let compile = RuleSet::compile(&all_rules, topo);
        if !compile.errors.is_empty() {
            let detail: Vec<String> = compile.errors.iter().map(|e| e.to_string()).collect();
            // 无效规则跳过不致命，但显著提示。
            eprintln!("warning: {} rules invalid: {}", compile.errors.len(), detail.join("; "));
        }

        Ok(Arc::new(ConfigSnapshot {
            version,
            config_file: config_file.to_string(),
            daemon,
            engine,
            rules: compile.rules,
        }))
    }

    /// 生成默认配置模板。
    pub fn default_template() -> String {
        include_str!("../config/threadctl.kdl").to_string()
    }
}

// ── ConfigModel — 格式无关的配置 AST ─────────────────────────
//
// TOML / KDL / 任何未来格式都先转换为 ConfigModel，
// 再由 ConfigModel 展开为 Vec<RuleConfig>。
// Engine 始终不变。

/// 策略片段（可复用于 default、thread、thread_type）。
#[derive(Debug, Clone, Default)]
pub struct PolicyModel {
    pub cluster: Option<String>,
    pub cpus: Option<String>,
    pub uclamp_min: Option<u32>,
    pub uclamp_max: Option<u32>,
    pub sched: Option<String>,
    pub nice: Option<i32>,
}

/// 单个 APP 的配置节点。
#[derive(Debug, Clone)]
pub struct AppModel {
    pub pkg: String,
    /// P6.0：内置 profile 名（展开时先取模板，再应用用户覆盖）
    pub profile: Option<String>,
    pub default_policy: PolicyModel,
    pub threads: HashMap<String, PolicyModel>,
    pub thread_types: HashMap<String, PolicyModel>,
}

/// 格式无关的配置中间表示。
#[derive(Debug, Clone)]
pub struct ConfigModel {
    pub apps: HashMap<String, AppModel>,
}

/// 内置线程类型→fnmatch 别名表。
const THREAD_TYPE_ALIASES: &[(&str, &[&str])] = &[
    ("render", &["*RenderThread*", "*GL*", "*Vk*"]),
    ("audio", &["*Audio*", "*Sound*"]),
    ("binder", &["*Binder:*"]),
    ("main", &["*main*"]),
];

impl ConfigModel {
    /// 展开为规则列表（KDL AST → RuleConfig）。
    pub fn to_rules(&self) -> Vec<RuleConfig> {
        self.to_rules_with_clusters(&[])
    }

    /// 展开时解析 cluster 名称为 CPU 范围。
    pub fn to_rules_with_clusters(&self, clusters: &[crate::topology::CpuCluster]) -> Vec<RuleConfig> {
        let mut rules = Vec::new();
        for app in self.apps.values() {
            // P6.0：profile 模板展开（用户显式配置覆盖模板，覆盖顺序：thread > default > profile）
            let (mut default_pol, mut threads, mut thread_types) = match &app.profile {
                Some(name) => match crate::profile::builtin_profiles().get(name.as_str()) {
                    Some(p) => (p.default.clone(), p.threads.clone(), p.thread_types.clone()),
                    None => {
                        eprintln!("warning: unknown profile \"{name}\" (app {}), falling back to default", app.pkg);
                        (PolicyModel::default(), HashMap::new(), HashMap::new())
                    }
                },
                None => (PolicyModel::default(), HashMap::new(), HashMap::new()),
            };
            override_policy(&mut default_pol, &app.default_policy);
            for (k, v) in &app.threads {
                let entry = threads.entry(k.clone()).or_default();
                override_policy(entry, v);
            }
            for (k, v) in &app.thread_types {
                let entry = thread_types.entry(k.clone()).or_default();
                override_policy(entry, v);
            }

            let default_rules = policy_to_rules(&app.pkg, "", &default_pol, clusters);
            let has_default = !default_rules.is_empty();
            rules.extend(default_rules);

            for (thread, pol) in &threads {
                rules.extend(policy_to_rules(&app.pkg, thread, pol, clusters));
            }
            for (ttype, pol) in &thread_types {
                if let Some(patterns) = thread_type_patterns(ttype) {
                    for pat in patterns {
                        rules.extend(policy_to_rules(&app.pkg, pat, pol, clusters));
                    }
                }
            }

            // 无线程规则且无默认规则时，仍需要一个占位让白名单包含此包
            let has_any = has_default
                || threads.values().any(|p| has_cpus(p))
                || thread_types.values().any(|p| has_cpus(p));
            if !has_any {
                rules.push(RuleConfig {
                    pkg: app.pkg.clone(),
                    thread: String::new(),
                    cpus: String::new(),
                    sched: None, nice: None,
                    uclamp_min: None, uclamp_max: None,
                });
            }
        }
        rules
    }
}

/// P6.0：用户显式字段覆盖模板（有值即覆盖；无值保留模板）。
/// cpus 与 cluster 互斥：用户显式写 cpus 时清除模板 cluster，反之亦然。
fn override_policy(base: &mut PolicyModel, user: &PolicyModel) {
    if user.cluster.is_some() {
        base.cluster = user.cluster.clone();
        base.cpus = None;
    }
    if user.cpus.is_some() {
        base.cpus = user.cpus.clone();
        base.cluster = None;
    }
    if user.sched.is_some() {
        base.sched = user.sched.clone();
    }
    if user.nice.is_some() {
        base.nice = user.nice;
    }
    if user.uclamp_min.is_some() {
        base.uclamp_min = user.uclamp_min;
    }
    if user.uclamp_max.is_some() {
        base.uclamp_max = user.uclamp_max;
    }
}

fn has_cpus(p: &PolicyModel) -> bool {
    p.cpus.is_some() || p.cluster.is_some()
}

/// 是否有任何策略字段（cpus/cluster/sched/nice/uclamp）——空 policy 不产规则。
fn has_any_policy(p: &PolicyModel) -> bool {
    has_cpus(p)
        || p.sched.is_some()
        || p.nice.is_some()
        || p.uclamp_min.is_some()
        || p.uclamp_max.is_some()
}

/// thread-type 别名展开（P6.0：render/audio/binder/main 内置模式）。
fn thread_type_patterns(ttype: &str) -> Option<&'static [&'static str]> {
    THREAD_TYPE_ALIASES
        .iter()
        .find(|(name, _)| *name == ttype)
        .map(|(_, patterns)| *patterns)
}

fn policy_to_rules(
    pkg: &str,
    thread: &str,
    pol: &PolicyModel,
    clusters: &[crate::topology::CpuCluster],
) -> Vec<RuleConfig> {
    // 空 policy（无任何字段）→ 零规则（占位规则仅在 has_any 判定时补）
    if !has_any_policy(pol) {
        return Vec::new();
    }
    // cpus 优先；cluster 名 → 集群范围（P6.3 容错：数字范围自动当 cpus）
    let cpus = pol.cpus.clone().or_else(|| {
        pol.cluster.as_ref().and_then(|name| {
            clusters
                .iter()
                .find(|c| format!("{:?}", c.kind).to_lowercase() == name.to_lowercase())
                .map(|c| c.range_str.clone())
                .or_else(|| {
                    // cluster 写成了数字范围（如 "0-6"）→ 容错当 cpus
                    if name.chars().all(|ch| ch.is_ascii_digit() || ch == '-' || ch == ',') {
                        Some(name.clone())
                    } else {
                        eprintln!("warning: app {:?} cluster {:?} not available on this SoC — using big cluster instead", pkg, name);
                        // 同档近似：优先 Big 集群；无 Big 时取最大 capacity（P6.3 全大核设备）
                        clusters.iter().find(|c| c.kind == crate::topology::CpuClusterKind::Big)
                            .or_else(|| clusters.iter().max_by_key(|c| c.capacity))
                            .map(|c| c.range_str.clone())
                    }
                })
        })
    });
    vec![RuleConfig {
        pkg: pkg.to_string(),
        thread: thread.to_string(),
        cpus: cpus.unwrap_or_default(),
        sched: pol.sched.clone(),
        nice: pol.nice,
        uclamp_min: pol.uclamp_min,
        uclamp_max: pol.uclamp_max,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let kdl = r#"
            daemon { pid-file "./run/x.pid" }
            engine { mode "hybrid"; scan-interval 3 }
            app "com.example" {
                default { cpus "0-3" }
                thread "RenderThread" { cpus "4-7"; sched "fifo:60"; nice -10 }
            }
        "#;
        let (model, _d, engine) = crate::kdl_parser::parse_kdl(kdl).expect("parse");
        assert_eq!(engine.mode, EngineMode::Hybrid);
        assert_eq!(engine.scan_interval, 3);
        let rules = model.to_rules();
        assert_eq!(rules.len(), 2);
        let spec = rules[1].sched_spec().expect("sched");
        assert_eq!(spec.policy, SchedPolicy::Fifo);
        assert_eq!(spec.prio, Some(60));
    }

    #[test]
    fn defaults_applied() {
        let kdl = "// empty\n";
        let (_m, _d, engine) = crate::kdl_parser::parse_kdl(kdl).expect("parse");
        assert_eq!(engine.mode, EngineMode::Auto);
        assert_eq!(engine.lock_interval, 60);
    }

    #[test]
    fn app_format_parses() {
        let kdl = r#"
            app "com.tencent.mm" {
                cpus "0-6"
                thread "RenderThread" { cpus "7"; sched "fifo:60" }
                thread "AudioThread" { cpus "0-3"; nice -10 }
            }
        "#;
        let (model, _d, _e) = crate::kdl_parser::parse_kdl(kdl).expect("parse");
        assert_eq!(model.apps.len(), 1);
        let mm = model.apps.get("com.tencent.mm").unwrap();
        assert_eq!(mm.default_policy.cpus.as_deref(), Some("0-6"));
        assert_eq!(mm.threads.len(), 2);
    }

    #[test]
    fn merged_format_loads() {
        let kdl = r#"
            app "com.a" {
                cpus "0-6"
                thread "RenderThread" { cpus "7" }
            }
            app "com.b" { default { cpus "0-3" } }
        "#;
        let (model, _d, _e) = crate::kdl_parser::parse_kdl(kdl).expect("parse");
        let rules = model.to_rules();
        // app "com.a" → 2 rules (default + RenderThread), "com.b" → 1 = 3
        assert_eq!(rules.len(), 3);
    }

    #[test]
    fn multi_pkg_rules_merge_or() {
        // P8.1 KDL 语义：同包多条 default 节点 = 后写覆盖（TOML [[rule]]
        // 时代的"按位或合并"随 TOML 移除——KDL 树形配置用显式 default 唯一表达）
        let kdl = r#"
            app "com.x" {
                default { cpus "0-3" }
                default { cpus "4-7" }
                default { sched "fifo:60" }
            }
        "#;
        let (model, _d, _e) = crate::kdl_parser::parse_kdl(kdl).expect("parse");
        let rules = model.to_rules();
        // 最后 default 完全覆盖 → 1 条规则，cpus 空（sched-only 占位）
        assert_eq!(rules.len(), 1, "多次 default 覆盖为一条");
        assert_eq!(rules[0].cpus, "", "最后一个 default（仅 sched）覆盖 cpus");
        assert_eq!(rules[0].sched.as_deref(), Some("fifo:60"), "sched 保留");
    }

    #[test]
    fn same_thread_rules_merge() {
        // P8.1 KDL 语义：同包同名线程节点 = 后写覆盖（原 TOML 按位或随移除）
        let kdl = r#"
            app "com.x" {
                thread "RenderThread" { cpus "0-3" }
                thread "RenderThread" { cpus "7"; sched "fifo:60" }
            }
        "#;
        let (model, _d, _e) = crate::kdl_parser::parse_kdl(kdl).expect("parse");
        let rules = model.to_rules();
        assert_eq!(rules.len(), 1, "同名线程节点覆盖为一条");
        assert_eq!(rules[0].cpus, "7", "后写 cpus 覆盖");
        assert_eq!(rules[0].sched.as_deref(), Some("fifo:60"));
    }
}

#[cfg(test)]
mod h2_tests {
    use super::*;

    #[test]
    fn sched_only_rule_is_whitelist_placeholder() {
        // H2 回归测试：sched-only 规则（无 cpus）应作为白名单占位合法编译
        let kdl = r#"
            app "com.schedonly" { default { sched "fifo:60" } }
        "#;
        let (model, _d, _e) = crate::kdl_parser::parse_kdl(kdl).expect("parse");
        let rules = model.to_rules();
        // sched-only → PolicyModel 只有 sched → 展开为 cpus="" 的占位规则
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].cpus, "", "占位规则无 CPU 约束");
        assert_eq!(rules[0].sched.as_deref(), Some("fifo:60"));

        // 占位规则必须能通过 RuleSet::compile（白名单包含此包）
        let topo = crate::topology::CpuTopology::default();
        let compile = RuleSet::compile(&rules, &topo);
        assert!(compile.errors.is_empty(), "占位规则不应报错: {:?}", compile.errors);
        assert!(compile.rules.is_interested("com.schedonly"), "白名单应包含此包");
    }
}

#[cfg(test)]
mod profile_tests {
    use super::*;

    #[test]
    fn profile_expands_template_kdl() {
        // P6.0：profile "game" 展开为内置模板 + 用户覆盖
        let kdl = r#"
            app "com.example.game" {
                profile "game"
                thread "RenderThread" { cpus "7" }
            }
        "#;
        let (model, _d, _e) = crate::kdl_parser::parse_kdl(kdl).expect("parse");
        let rules = model.to_rules();
        // RenderThread 用户覆盖 cpus="7" → 1 条线程规则
        let rt = rules.iter().find(|r| r.thread == "RenderThread").expect("RenderThread 规则");
        assert_eq!(rt.cpus, "7", "用户 cpus 应覆盖模板");
        assert_eq!(rt.pkg, "com.example.game");
    }

    #[test]
    fn profile_override_beats_template() {
        // 用户显式 default 覆盖 profile 模板的 default
        let kdl = r#"
            app "com.x" {
                profile "audio"
                cpus "0-3"
            }
        "#;
        let (model, _d, _e) = crate::kdl_parser::parse_kdl(kdl).expect("parse");
        let rules = model.to_rules();
        let def = rules.iter().find(|r| r.thread.is_empty()).expect("default 规则");
        assert_eq!(def.cpus, "0-3", "用户 cpus 应覆盖 audio 模板的 little 集群");
    }

    #[test]
    fn unknown_profile_warns_and_falls_back() {
        let kdl = r#"
            app "com.x" { profile "nonexistent" }
        "#;
        let (model, _d, _e) = crate::kdl_parser::parse_kdl(kdl).expect("parse");
        let rules = model.to_rules();
        // 未知 profile → 空模板 → 无策略 → 占位规则
        assert_eq!(rules.len(), 1);
        assert!(rules[0].cpus.is_empty());
    }

    // ── P6.3 M3：cluster mid + BUG-M2 fallback 测试 ──

    fn mk_cluster(kind: crate::topology::CpuClusterKind, cpus: &str, cap: u32) -> crate::topology::CpuCluster {
        let set = crate::topology::parse_cpu_ranges(cpus, None);
        crate::topology::CpuCluster {
            kind,
            cpus: set.clone(),
            range_str: set.to_range_string(),
            capacity: cap,
        }
    }

    fn mk_pol_cluster(name: &str) -> PolicyModel {
        PolicyModel {
            cpus: None,
            cluster: Some(name.into()),
            sched: None,
            nice: None,
            uclamp_min: None,
            uclamp_max: None,
        }
    }

    #[test]
    fn cluster_mid_resolves_on_four_group() {
        // SM8650 4 组：cluster "mid" → 中核 2-3（P6.3 M3 核心）
        use crate::topology::CpuClusterKind as K;
        let clusters = vec![
            mk_cluster(K::Little, "0-1", 240),
            mk_cluster(K::Mid, "2-3", 560),
            mk_cluster(K::Big, "4-6", 720),
            mk_cluster(K::Prime, "7", 1024),
        ];
        let rules = policy_to_rules("com.x", "", &mk_pol_cluster("mid"), &clusters);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].cpus, "2-3", "cluster mid 应解析为中核组");
    }

    #[test]
    fn cluster_little_fallback_to_big_on_all_big() {
        // CLAUDE BUG-M2：全大核 SoC（SM8750）上 cluster "little" 应 fallback
        // 到 Big(0-5)，而非静默取最高容量 Prime(6-7)
        use crate::topology::CpuClusterKind as K;
        let clusters = vec![
            mk_cluster(K::Big, "0-5", 700),
            mk_cluster(K::Prime, "6-7", 900),
        ];
        let rules = policy_to_rules("com.x", "", &mk_pol_cluster("little"), &clusters);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].cpus, "0-5", "little 不可用 → 同档近似 big");
    }

    #[test]
    fn cluster_big_fallback_on_single_unknown() {
        // 单集群 Unknown（同构 SoC）：cluster "big" → 唯一集群兜底
        use crate::topology::CpuClusterKind as K;
        let clusters = vec![mk_cluster(K::Unknown, "0-7", 800)];
        let rules = policy_to_rules("com.x", "", &mk_pol_cluster("big"), &clusters);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].cpus, "0-7", "单集群兜底全部 CPU");
    }

    #[test]
    fn cluster_exact_hit_no_fallback() {
        // 精确命中不触发 fallback（保持既有语义）
        use crate::topology::CpuClusterKind as K;
        let clusters = vec![
            mk_cluster(K::Little, "0-2", 280),
            mk_cluster(K::Big, "3-6", 855),
            mk_cluster(K::Prime, "7", 1024),
        ];
        let rules = policy_to_rules("com.x", "", &mk_pol_cluster("big"), &clusters);
        assert_eq!(rules[0].cpus, "3-6", "big 精确命中");
    }
}
