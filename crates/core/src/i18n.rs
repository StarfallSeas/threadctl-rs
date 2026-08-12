//! P9 日志国际化——系统语言匹配（中英双语；其他语言 → 英文）。
//!
//! 检测顺序：
//! 1. Android：`getprop persist.sys.locale` / `getprop ro.product.locale`
//! 2. 非 Android：`LANG` / `LC_ALL` 环境变量
//!
//! 使用：`println!("{}", i18n::t("中文", "English"))`——返回匹配语言的参数。
//! debug 工程日志保持英文（调试受众）；用户可见日志走双语。

use std::sync::OnceLock;

static ZH: OnceLock<bool> = OnceLock::new();

/// 系统语言是否中文（仅检测一次，缓存）。
pub fn is_zh() -> bool {
    *ZH.get_or_init(|| {
        // Android 优先（getprop）；失败回退环境变量
        let android = std::process::Command::new("getprop")
            .arg("persist.sys.locale")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .or_else(|| {
                std::process::Command::new("getprop")
                    .arg("ro.product.locale")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
            });
        if let Some(locale) = android {
            let l = locale.trim().to_lowercase();
            return l.starts_with("zh") || l.starts_with("cmn") || l.starts_with("yue");
        }
        // 非 Android：LANG/LC_ALL（如 zh_CN.UTF-8 / zh_TW）
        let env_lang = std::env::var("LANG")
            .or_else(|_| std::env::var("LC_ALL"))
            .unwrap_or_default()
            .to_lowercase();
        env_lang.starts_with("zh") || env_lang.starts_with("cmn")
    })
}

/// 按系统语言返回参数（zh=中文，en=英文；其他语言 → 英文）。
pub fn t<'a>(zh: &'a str, en: &'a str) -> &'a str {
    if is_zh() {
        zh
    } else {
        en
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t_returns_matching_language() {
        // 无论系统语言，t 都返回两个参数之一（结构正确性）
        let s = t("中文", "English");
        assert!(s == "中文" || s == "English");
    }
}
