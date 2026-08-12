//! P12 — 场景一键套用（`threadctl apply-scene <name>`）。
//!
//! 设计：场景 = 引擎参数预设（不碰 app 规则——避免与用户配置冲突）。
//! 写回配置（KDL 标记段）→ 触发 reload 热生效；重复应用先移除旧场景段。
//!
//! 场景表（可扩展）：
//! - game：对抗最强（60s 起步自适应 + force 迁移）
//! - video：30s 周期（视频渲染线程需要较快对抗）
//! - power-save：300s 长周期（省电优先）
//! - balanced / default：60s 标准

/// 场景段标记（配置文件中可被 apply-scene 识别/移除）。
pub const SCENE_MARKER_START: &str = "// <<scene:";
pub const SCENE_MARKER_END: &str = "// <<scene: end>>";

/// 内置场景表：场景名 → 引擎参数 KDL 片段。
pub fn scene_kdl(name: &str) -> Option<String> {
    let inner = match name {
        "game" => Some("lock-interval 60\n    migrate-action \"force\""),
        "video" => Some("lock-interval 30\n    migrate-action \"observe\""),
        "power-save" => Some("lock-interval 300\n    pressure-sensitive \"true\""),
        "balanced" | "default" => Some("lock-interval 60\n    migrate-action \"observe\""),
        _ => None,
    }?;
    Some(format!(
        "{SCENE_MARKER_START} {name}>>\nengine {{\n    {inner}\n}}\n{SCENE_MARKER_END}\n"
    ))
}

/// 场景名是否有效。
pub fn is_valid_scene(name: &str) -> bool {
    scene_kdl(name).is_some()
}

/// 应用场景：读配置 → 移除旧场景段 → 追加新场景段 → 写回。
/// 返回新配置内容（调用方负责触发 reload）。
pub fn apply_scene_to_config(content: &str, name: &str) -> Result<String, String> {
    let block = scene_kdl(name).ok_or_else(|| format!("未知场景: {name}（可选: game/video/power-save/balanced/default）"))?;
    // 移除旧场景段（从标记到 end 标记）
    let mut cleaned = String::new();
    let mut in_scene = false;
    for line in content.lines() {
        if line.trim_start().starts_with(SCENE_MARKER_START) {
            in_scene = true;
            continue;
        }
        if line.trim_start().starts_with(SCENE_MARKER_END) {
            in_scene = false;
            continue;
        }
        if !in_scene {
            cleaned.push_str(line);
            cleaned.push('\n');
        }
    }
    cleaned.push_str(&block);
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_kdl_generates_valid_block() {
        let b = scene_kdl("game").expect("game 场景");
        assert!(b.contains("lock-interval 60"));
        assert!(b.contains("<<scene: game>>"));
        assert!(b.contains("<<scene: end>>"));
        assert!(scene_kdl("nonexistent").is_none());
        assert!(!is_valid_scene("foo"));
        assert!(is_valid_scene("game"));
    }

    #[test]
    fn apply_scene_appends_and_replaces() {
        let base = "engine { mode \"auto\" }\napp \"com.x\" { cpus \"0-3\" }\n";
        let v1 = apply_scene_to_config(base, "game").expect("apply");
        assert!(v1.contains("<<scene: game>>"), "追加场景段");
        assert!(v1.contains("app \"com.x\""), "原配置保留");
        // 第二次应用不同场景 → 旧段移除
        let v2 = apply_scene_to_config(&v1, "power-save").expect("apply");
        assert!(!v2.contains("<<scene: game>>"), "旧场景段移除");
        assert!(v2.contains("<<scene: power-save>>"), "新场景段");
        assert!(v2.contains("lock-interval 300"));
        assert!(v2.contains("app \"com.x\""), "原配置仍保留");
    }

    #[test]
    fn apply_unknown_scene_errors() {
        let base = "engine { }\n";
        assert!(apply_scene_to_config(base, "nope").is_err());
    }
}
