//! Rule set: compiled index + matching. Pure functions, no I/O, unit-testable.
//!
//! # Match priority (MatchPriority, ChatGPT P6.1 constraint)
//!
//! ```text
//! exact package (highest)
//!   > wildcard package (multiple hits → highest specificity wins)
//!   > global default (no rules → None)
//! ```
//!
//! groups/profiles do not participate in matching: they are expanded in the
//! **Config Compiler phase** (P6.2/6.3) into plain rules; RuleSet only knows
//! pkg/thread matching, never high-level semantics (GPT architecture review #4).
//!
//! # Inheritance semantics (GPT 3rd review: exact overrides wildcard, not exclusive)
//!
//! Package-level sources **coexist rather than being mutually exclusive**:
//! `resolve()` collects both exact and wildcard rules and merges them at the
//! field level (CSS-like: `.global{color:red}` + `.button{font:bold}` → both
//! apply). Higher-priority sources override lower-priority fields; lower-priority
//! sources fill gaps the higher ones left unset (inheritance). When thread rules
//! hit, they override the package-level rules' same fields, and package rules
//! still fill the gaps.
//!
//! # Specificity (GPT review: nginx-location-style scoring)
//!
//! ```text
//! score = fixed prefix length × 100 + literal char count − wildcard count × 10
//! ```
//!
//! `com.tencent.*` (1202) > `com.*` (394), and `com.*.service` (401) > `com.*` (394);
//! wildcard position affects priority. User-facing docs only expose the
//! "longest fixed prefix wins" semantic; the scoring formula is an internal detail.
//!
//! # Performance
//!
//! `collect_pkg_matches` has an instance-scoped cache (`Mutex<HashMap<pkg, Vec<RuleMatch>>>`):
//! first resolve scans exact/wildcard, subsequent lookups are O(1).
//! Hot-reload creates a **new RuleSet instance**, so the cache naturally
//! invalidates (instance-scoped, never global).

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::Mutex;

use crate::config::{RuleConfig, SchedSpec};
use crate::policy::Policy;
use crate::topology::{parse_cpu_ranges, CpuTopology, MAX_PKG_LEN, MAX_THREAD_LEN};

/// Rule source. Priority is **explicit** via `RuleSource::priority()` —
/// never derive Ord: enum declaration order is fragile (inserting a variant
/// mid-list would silently change priorities; GPT open-source review P0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleSource {
    /// Global default (P6.2 reserved: Config Compiler expands Global rules)
    Global,
    /// profile expansion (P6.2 reserved)
    Profile,
    /// group expansion (P6.2 reserved)
    Group,
    /// Wildcard package name (highest-specificity group)
    PackageWildcard,
    /// Exact package name
    PackageExact,
    /// Thread-type rules (P6.2 reserved: thread-type expands to fnmatch patterns,
    /// variant kept for a future direct ThreadMatcher expression)
    ThreadType,
    /// Exact thread-name rules (reserved, same rationale)
    ThreadExact,
}

impl RuleSource {
    /// Explicit priority — higher wins. Declared as a match table so that
    /// inserting a new source never silently changes existing priorities.
    pub fn priority(self) -> u8 {
        match self {
            Self::Global => 10,
            Self::Profile => 20,
            Self::Group => 30,
            Self::PackageWildcard => 40,
            Self::PackageExact => 50,
            Self::ThreadType => 60,
            Self::ThreadExact => 70,
        }
    }
}

/// 一条命中的规则（index + 来源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuleMatch {
    pub index: usize,
    pub source: RuleSource,
}

/// Compiled rule (pub(crate): merge.rs reads policy fields for merging).
#[derive(Clone)]
pub(crate) struct CompiledRule {
    pub(crate) pkg: String,
    pub(crate) thread: String,
    pub(crate) thread_pattern: Option<CString>,
    pub(crate) policy: Policy,
}

/// 通配符包名规则组。
#[derive(Clone)]
struct WildcardRule {
    pattern: String,
    pattern_cstr: CString,
    /// nginx 风格评分：前缀长×100 + 字面字符数 − 通配符数×10
    specificity: usize,
    rule_idxs: Vec<usize>,
}

