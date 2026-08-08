//! KDL → ConfigModel parser (P6 config format migration).

use crate::config::{
    AppModel, ConfigModel, DaemonConfig, EngineConfig, EngineMode, PolicyModel,
};
use std::collections::HashMap;

/// 解析 KDL 文档 → (ConfigModel, DaemonConfig, EngineConfig)。
/// M3 修复：支持顶层 daemon {} / engine {} 节点。
pub fn parse_kdl(input: &str) -> Result<(ConfigModel, DaemonConfig, EngineConfig), String> {
    let doc: kdl::KdlDocument = input.parse().map_err(|e| format!("KDL 语法错误: {e}"))?;
    let mut apps: HashMap<String, AppModel> = HashMap::new();
    let mut daemon = DaemonConfig::default();
    let mut engine = EngineConfig::default();

    for node in doc.nodes() {
        match node.name().value() {
            "app" => {
                let pkg = node.entries().first()
                    .and_then(|e| e.value().as_string())
                    .ok_or_else(|| "app 节点需要包名字符串".to_string())?.to_string();
                let entry = apps.entry(pkg.clone()).or_insert_with(|| AppModel {
                    pkg: pkg.clone(),
                    profile: None,
                    default_policy: PolicyModel::default(),
                    threads: HashMap::new(), thread_types: HashMap::new(),
                });
                if let Some(children) = node.children() {
                    for child in children.nodes() {
                        match child.name().value() {
                            "default" => { entry.default_policy = parse(child)?; }
                            "profile" => {
                                // P6.0：profile "game"
                                if let Some(pn) = child.entries().first()
                                    .and_then(|e| e.value().as_string()).map(|s| s.to_string())
                                { entry.profile = Some(pn); }
                            }
                            "thread" => {
                                if let Some(tn) = child.entries().first()
                                    .and_then(|e| e.value().as_string()).map(|s| s.to_string())
                                { entry.threads.insert(tn, parse(child)?); }
                            }
                            "thread-type" => {
                                if let Some(tt) = child.entries().first()
                                    .and_then(|e| e.value().as_string()).map(|s| s.to_string())
                                { entry.thread_types.insert(tt, parse(child)?); }
                            }
                            _ => {}
                        }
                    }
                }
            }
            "daemon" => parse_daemon(node, &mut daemon)?,
            "engine" => parse_engine(node, &mut engine)?,
            _ => {}
        }
    }
    Ok((ConfigModel { apps }, daemon, engine))
}

/// M3：解析 daemon 节点：pid-file / ipc-socket / log-level。
fn parse_daemon(node: &kdl::KdlNode, daemon: &mut DaemonConfig) -> Result<(), String> {
    if let Some(children) = node.children() {
        for child in children.nodes() {
            let val = child.entries().first().and_then(|e| e.value().as_string());
            match child.name().value() {
                "pid-file" => if let Some(v) = val { daemon.pid_file = v.to_string(); },
                "ipc-socket" => if let Some(v) = val { daemon.ipc_socket = v.to_string(); },
                "log-level" => if let Some(v) = val { daemon.log_level = v.to_string(); },
                _ => {}
            }
        }
    }
    Ok(())
}

