//! Config model & parsing — TOML + serde, structurally compatible with threadctl v1.
//!
//! Parsing (syntax) and compiling (semantics) are separated:
//! - `RawConfig`: serde-deserialized raw structure (with defaults)
//! - `ConfigSnapshot`: compiled immutable snapshot (rule index + engine params)

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

use crate::decision::MigrateAction;
use crate::policy::SchedPolicy;
use crate::ruleset::RuleSet;
use crate::topology::CpuTopology;

/// 引擎模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
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

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DaemonConfig {
    #[serde(default = "default_pid_file")]
    pub pid_file: String,
    #[serde(default = "default_ipc_socket")]
    pub ipc_socket: String,
    #[serde(default = "default_log_level")]
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

#[derive(Debug, Clone, serde::Deserialize)]
pub struct EngineConfig {
    #[serde(default)]
    pub mode: EngineMode,
    #[serde(default = "default_scan_interval")]
    pub scan_interval: u64,
    /// 周期重锁定间隔（对 Android cgroup/AMS 覆盖）。
    #[serde(default = "default_lock_interval")]
    pub lock_interval: u64,
    #[serde(default = "default_dead_cleanup")]
    pub dead_cleanup_interval: u64,
    /// sched_switch CpuMigrate 保护模式（默认 observe，仅 force_affinity 时设为 Force）。
    #[serde(default)]
    pub migrate_action: MigrateAction,
    /// 是否允许系统压力下调策略强度。
    #[serde(default = "default_true")]
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
fn default_true() -> bool { true }

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

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RuleConfig {
    pub pkg: String,
    #[serde(default)]
    pub thread: String,
    /// H2 配套：允许缺省（sched-only 规则 = 白名单占位）
    #[serde(default)]
    pub cpus: String,
    #[serde(default)]
    pub sched: Option<String>,
    #[serde(default)]
    pub nice: Option<i32>,
    #[serde(default)]
    pub uclamp_min: Option<u32>,
    #[serde(default)]
    pub uclamp_max: Option<u32>,
}

/// 线程级配置（`[app."pkg".threads."name"]` 格式）。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ThreadConfig {
    pub cpus: Option<String>,
    pub sched: Option<String>,
    pub nice: Option<i32>,
    pub uclamp_min: Option<u32>,
    pub uclamp_max: Option<u32>,
}

/// APP 级配置（`[app."pkg"]` 格式，不含包名重复）。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct AppConfig {
    /// P6.0：内置 profile 名（game/audio/launcher/balanced/power-save）
    #[serde(default)]
    pub profile: Option<String>,
    /// 默认 CPU 范围（所有线程）
    pub cpus: Option<String>,
    pub sched: Option<String>,
    pub nice: Option<i32>,
    pub uclamp_min: Option<u32>,
    pub uclamp_max: Option<u32>,
    /// 线程级覆盖
    #[serde(default)]
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

/// 顶层配置（serde 结构）。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct RawConfig {
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub engine: EngineConfig,
    /// 旧格式 [[rule]] — 向后兼容
    #[serde(default)]
    pub rule: Vec<RuleConfig>,
    /// 新格式 [app] — 推荐
    #[serde(default)]
    pub app: HashMap<String, AppConfig>,
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

        // 自动检测格式：.kdl → KDL，其余 → TOML
        let (model, daemon, engine) = if config_file.ends_with(".kdl") {
            #[cfg(feature = "kdl")]
            {
                // M3 修复：KDL 现在支持 daemon/engine 顶层节点
                crate::kdl_parser::parse_kdl(&content)
                    .map_err(|e| format!("KDL parse failed: {e}"))?
            }
            #[cfg(not(feature = "kdl"))]
            return Err("KDL 支持未编译（需启用 kdl feature）".into());
        } else {
            let raw: RawConfig = toml::from_str(&content)
                .map_err(|e| format!("config syntax error: {e}"))?;
            let m = ConfigModel::from_toml(&raw);
            (m, raw.daemon, raw.engine)
        };
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
        include_str!("../config/threadctl.toml").to_string()
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
    /// 从 TOML RawConfig 构建。
    pub fn from_toml(raw: &RawConfig) -> Self {
        let mut apps: HashMap<String, AppModel> = HashMap::new();

        // ① [[rule]] → ConfigModel
        for r in &raw.rule {
            let entry = apps.entry(r.pkg.clone()).or_insert_with(|| AppModel {
                pkg: r.pkg.clone(),
                profile: None,
                default_policy: PolicyModel::default(),
                threads: HashMap::new(),
                thread_types: HashMap::new(),
            });
            let pol = PolicyModel {
                cpus: if r.cpus.is_empty() { None } else { Some(r.cpus.clone()) },
                sched: r.sched.clone(),
                nice: r.nice,
                uclamp_min: r.uclamp_min,
                uclamp_max: r.uclamp_max,
                cluster: None,
            };
            if r.thread.is_empty() {
                // H1 修复：多条包级规则字段级合并（cpus 按位或，其余首个生效），
                // 复刻旧 RuleSet::resolve 的 OR 合并语义
                merge_policy(&mut entry.default_policy, pol);
            } else {
                let thread_pol = entry.threads.entry(r.thread.clone()).or_default();
                merge_policy(thread_pol, pol);
            }
        }

        // ② [app] → ConfigModel
        for (pkg, ac) in &raw.app {
            let entry = apps.entry(pkg.clone()).or_insert_with(|| AppModel {
                pkg: pkg.clone(),
                profile: ac.profile.clone(),
                default_policy: PolicyModel::default(),
                threads: HashMap::new(),
                thread_types: HashMap::new(),
            });
            if entry.profile.is_none() {
                entry.profile = ac.profile.clone();
            }
            if ac.cpus.is_some() || ac.sched.is_some() {
                // Claude 审查 1.3：改为 merge 而非全量替换——
                // [[rule]] 与 [app] 混用同一 pkg 时字段不丢失。
                // （[app] 定义的是该格式自己的字段，merge 语义：cpus 按位或、其余首个生效）
                let ac_pol = PolicyModel {
                    cpus: ac.cpus.clone(),
                    sched: ac.sched.clone(),
                    nice: ac.nice,
                    uclamp_min: ac.uclamp_min,
                    uclamp_max: ac.uclamp_max,
                    cluster: None,
                };
                merge_policy(&mut entry.default_policy, ac_pol);
            }
            for (tname, tc) in &ac.threads {
                let thread_pol = entry.threads.entry(tname.clone()).or_default();
                merge_policy(thread_pol, PolicyModel {
                    cpus: tc.cpus.clone(),
                    sched: tc.sched.clone(),
                    nice: tc.nice,
                    uclamp_min: tc.uclamp_min,
                    uclamp_max: tc.uclamp_max,
                    cluster: None,
                });
            }
        }

        ConfigModel { apps }
    }

    /// 展开为引擎需要的 RuleConfig 列表。
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

/// 判断字符串是否"看起来像 CPU 范围"（纯数字/逗号/连字符）。
/// 用户友好容错：`cluster "0-6"` 自动按 cpus 处理，不再告警。
fn looks_like_cpu_range(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == ',' || c == ' ' || c == '\t')
        && s.chars().any(|c| c.is_ascii_digit())
}