/// 已编译、可查询的规则集。
pub struct RuleSet {
    rules: Vec<CompiledRule>,
    /// 精确包名 → 规则索引
    exact: HashMap<String, Vec<usize>>,
    /// 通配符包名 → 规则组（多条命中取 specificity 最大者）
    wildcards: Vec<WildcardRule>,
    pkgs: Vec<String>,
    /// pkg → 命中规则缓存（实例级，reload 重建即失效）
    cache: Mutex<HashMap<String, Vec<RuleMatch>>>,
}

impl Clone for RuleSet {
    fn clone(&self) -> Self {
        Self {
            rules: self.rules.clone(),
            exact: self.exact.clone(),
            wildcards: self.wildcards.clone(),
            pkgs: self.pkgs.clone(),
            cache: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for RuleSet {
    fn default() -> Self {
        Self {
            rules: Vec::new(),
            exact: HashMap::new(),
            wildcards: Vec::new(),
            pkgs: Vec::new(),
            cache: Mutex::new(HashMap::new()),
        }
    }
}

/// 配置编译错误（带上下文）。
#[derive(Debug)]
pub struct RuleError {
    pub index: usize,
    pub reason: String,
}

impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rule #{}: {}", self.index + 1, self.reason)
    }
}

pub struct CompileResult {
    pub rules: RuleSet,
    pub errors: Vec<RuleError>,
}

impl RuleSet {
    /// 配置中出现的包名列表（P7.1 eBPF 白名单键生成用）。
    pub fn pkgs(&self) -> &[String] {
        &self.pkgs
    }

    /// 编译配置中的规则列表；无效规则计入 errors 并跳过。
    pub fn compile(configs: &[RuleConfig], topo: &CpuTopology) -> CompileResult {
        let mut rules: Vec<CompiledRule> = Vec::new();
        let mut errors: Vec<RuleError> = Vec::new();

        for (i, rc) in configs.iter().enumerate() {
            if rc.pkg.is_empty() || rc.pkg.len() >= MAX_PKG_LEN {
                errors.push(RuleError { index: i, reason: "pkg 为空或超过长度限制".into() });
                continue;
            }
            if rc.thread.len() >= MAX_THREAD_LEN {
                errors.push(RuleError { index: i, reason: "thread 超过长度限制".into() });
                continue;
            }
            // Claude review 1.2: exact thread names >15 bytes get truncated by the kernel
            // comm (TASK_COMM_LEN=16 incl. NUL); fnmatch would never match — warn at
            // compile time and show the actual truncated comm value.
            if rc.thread.len() > 15 && !contains_wildcard(&rc.thread) {
                let truncated: String = rc.thread.chars().take(15).collect();
                eprintln!(
                    "warning: rule #{} thread \"{}\" exceeds 15 bytes; kernel comm will truncate to \"{}\" — exact match will never hit, use the truncated value or a wildcard",
                    i + 1, rc.thread, truncated
                );
            }

            let cpus = parse_cpu_ranges(&rc.cpus, Some(&topo.present_cpus));
            if cpus.count() == 0 && !rc.cpus.is_empty() {
                errors.push(RuleError { index: i, reason: format!("cpus \"{}\" parses empty or invalid", rc.cpus) });
                continue;
            }
            // H2 fix: cpus="" is a whitelist placeholder rule (package enters the
            // whitelist but CPU is unconstrained; resolve only applies sched/nice)

            // cpuset dirs are created at runtime (after hot-reload/topology ready);
            // at compile time we keep the target range string, dir name ↔ range 1:1.
            let dir_name = cpus.to_range_string();

            let (sched, sched_prio) = match rc.sched_spec() {
                Some(SchedSpec { policy, prio }) => (Some(policy), prio),
                None => (None, None),
            };

            rules.push(CompiledRule {
                pkg: rc.pkg.clone(),
                thread: rc.thread.clone(),
                thread_pattern: CString::new(rc.thread.as_str()).ok(),
                policy: Policy {
                    cpus,
                    cpuset_dir: dir_name,
                    sched,
                    sched_prio,
                    nice: rc.nice,
                    // NEW-H2: uclamp 全链路传递（此前在此静默丢失）
                    uclamp_min: rc.uclamp_min,
                    uclamp_max: rc.uclamp_max,
                },
            });
        }

        let mut exact: HashMap<String, Vec<usize>> = HashMap::new();
        let mut wildcards: Vec<WildcardRule> = Vec::new();
        let mut pkgs: Vec<String> = Vec::new();

        for (i, r) in rules.iter().enumerate() {
            if contains_wildcard(&r.pkg) {
                // 同一通配模式的多条规则归组（线程规则 + 包规则）
                match wildcards.iter_mut().find(|w| w.pattern == r.pkg) {
                    Some(w) => w.rule_idxs.push(i),
                    None => {
                        if let Ok(pattern_cstr) = CString::new(r.pkg.as_str()) {
                            wildcards.push(WildcardRule {
                                pattern: r.pkg.clone(),
                                pattern_cstr,
                                specificity: pattern_specificity(&r.pkg),
                                rule_idxs: vec![i],
                            });
                        }
                    }
                }
            } else {
                exact.entry(r.pkg.clone()).or_default().push(i);
            }
            if !pkgs.contains(&r.pkg) {
                pkgs.push(r.pkg.clone());
            }
        }

        CompileResult {
            rules: RuleSet { rules, exact, wildcards, pkgs, cache: Mutex::new(HashMap::new()) },
            errors,
        }
    }

