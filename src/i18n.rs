//! English and Simplified Chinese interface text. Source values and metadata retain their original form.

use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

#[path = "i18n_zh.rs"]
mod zh;
#[path = "i18n_zh_dynamic.rs"]
mod zh_dynamic;
#[path = "i18n_zh_errors.rs"]
mod zh_errors;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Language {
    #[serde(alias = "Turkish")]
    English,
    SimplifiedChinese,
}

impl Default for Language {
    fn default() -> Self {
        #[cfg(target_os = "macos")]
        {
            static PREFERRED: std::sync::OnceLock<Language> = std::sync::OnceLock::new();
            *PREFERRED.get_or_init(|| mac_preferred_language(None))
        }
        #[cfg(not(target_os = "macos"))]
        Self::English
    }
}

#[cfg(target_os = "macos")]
fn mac_preferred_language(
    preferences: Option<&core_foundation::array::CFArray<core_foundation::string::CFString>>,
) -> Language {
    use core_foundation::{
        array::CFArray, base::TCFType, bundle::CFBundleCopyLocalizationsForPreferences,
        string::CFString,
    };
    let supported = CFArray::from_CFTypes(&[CFString::new("en"), CFString::new("zh-Hans")]);
    // SAFETY: Both arrays retain their strings during the call. A null preference array selects the user's system languages.
    let matched = unsafe {
        CFBundleCopyLocalizationsForPreferences(
            supported.as_concrete_TypeRef(),
            preferences.map_or(std::ptr::null(), TCFType::as_concrete_TypeRef),
        )
    };
    if !matched.is_null() {
        // SAFETY: The Copy function returns an owned string array. CFArray releases it when it goes out of scope.
        let locales: CFArray<CFString> = unsafe { CFArray::wrap_under_create_rule(matched) };
        if locales
            .get(0)
            .is_some_and(|tag| *tag == CFString::new("zh-Hans"))
        {
            return Language::SimplifiedChinese;
        }
    }
    Language::English
}

static CHINESE: AtomicBool = AtomicBool::new(false);

pub fn set_language(language: Language) {
    CHINESE.store(language == Language::SimplifiedChinese, Ordering::Relaxed);
}

pub fn t(text: impl AsRef<str>) -> String {
    let language = if CHINESE.load(Ordering::Relaxed) {
        Language::SimplifiedChinese
    } else {
        Language::English
    };
    translate(language, text)
}

/// Translate using an explicit language independently of window and global state.
pub fn translate(language: Language, text: impl AsRef<str>) -> String {
    let text = text.as_ref();
    match language {
        Language::English => text.to_owned(),
        Language::SimplifiedChinese => zh::static_text(text)
            .map(str::to_owned)
            .or_else(|| zh_errors::error_text(text))
            .or_else(|| zh_dynamic::dynamic_text(text))
            .unwrap_or_else(|| text.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_language_honors_preference_order_and_region() {
        use core_foundation::{array::CFArray, string::CFString};
        for (tags, expected) in [
            (["zh-Hans-CN", "en-US"], Language::SimplifiedChinese),
            (["en-GB", "zh-Hans"], Language::English),
            (["fr-FR", "zh-CN"], Language::SimplifiedChinese),
            (["fr-FR", "de-DE"], Language::English),
        ] {
            let requested = CFArray::from_CFTypes(&tags.map(CFString::new));
            assert_eq!(mac_preferred_language(Some(&requested)), expected);
        }
    }

    #[test]
    fn simplified_chinese_localizes_controls_and_preserves_source_payloads() {
        let language = Language::SimplifiedChinese;
        assert_eq!(translate(language, "Language"), "语言");
        assert_eq!(translate(language, "Memory"), "内存");
        assert_eq!(
            translate(language, "Showing 123 Results"),
            "显示 123 条结果"
        );
        assert!(
            translate(language, "Jump to last loaded row (current result only)")
                .contains("当前已载入结果")
        );
        for payload in [
            "customer_name",
            "/Users/张三/Open.parquet",
            "[1,null,3]",
            "{\"Open\":\"Close\"}",
        ] {
            assert_eq!(translate(language, payload), payload);
        }
        for language in [Language::English, Language::SimplifiedChinese] {
            let saved = serde_json::to_string(&language).unwrap();
            assert_eq!(serde_json::from_str::<Language>(&saved).unwrap(), language);
        }
    }

    #[test]
    fn english_and_unknown_values_are_preserved() {
        assert_eq!(translate(Language::English, "Open File…"), "Open File…");
        for value in [
            "/Users/İpek/Showing 10 Results.parquet",
            "customer_name",
            "WHERE city = 'İstanbul'",
            "[1,null,3]",
            "Showing employee Results",
            "unknown diagnostic",
        ] {
            assert_eq!(translate(Language::SimplifiedChinese, value), value);
        }
    }

    #[test]
    fn file_workflow_and_parameterized_notices_render_in_both_languages() {
        for (english, chinese) in [
            ("Import Text File…", "导入文本文件…"),
            ("Statistical Analysis", "统计分析"),
            ("Maximum bars per chart", "每张图的最大柱数"),
            ("Median", "中位数"),
            ("Duplicate grouping field: city", "重复的分类字段：city"),
            (
                "… ({count} bytes total; Save Value writes the original bytes)",
                "…（共 {count} 字节，「保存值」会写出原始字节）",
            ),
        ] {
            assert_eq!(translate(Language::English, english), english);
            assert_eq!(translate(Language::SimplifiedChinese, english), chinese);
        }
    }
}