fn has_cpus(p: &PolicyModel) -> bool {
    p.cpus.is_some() || p.cluster.is_some()
}

/// 字段级合并两条策略（H1 修复）。
/// 复刻旧 RuleSet::resolve 语义：
/// - cpus：按位或合并（多条规则范围叠加）
/// - cluster/sched/nice/uclamp：首个非 None 生效（后续不覆盖）
fn merge_policy(dst: &mut PolicyModel, src: PolicyModel) {
    if let Some(new_cpus) = src.cpus {
        match &mut dst.cpus {
            Some(old) => {
                let mut set = crate::topology::parse_cpu_ranges(old, None);
                set.or(&crate::topology::parse_cpu_ranges(&new_cpus, None));
                dst.cpus = Some(set.to_range_string());
            }
            None => dst.cpus = Some(new_cpus),
        }
    }
    if dst.cluster.is_none() {
        dst.cluster = src.cluster;
    }
    if dst.sched.is_none() {
        dst.sched = src.sched;
    }
    if dst.nice.is_none() {
        dst.nice = src.nice;
    }
    if dst.uclamp_min.is_none() {
        dst.uclamp_min = src.uclamp_min;
    }
    if dst.uclamp_max.is_none() {
        dst.uclamp_max = src.uclamp_max;
    }
}