    /// pkg → 命中规则（**exact 与 wildcard 并存**，GPT 第三次审查：
    /// 来源不互斥，低优先级来源参与字段填充）。
    /// 带实例级缓存：首次扫描，后续 O(1)。
    fn collect_pkg_matches(&self, pkg: &str) -> Vec<RuleMatch> {
        // ① 缓存命中
        if let Some(cached) = self.cache.lock().unwrap_or_else(|e| e.into_inner()).get(pkg) {
            return cached.clone();
        }

        let mut result: Vec<RuleMatch> = Vec::new();

        // ② exact（高优先级，先入）
        if let Some(idxs) = self.exact.get(pkg) {
            result.extend(
                idxs.iter().map(|&i| RuleMatch { index: i, source: RuleSource::PackageExact }),
            );
        }

        // ③ wildcard：多条命中取 specificity 最大者（保留一组）
        let mut best_spec = 0usize;
        let mut best: Option<&[usize]> = None;
        for w in &self.wildcards {
            if w.specificity > best_spec && fnmatch_pkg(&w.pattern_cstr, pkg) {
                best_spec = w.specificity;
                best = Some(&w.rule_idxs);
            }
        }
        if let Some(idxs) = best {
            result.extend(
                idxs.iter().map(|&i| RuleMatch { index: i, source: RuleSource::PackageWildcard }),
            );
        }

        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(pkg.to_string(), result.clone());
        result
    }

    /// 包名是否在白名单中。
    pub fn is_interested(&self, pkg: &str) -> bool {
        !self.collect_pkg_matches(pkg).is_empty()
    }

    /// 该包是否存在线程级规则。
    pub fn has_thread_rules(&self, pkg: &str) -> bool {
        self.collect_pkg_matches(pkg)
            .iter()
            .any(|m| !self.rules[m.index].thread.is_empty())
    }

    pub fn pkg_list(&self) -> &[String] {
        &self.pkgs
    }

    /// 是否存在需要 CAP_SYS_NICE 的 RT 调度规则（Q6 检查用）。
    pub fn has_rt_sched(&self) -> bool {
        self.rules.iter().any(|r| {
            matches!(
                r.policy.sched,
                Some(crate::policy::SchedPolicy::Fifo | crate::policy::SchedPolicy::Rr)
            )
        })
    }