/// M3：解析 engine 节点：mode / scan-interval / lock-interval / dead-cleanup-interval
/// / migrate-action / pressure-sensitive。
fn parse_engine(node: &kdl::KdlNode, engine: &mut EngineConfig) -> Result<(), String> {
    if let Some(children) = node.children() {
        for child in children.nodes() {
            let name = child.name().value();
            let val_str = child.entries().first().and_then(|e| e.value().as_string());
            let val_int = child.entries().first().and_then(|e| e.value().as_integer());
            match name {
                "mode" => {
                    if let Some(v) = val_str {
                        engine.mode = match v {
                            "auto" => EngineMode::Auto,
                            "ebpf" => EngineMode::Ebpf,
                            "proc" => EngineMode::Proc,
                            "hybrid" => EngineMode::Hybrid,
                            _ => return Err(format!("engine mode 无效: {v}")),
                        };
                    }
                }
                "scan-interval" => if let Some(v) = val_int { engine.scan_interval = v as u64; },
                "lock-interval" => if let Some(v) = val_int { engine.lock_interval = v as u64; },
                "dead-cleanup-interval" => if let Some(v) = val_int { engine.dead_cleanup_interval = v as u64; },
                "migrate-action" => {
                    if let Some(v) = val_str {
                        engine.migrate_action = match v {
                            "observe" => crate::decision::MigrateAction::Observe,
                            "suggest" => crate::decision::MigrateAction::Suggest,
                            "force" => crate::decision::MigrateAction::Force,
                            _ => return Err(format!("migrate-action 无效: {v}")),
                        };
                    }
                }
                "pressure-sensitive" => {
                    // KDL 裸布尔是 #true/#false；也兼容 "true"/"false" 字符串
                    let v = child.entries().first().map(|e| e.value());
                    if let Some(val) = v {
                        let b = if let Some(b) = val.as_bool() {
                            Some(b)
                        } else if let Some(s) = val.as_string() {
                            match s {
                                "true" => Some(true),
                                "false" => Some(false),
                                _ => None,
                            }
                        } else { None };
                        if let Some(b) = b {
                            engine.pressure_sensitive = b;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn parse(node: &kdl::KdlNode) -> Result<PolicyModel, String> {
    let mut pol = PolicyModel::default();
    let mut priority: Option<i32> = None;

    for entry in node.entries() {
        let val = entry.value();
        let key = entry.name().map(|n| n.value().to_string());
        match key.as_deref() {
            Some("cluster") => pol.cluster = val.as_string().map(|s| s.to_string()),
            Some("cpus") => pol.cpus = val.as_string().map(|s| s.to_string()),
            Some("sched") => pol.sched = val.as_string().map(|s| s.to_string()),
            Some("priority") => priority = val.as_integer().map(|v| v as i32),
            Some("nice") => pol.nice = val.as_integer().map(|v| v as i32),
            Some("uclamp-min") => pol.uclamp_min = val.as_integer().map(|v| v as u32),
            Some("uclamp-max") => pol.uclamp_max = val.as_integer().map(|v| v as u32),
            _ => {}
        }
    }
    if let Some(children) = node.children() {
        for child in children.nodes() {
            let name = child.name().value();
            let val = child.entries().first().map(|e| e.value());
            match name {
                "cpus" => pol.cpus = val.and_then(|v| v.as_string()).map(|s| s.to_string()),
                "cluster" => pol.cluster = val.and_then(|v| v.as_string()).map(|s| s.to_string()),
                "sched" => pol.sched = val.and_then(|v| v.as_string()).map(|s| s.to_string()),
                "priority" => priority = val.and_then(|v| v.as_integer()).map(|v| v as i32),
                "nice" => pol.nice = val.and_then(|v| v.as_integer()).map(|v| v as i32),
                "uclamp-min" => pol.uclamp_min = val.and_then(|v| v.as_integer()).map(|v| v as u32),
                "uclamp-max" => pol.uclamp_max = val.and_then(|v| v.as_integer()).map(|v| v as u32),
                _ => {}
            }
        }
    }
    // 合并 sched + priority → "fifo:60"
    if let Some(prio) = priority {
        if pol.sched.as_deref().map_or(true, |s| !s.contains(':')) {
            pol.sched = Some(format!("{}:{}", pol.sched.as_deref().unwrap_or("fifo"), prio));
        }
    }
    Ok(pol)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let kdl = r#"app "com.miui.home" {
            thread "RenderThread" { cluster "prime"; sched "fifo"; priority 60; nice -10 }
            default { cpus "0-6" }
        }"#;
        let (model, _d, _e) = parse_kdl(kdl).expect("parse");
        let mm = model.apps.get("com.miui.home").unwrap();
        assert_eq!(mm.default_policy.cpus.as_deref(), Some("0-6"));
        let rt = mm.threads.get("RenderThread").unwrap();
        assert_eq!(rt.cluster.as_deref(), Some("prime"));
        assert_eq!(rt.sched.as_deref(), Some("fifo:60"));
        assert_eq!(rt.nice, Some(-10));
    }

    #[test]
    fn kdl_to_rules_works() {
        let kdl = r#"app "com.test" { default { cpus "0-3" } }"#;
        let (model, _d, _e) = parse_kdl(kdl).expect("parse");
        let rules = model.to_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].cpus, "0-3");
    }

    #[test]
    fn kdl_daemon_engine_nodes() {
        // M3 回归测试
        let kdl = r#"
            daemon { pid-file "/data/run/x.pid"; ipc-socket "/data/run/x.sock" }
            engine { mode "hybrid"; scan-interval 3; lock-interval 30; migrate-action "observe" }
            app "com.test" { default { cpus "0-3" } }
        "#;
        let (model, daemon, engine) = parse_kdl(kdl).expect("parse");
        assert_eq!(daemon.pid_file, "/data/run/x.pid");
        assert_eq!(daemon.ipc_socket, "/data/run/x.sock");
        assert_eq!(engine.mode, EngineMode::Hybrid);
        assert_eq!(engine.scan_interval, 3);
        assert_eq!(engine.lock_interval, 30);
        assert_eq!(engine.migrate_action, crate::decision::MigrateAction::Observe);
        assert!(model.apps.contains_key("com.test"));
    }
}
