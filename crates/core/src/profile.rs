//! Built-in Profile table (P6.0/P6.2: configure an entire app in one line).
//!
//! Three-tier abstraction:
//!   thread override > profile > default
//!
//! Built-in profiles: game / chat / video / launcher / audio / balanced / power-save.
//! `profile "game"` grants the full policy set; default/thread can override.

use std::collections::HashMap;

use crate::config::PolicyModel;

/// 单个 Profile 的完整策略模板。
#[derive(Debug, Clone, Default)]
pub struct ProfileModel {
    pub default: PolicyModel,
    pub threads: HashMap<String, PolicyModel>,
    pub thread_types: HashMap<String, PolicyModel>,
}

/// 内置 Profile 表。
pub fn builtin_profiles() -> HashMap<&'static str, ProfileModel> {
    let mut map = HashMap::new();

    // ── game：渲染上 prime + uclamp 保护，音频下放，其余 big ──
    map.insert(
        "game",
        ProfileModel {
            default: PolicyModel {
                cluster: Some("big".into()),
                ..Default::default()
            },
            threads: HashMap::new(),
            thread_types: HashMap::from([
                (
                    "render".into(),
                    PolicyModel {
                        cluster: Some("prime".into()),
                        uclamp_min: Some(700),
                        ..Default::default()
                    },
                ),
                (
                    "audio".into(),
                    PolicyModel {
                        cluster: Some("little".into()),
                        ..Default::default()
                    },
                ),
            ]),
        },
    );

    // ── audio：音频线程保 big + rr，其余省电 little ──
    map.insert(
        "audio",
        ProfileModel {
            default: PolicyModel {
                cluster: Some("little".into()),
                ..Default::default()
            },
            threads: HashMap::new(),
            thread_types: HashMap::from([(
                "audio".into(),
                PolicyModel {
                    cluster: Some("big".into()),
                    sched: Some("rr:30".into()),
                    ..Default::default()
                },
            )]),
        },
    );

    // ── launcher：渲染 prime + fifo，主线程 big ──
    map.insert(
        "launcher",
        ProfileModel {
            default: PolicyModel {
                cluster: Some("big".into()),
                ..Default::default()
            },
            threads: HashMap::from([(
                "RenderThread".into(),
                PolicyModel {
                    cluster: Some("prime".into()),
                    sched: Some("fifo:60".into()),
                    ..Default::default()
                },
            )]),
            thread_types: HashMap::new(),
        },
    );

    // ── balanced：全部 big，无特殊线程处理 ──
    map.insert(
        "balanced",
        ProfileModel {
            default: PolicyModel {
                cluster: Some("big".into()),
                ..Default::default()
            },
            threads: HashMap::new(),
            thread_types: HashMap::new(),
        },
    );

    // ── power-save：全部 little（后台常驻）──
    map.insert(
        "power-save",
        ProfileModel {
            default: PolicyModel {
                cluster: Some("little".into()),
                ..Default::default()
            },
            threads: HashMap::new(),
            thread_types: HashMap::new(),
        },
    );

    // ── chat：聊天/IM 场景（微信/QQ）——渲染流畅 + 音频清晰，其余 big ──
    map.insert(
        "chat",
        ProfileModel {
            default: PolicyModel {
                cluster: Some("big".into()),
                ..Default::default()
            },
            threads: HashMap::from([(
                "RenderThread".into(),
                PolicyModel {
                    cluster: Some("prime".into()),
                    ..Default::default()
                },
            )]),
            thread_types: HashMap::from([(
                "audio".into(),
                PolicyModel {
                    cluster: Some("big".into()),
                    sched: Some("rr:30".into()),
                    ..Default::default()
                },
            )]),
        },
    );

    // ── video：视频播放场景——渲染/解码流畅，音频保 big，其余省电 little ──
    map.insert(
        "video",
        ProfileModel {
            default: PolicyModel {
                cluster: Some("little".into()),
                ..Default::default()
            },
            threads: HashMap::from([(
                "RenderThread".into(),
                PolicyModel {
                    cluster: Some("big".into()),
                    ..Default::default()
                },
            )]),
            thread_types: HashMap::from([(
                "audio".into(),
                PolicyModel {
                    cluster: Some("big".into()),
                    ..Default::default()
                },
            )]),
        },
    );

    map
}

/// Profile 名称是否有效。
pub fn is_valid_profile(name: &str) -> bool {
    builtin_profiles().contains_key(name)
}

/// 可用的 profile 名列表（文档/诊断用）。
pub fn profile_names() -> Vec<&'static str> {
    builtin_profiles().into_keys().collect()
}