    /// pkg 缓存当前条目数（测试观察用）。
    #[cfg(test)]
    pub(crate) fn cache_len(&self) -> usize {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 测试观察用：直接访问来源收集结果（验证 matcher 只找来源不合并）。
    #[cfg(test)]
    pub(crate) fn collect_pkg_matches_for_test(&self, pkg: &str) -> Vec<RuleMatch> {
        self.collect_pkg_matches(pkg)
    }

    /// 测试观察用：访问编译后的规则（merge.rs 单测注入用）。
    #[cfg(test)]
    pub(crate) fn rules_for_test(&self) -> &[CompiledRule] {
        &self.rules
    }

    /// 解析 (pkg, thread) → 策略。
    ///
    /// 分层（GPT 审查）：
    /// 1. **PackageMatcher**：`collect_pkg_matches` — 收集 exact + wildcard（并存）
    /// 2. **ThreadMatcher**：线程规则 fnmatch 命中集（跨来源）；miss 时用包级规则集
    /// 3. **PolicyMerge**（merge.rs）：`merge_rules` — 字段级覆盖合并（CSS 模型）：
    ///    - 高优先级来源字段覆盖低优先级来源（exact 覆盖 wildcard）
    ///    - 低优先级来源填充高优先级未设置的字段（inheritance）
    ///    - 同来源组内 cpus 按位或、sched/nice 首个生效（兼容语义）
    ///    - 线程规则命中时覆盖包级规则同字段，包级规则填充空缺
    pub fn resolve(&self, pkg: &str, thread: &str) -> Option<Policy> {
        let pkg_matches = self.collect_pkg_matches(pkg);
        if pkg_matches.is_empty() {
            return None;
        }

        // 线程规则命中集（跨来源，按 pkg_matches 顺序 = 高优先级在前）
        let thread_hits: Vec<RuleMatch> = pkg_matches
            .iter()
            .copied()
            .filter(|m| !self.rules[m.index].thread.is_empty())
            .filter(|m| {
                self.rules[m.index]
                    .thread_pattern
                    .as_ref()
                    .is_some_and(|pat| fnmatch_c(pat, thread))
            })
            .collect();

        // 包级规则集（线程规则 miss 时用；线程命中时填充空缺字段）
        let pkg_rules: Vec<RuleMatch> = pkg_matches
            .iter()
            .copied()
            .filter(|m| self.rules[m.index].thread.is_empty())
            .collect();

        let mut policy = if !thread_hits.is_empty() {
            let mut pol = crate::merge::merge_rules(&thread_hits, &self.rules)?;
            // 继承：包级规则填充线程规则未设置的字段
            if let Some(fb) = crate::merge::merge_rules(&pkg_rules, &self.rules) {
                crate::merge::fill_missing(&mut pol, &fb);
            }
            pol
        } else {
            crate::merge::merge_rules(&pkg_rules, &self.rules)?
        };

        // H2：cpus 为空但有 sched/nice 时仍返回策略（仅应用调度属性）
        if policy.cpus.count() == 0 && policy.sched.is_none() && policy.nice.is_none() {
            return None;
        }

        // 合并后 cpuset 目录名必须由实际 CPU 范围派生（Claude 审查 ❹）
        policy.cpuset_dir = policy.cpus.to_range_string();

        Some(policy)
    }
}

/// POSIX fnmatch 封装（与 既有实现 相同语义）。
pub fn fnmatch_c(pattern: &CString, string: &str) -> bool {
    const BUF_LEN: usize = MAX_THREAD_LEN;
    if string.len() >= BUF_LEN {
        return false;
    }
    let mut buf = [0u8; BUF_LEN];
    buf[..string.len()].copy_from_slice(string.as_bytes());
    unsafe { libc::fnmatch(pattern.as_ptr(), buf.as_ptr() as *const _, libc::FNM_NOESCAPE) == 0 }
}

/// 包名 fnmatch（缓冲用 MAX_PKG_LEN，线程名版 32 字节会拒绝长包名）。
fn fnmatch_pkg(pattern: &CString, string: &str) -> bool {
    const BUF_LEN: usize = MAX_PKG_LEN;
    if string.len() >= BUF_LEN {
        return false;
    }
    let mut buf = [0u8; BUF_LEN];
    buf[..string.len()].copy_from_slice(string.as_bytes());
    unsafe { libc::fnmatch(pattern.as_ptr(), buf.as_ptr() as *const _, libc::FNM_NOESCAPE) == 0 }
}

/// 判断字符串是否含通配符。
fn contains_wildcard(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

/// 模式特异性评分（nginx location 风格，内部实现）。
///
/// ```text
/// score = 固定前缀长度 × 100 + 固定字符数 − 通配符数量 × 10
/// ```
///
/// - `com.tencent.*`   → 12×100 + 12 − 1×10 = 1202
/// - `com.*.service`   → 4×100 + 11 − 1×10 = 401  （通配符位置影响优先级）
/// - `com.*`           → 4×100 + 4 − 1×10 = 394
/// - `com.tencent.mm*` → 14×100 + 14 − 1×10 = 1404
fn pattern_specificity(s: &str) -> usize {
    let mut prefix_len = s.len();
    let mut literals = 0usize;
    let mut wildcards = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '*' | '?' | '[' | ']' => {
                if prefix_len == s.len() {
                    prefix_len = i;
                }
                wildcards += 1;
            }
            _ => literals += 1,
        }
    }
    prefix_len * 100 + literals - wildcards * 10
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc(pkg: &str, cpus: &str) -> RuleConfig {
        RuleConfig {
            pkg: pkg.into(),
            thread: String::new(),
            cpus: cpus.into(),
            sched: None,
            nice: None,
            uclamp_min: None,
            uclamp_max: None,
        }
    }