fn thread_type_patterns(ttype: &str) -> Option<&[&str]> {
    THREAD_TYPE_ALIASES
        .iter()
        .find(|(k, _)| *k == ttype)
        .map(|(_, v)| *v)
}

fn policy_to_rules(pkg: &str, thread: &str, pol: &PolicyModel, clusters: &[crate::topology::CpuCluster]) -> Vec<RuleConfig> {
    let cpus = if let Some(ref cluster_name) = pol.cluster {
        // 用户友好容错：cluster 字段写入数字范围（如 "0-6"、"4-7"）时
        // 自动按 cpus 处理——消灭 miui.home 类"cluster 写错"的坑。
        if looks_like_cpu_range(cluster_name) {
            Some(cluster_name.clone())
        } else {
            // 集群名解析：合法名（little/big/prime）但设备无此集群时
            // fallback 到容量最大的集群；非法名仍告警跳过。
            let is_valid_name = matches!(
                cluster_name.to_lowercase().as_str(),
                "little" | "big" | "prime"
            );
            let found = clusters
                .iter()
                .find(|c| format!("{:?}", c.kind).to_lowercase() == cluster_name.to_lowercase())
                .map(|c| c.cpus.to_range_string())
                .or_else(|| {
                    if is_valid_name {
                        clusters
                            .last() // detect_clusters 按容量升序，最后是最大容量集群
                            .map(|c| c.cpus.to_range_string())
                    } else {
                        None
                    }
                });
            if found.is_none() && !clusters.is_empty() {
                // 告警而非静默丢弃：cluster 名写错（如 "0-6"）导致规则不生效是隐形 bug
                let valid: Vec<String> = clusters
                    .iter()
                    .map(|c| format!("{:?}", c.kind).to_lowercase())
                    .collect();
                eprintln!(
                    "warning: app \"{pkg}\"{} cluster \"{cluster_name}\" invalid (available: {}) — rule skipped; use cpus or a valid cluster name",
                    if thread.is_empty() { String::new() } else { format!(" thread \"{thread}\"") },
                    valid.join(" / ")
                );
            }
            found
        }
    } else {
        pol.cpus.clone()
    };

    if cpus.is_none() && pol.sched.is_none() && pol.nice.unwrap_or(0) == 0 {
        return vec![];
    }

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
        let toml = r#"
            [daemon]
            pid_file = "./run/x.pid"

            [engine]
            mode = "hybrid"
            scan_interval = 3

            [[rule]]
            pkg = "com.example"
            cpus = "0-3"

            [[rule]]
            pkg = "com.example"
            thread = "RenderThread"
            cpus = "4-7"
            sched = "fifo:60"
            nice = -10
        "#;
        let raw: RawConfig = toml::from_str(toml).expect("parse");
        assert_eq!(raw.engine.mode, EngineMode::Hybrid);
        assert_eq!(raw.engine.scan_interval, 3);
        assert_eq!(raw.rule.len(), 2);
        let spec = raw.rule[1].sched_spec().expect("sched");
        assert_eq!(spec.policy, SchedPolicy::Fifo);
        assert_eq!(spec.prio, Some(60));
    }

    #[test]
    fn defaults_applied() {
        let toml = "# empty\n";
        let raw: RawConfig = toml::from_str(toml).expect("parse");
        assert_eq!(raw.engine.mode, EngineMode::Auto);
        assert_eq!(raw.engine.lock_interval, 60);
        assert!(raw.rule.is_empty());
    }

    #[test]
    fn app_format_parses() {
        let toml = r#"
            [app."com.tencent.mm"]
            cpus = "0-6"

            [app."com.tencent.mm".threads.RenderThread]
            cpus = "7"
            sched = "fifo:60"

            [app."com.tencent.mm".threads.AudioThread]
            cpus = "0-3"
            nice = -10
        "#;
        let raw: RawConfig = toml::from_str(toml).expect("parse");
        assert_eq!(raw.app.len(), 1);
        let mm = raw.app.get("com.tencent.mm").unwrap();
        assert_eq!(mm.cpus.as_deref(), Some("0-6"));
        assert_eq!(mm.threads.len(), 2);
    }

    #[test]
    fn merged_format_loads() {
        let toml = r#"
            [app."com.a"]
            cpus = "0-6"

            [app."com.a".threads.RenderThread]
            cpus = "7"

            [[rule]]
            pkg = "com.b"
            cpus = "0-3"
        "#;
        let raw: RawConfig = toml::from_str(toml).expect("parse");
        let model = ConfigModel::from_toml(&raw);
        let rules = model.to_rules();
        // app "com.a" → 2 rules (default + RenderThread), rule "com.b" → 1 = 3
        assert_eq!(rules.len(), 3);
    }

    #[test]
    fn multi_pkg_rules_merge_or() {
        // H1 回归测试：同包多条包级规则 cpus 按位或合并、sched 首个生效
        let toml = r#"
            [[rule]]
            pkg = "com.x"
            cpus = "0-3"

            [[rule]]
            pkg = "com.x"
            cpus = "4-7"

            [[rule]]
            pkg = "com.x"
            sched = "fifo:60"
        "#;
        let raw: RawConfig = toml::from_str(toml).expect("parse");
        let model = ConfigModel::from_toml(&raw);
        let rules = model.to_rules();
        // 合并后：default 一条规则，cpus = 0-7，sched = fifo:60
        assert_eq!(rules.len(), 1, "三条包级规则应合并为一条");
        assert_eq!(rules[0].cpus, "0-7", "cpus 应按位或合并");
        assert_eq!(rules[0].sched.as_deref(), Some("fifo:60"), "sched 应保留");
    }

    #[test]
    fn same_thread_rules_merge() {
        // H1 回归测试：同包同名线程规则合并
        let toml = r#"
            [[rule]]
            pkg = "com.x"
            thread = "RenderThread"
            cpus = "0-3"

            [[rule]]
            pkg = "com.x"
            thread = "RenderThread"
            cpus = "7"
            sched = "fifo:60"
        "#;
        let raw: RawConfig = toml::from_str(toml).expect("parse");
        let model = ConfigModel::from_toml(&raw);
        let rules = model.to_rules();
        assert_eq!(rules.len(), 1, "同名线程规则应合并为一条");
        assert_eq!(rules[0].cpus, "0-3,7");
        assert_eq!(rules[0].sched.as_deref(), Some("fifo:60"));
    }
}