    fn rc_thread(pkg: &str, thread: &str, cpus: &str) -> RuleConfig {
        RuleConfig {
            pkg: pkg.into(),
            thread: thread.into(),
            cpus: cpus.into(),
            sched: None,
            nice: None,
            uclamp_min: None,
            uclamp_max: None,
        }
    }

    fn rc_thread_sched(pkg: &str, thread: &str, sched: &str) -> RuleConfig {
        RuleConfig {
            pkg: pkg.into(),
            thread: thread.into(),
            cpus: String::new(),
            sched: Some(sched.into()),
            nice: None,
            uclamp_min: None,
            uclamp_max: None,
        }
    }

    /// 包级 sched-only 规则（白名单占位：无 cpus）。
    fn rc_sched_only(pkg: &str, sched: &str) -> RuleConfig {
        RuleConfig {
            pkg: pkg.into(),
            thread: String::new(),
            cpus: String::new(),
            sched: Some(sched.into()),
            nice: None,
            uclamp_min: None,
            uclamp_max: None,
        }
    }

    fn topo() -> CpuTopology {
        let mut t = CpuTopology::default();
        for i in 0..8 {
            t.present_cpus.set(i);
        }
        t
    }

    #[test]
    fn specificity_ordering() {
        // 前缀权重 + 通配符惩罚
        assert!(pattern_specificity("com.tencent.mm*") > pattern_specificity("com.tencent.*"));
        assert!(pattern_specificity("com.tencent.*") > pattern_specificity("com.*"));
        assert!(pattern_specificity("com.*.service") > pattern_specificity("com.*"),
            "通配符位置影响优先级：com.*.service > com.*");
        assert_eq!(pattern_specificity("com.tencent.*"), 1202);
        assert_eq!(pattern_specificity("com.*"), 394);
        assert_eq!(pattern_specificity("com.tencent.mm*"), 1404);
    }

    #[test]
    fn exact_overrides_wildcard_field() {
        // exact 的 cpus 覆盖 wildcard 的 cpus（同字段覆盖）
        let rules = vec![
            rc("com.tencent.mm", "0-1"),
            rc("com.tencent.*", "4-7"),
        ];
        let rs = RuleSet::compile(&rules, &topo()).rules;
        let p = rs.resolve("com.tencent.mm", "").expect("resolve");
        assert_eq!(p.cpus.to_range_string(), "0-1", "exact cpus 覆盖 wildcard cpus");
    }

    #[test]
    fn specific_wildcard_priority() {
        // 多个 wildcard 命中取 specificity 最大者
        let rules = vec![
            rc("com.*", "4-7"),
            rc("com.tencent.*", "0-1"),
        ];
        let rs = RuleSet::compile(&rules, &topo()).rules;
        let p = rs.resolve("com.tencent.qq", "").expect("resolve");
        assert_eq!(p.cpus.to_range_string(), "0-1", "com.tencent.* 应胜出（1202 > 394）");

        let p2 = rs.resolve("com.other.app", "").expect("resolve");
        assert_eq!(p2.cpus.to_range_string(), "4-7");
    }

    #[test]
    fn wildcard_inheritance_for_pkg_rules() {
        // GPT 第三次审查测试 6：exact 无线程规则时继承 wildcard 的包级规则
        let rules = vec![
            rc("com.tencent.*", "3-6"),
            rc_thread("com.tencent.mm", "RenderThread", "7"),
        ];
        let rs = RuleSet::compile(&rules, &topo()).rules;

        // 微信 RenderThread → 线程规则 7
        let p = rs.resolve("com.tencent.mm", "RenderThread").expect("resolve");
        assert_eq!(p.cpus.to_range_string(), "7", "线程规则覆盖包级规则");

        // 微信其他线程 → 继承 wildcard 包级规则 3-6（exact 未定义）
        let p2 = rs.resolve("com.tencent.mm", "OtherThread").expect("resolve");
        assert_eq!(p2.cpus.to_range_string(), "3-6", "wildcard 包级规则应被继承");
    }

    #[test]
    fn exact_supplements_wildcard() {
        // GPT 第三次审查例子 1：exact 的线程规则补充 wildcard 的包级规则（字段叠加）
        let rules = vec![
            rc("com.tencent.*", "3-6"),
            rc_thread_sched("com.tencent.mm", "RenderThread", "fifo:60"),
        ];
        let rs = RuleSet::compile(&rules, &topo()).rules;

        // 微信 RenderThread → cpus 继承 wildcard (3-6) + sched 来自 exact (fifo:60)
        let p = rs.resolve("com.tencent.mm", "RenderThread").expect("resolve");
        assert_eq!(p.cpus.to_range_string(), "3-6", "cpus 继承 wildcard 包级规则");
        assert_eq!(p.sched, Some(crate::policy::SchedPolicy::Fifo), "sched 来自 exact 线程规则");
        assert_eq!(p.sched_prio, Some(60));
    }

    #[test]
    fn matcher_collects_sources_not_policy() {
        // GPT open-source review: PackageMatcher finds candidate sources;
        // it must NOT decide the final policy (PolicyMerge does).
        // Both exact and wildcard sources must be returned for com.tencent.mm.
        let rules = vec![
            rc("com.tencent.*", "3-6"),
            rc("com.tencent.mm", "0-1"),
        ];
        let rs = RuleSet::compile(&rules, &topo()).rules;
        let matches = rs.collect_pkg_matches_for_test("com.tencent.mm");
        let sources: Vec<RuleSource> = matches.iter().map(|m| m.source).collect();
        assert!(
            sources.contains(&RuleSource::PackageExact)
                && sources.contains(&RuleSource::PackageWildcard),
            "matcher must return both sources, got: {sources:?}"
        );
    }

    #[test]
    fn rule_source_priority_is_explicit() {
        // GPT open-source review P0: priority must not depend on enum declaration order.
        assert!(RuleSource::PackageExact.priority() > RuleSource::PackageWildcard.priority());
        assert!(RuleSource::ThreadExact.priority() > RuleSource::ThreadType.priority());
        assert!(RuleSource::ThreadType.priority() > RuleSource::PackageExact.priority());
        assert!(RuleSource::Global.priority() < RuleSource::Profile.priority());
        assert_eq!(RuleSource::PackageWildcard.priority(), 40);
        assert_eq!(RuleSource::PackageExact.priority(), 50);
    }

    #[test]
    fn uclamp_flows_through_resolve() {
        // NEW-H2 regression (Claude): uclamp must survive config → compile → resolve
        let rules = vec![RuleConfig {
            pkg: "com.x".into(),
            thread: String::new(),
            cpus: "0-1".into(),
            sched: None,
            nice: None,
            uclamp_min: Some(700),
            uclamp_max: Some(1024),
        }];
        let rs = RuleSet::compile(&rules, &topo()).rules;
        let p = rs.resolve("com.x", "").expect("resolve");
        assert_eq!(p.uclamp_min, Some(700), "uclamp_min must not be dropped");
        assert_eq!(p.uclamp_max, Some(1024), "uclamp_max must not be dropped");
    }