#[cfg(test)]
mod h2_tests {
    use super::*;

    #[test]
    fn sched_only_rule_is_whitelist_placeholder() {
        // H2 回归测试：sched-only 规则（无 cpus）应作为白名单占位合法编译
        let toml = r#"
            [[rule]]
            pkg = "com.schedonly"
            sched = "fifo:60"
        "#;
        let raw: RawConfig = toml::from_str(toml).expect("parse");
        let model = ConfigModel::from_toml(&raw);
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
    fn toml_profile_expands_template() {
        // P6.0：profile "game" 展开为内置模板 + 用户覆盖
        let toml = r#"
            [app."com.example.game"]
            profile = "game"

            [app."com.example.game".threads.RenderThread]
            cpus = "7"
        "#;
        let raw: RawConfig = toml::from_str(toml).expect("parse");
        let model = ConfigModel::from_toml(&raw);
        let rules = model.to_rules();
        // RenderThread 用户覆盖 cpus="7" → 1 条线程规则
        let rt = rules.iter().find(|r| r.thread == "RenderThread").expect("RenderThread 规则");
        assert_eq!(rt.cpus, "7", "用户 cpus 应覆盖模板");
        assert_eq!(rt.pkg, "com.example.game");
    }

    #[test]
    fn profile_override_beats_template() {
        // 用户显式 default 覆盖 profile 模板的 default
        let toml = r#"
            [app."com.x"]
            profile = "audio"
            cpus = "0-3"
        "#;
        let raw: RawConfig = toml::from_str(toml).expect("parse");
        let model = ConfigModel::from_toml(&raw);
        let rules = model.to_rules();
        let def = rules.iter().find(|r| r.thread.is_empty()).expect("default 规则");
        assert_eq!(def.cpus, "0-3", "用户 cpus 应覆盖 audio 模板的 little 集群");
    }

    #[test]
    fn unknown_profile_warns_and_falls_back() {
        let toml = r#"
            [app."com.x"]
            profile = "nonexistent"
        "#;
        let raw: RawConfig = toml::from_str(toml).expect("parse");
        let model = ConfigModel::from_toml(&raw);
        let rules = model.to_rules();
        // 未知 profile → 空模板 → 无策略 → 占位规则
        assert_eq!(rules.len(), 1);
        assert!(rules[0].cpus.is_empty());
    }
}