    #[test]
    fn uclamp_constraint_merge() {
        // ChatGPT V3: uclamp is constraint merge, not FirstWins.
        // min takes the max across sources (all floor guarantees satisfied),
        // max takes the min across sources (all ceiling limits satisfied).
        let rules = vec![
            RuleConfig {
                pkg: "com.x".into(),
                thread: String::new(),
                cpus: "0-1".into(),
                sched: None,
                nice: None,
                uclamp_min: Some(300),
                uclamp_max: Some(512),
            },
            RuleConfig {
                pkg: "com.x".into(),
                thread: String::new(),
                cpus: "2-3".into(),
                sched: None,
                nice: None,
                uclamp_min: Some(700),
                uclamp_max: Some(1024),
            },
        ];
        let rs = RuleSet::compile(&rules, &topo()).rules;
        let p = rs.resolve("com.x", "").expect("resolve");
        assert_eq!(p.uclamp_min, Some(700), "min must take the max (700 > 300)");
        assert_eq!(p.uclamp_max, Some(512), "max must take the min (512 < 1024)");
        // cpus 仍按 BitOr 合并
        assert_eq!(p.cpus.to_range_string(), "0-3");
    }

    #[test]
    fn cache_hits_after_first_resolve() {
        // 缓存后 resolve 不再扫描 wildcard
        let mut rules = Vec::new();
        for i in 0..1000 {
            rules.push(rc(&format!("com.app{i}.*"), "0-1"));
        }
        let rs = RuleSet::compile(&rules, &topo()).rules;
        assert_eq!(rs.wildcards.len(), 1000);

        let pkg = "com.app500.some.thing";
        assert!(rs.is_interested(pkg));
        assert_eq!(rs.cache_len(), 1, "首次 resolve 后缓存 1 条");

        for _ in 0..10_000 {
            assert!(rs.resolve(pkg, "").is_some(), "缓存命中路径必须稳定");
        }
        assert_eq!(rs.cache_len(), 1, "重复 resolve 不应增长缓存");
    }

    #[test]
    fn cache_is_instance_scoped() {
        // reload（新实例）后旧缓存不可用、匹配仍正确
        let rules_v1 = vec![rc("com.tencent.mm", "0-1")];
        let rs1 = RuleSet::compile(&rules_v1, &topo()).rules;
        assert!(rs1.resolve("com.tencent.mm", "").is_some());
        assert_eq!(rs1.cache_len(), 1);

        let rules_v2 = vec![rc("com.tencent.*", "4-7")];
        let rs2 = RuleSet::compile(&rules_v2, &topo()).rules;
        assert!(rs2.resolve("com.tencent.mm", "").is_some(), "新实例应命中通配");
        assert_eq!(rs2.resolve("com.tencent.mm", "").unwrap().cpus.to_range_string(), "4-7");
        assert!(rs1.resolve("com.tencent.mm", "").is_some(), "旧实例语义不变");
    }

    #[test]
    fn wildcard_thread_rules_resolve() {
        // 通配 pkg + 线程规则
        let rules = vec![
            rc("com.tencent.*", "0-3"),
            rc_thread("com.tencent.*", "RenderThread", "7"),
        ];
        let rs = RuleSet::compile(&rules, &topo()).rules;
        assert!(rs.has_thread_rules("com.tencent.qq"));

        let p = rs.resolve("com.tencent.qq", "RenderThread").expect("resolve");
        assert_eq!(p.cpus.to_range_string(), "7", "线程规则优先");
    }

    #[test]
    fn empty_policy_returns_none() {
        // ChatGPT P6.2 审查 Q3：真正空规则（无 cpus/sched/nice）→ None
        let rules = vec![rc("com.empty", "")];
        let rs = RuleSet::compile(&rules, &topo()).rules;
        assert!(rs.resolve("com.empty", "").is_none(), "空规则应返回 None（系统默认）");
    }

    #[test]
    fn partial_policy_returns_some() {
        // ChatGPT P6.2 审查 Q3：部分规则（仅 sched）→ Some（白名单占位语义，
        // 仅应用调度属性，cpus 保持空）
        let rules = vec![rc_sched_only("com.partial", "fifo:60")];
        let rs = RuleSet::compile(&rules, &topo()).rules;
        let p = rs.resolve("com.partial", "").expect("部分规则应返回 Some");
        assert_eq!(p.sched, Some(crate::policy::SchedPolicy::Fifo));
        assert_eq!(p.sched_prio, Some(60));
        assert_eq!(p.cpus.count(), 0, "sched-only 规则不约束 CPU");
    }
}
