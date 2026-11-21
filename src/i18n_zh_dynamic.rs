//! Localize generated UI messages while preserving source names, paths, types, and cell values.

fn number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

fn decimal_number(text: &str) -> bool {
    if let Some((whole, fraction)) = text.split_once('.') {
        number(whole) && number(fraction)
    } else {
        number(text)
    }
}

fn nested_summary(text: &str) -> Option<String> {
    let (kind, dimensions) = text.split_once(" · ")?;
    let kind = match kind {
        "STRUCT" => "STRUCT",
        "JSON object" => "JSON object",
        "LIST of records" => "LIST of records",
        "JSON array of objects" => "JSON array of objects",
        "LIST / ARRAY" => "LIST / ARRAY",
        "JSON array (mixed values retained)" => "JSON array（保留混合类型值）",
        "MAP" => "MAP",
        "NULL value" => "NULL 值",
        "Empty object {}" => "Empty object {}",
        "Scalar value" => "Scalar 值",
        _ => return None,
    };
    let (rows, columns) = dimensions.split_once(" rows × ")?;
    let columns = columns.strip_suffix(" columns")?;
    if number(rows) && number(columns) {
        Some(format!("{kind} · {rows} 行 × {columns} 列"))
    } else {
        None
    }
}

fn audio_position(text: &str) -> Option<String> {
    let (position, duration) = text.strip_suffix(" s")?.split_once(" / ")?;
    if decimal_number(position) && decimal_number(duration) {
        Some(format!("{position} / {duration} 秒"))
    } else {
        None
    }
}

fn audio_description(text: &str) -> Option<String> {
    let parts = text.split(" · ").collect::<Vec<_>>();
    if !(3..=4).contains(&parts.len()) {
        return None;
    }
    let (codec, identifier) = parts[0].split_once(" (")?;
    let identifier = identifier.strip_suffix(')')?;
    let codec = match codec {
        "PCM" => "PCM",
        "MP3" => "MP3",
        "AAC" => "AAC",
        "Apple Lossless" => "Apple Lossless",
        "Audio" => "音频",
        _ => return None,
    };
    let quoted_code = identifier
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
        .is_some_and(|value| {
            value.len() == 4 && value.bytes().all(|byte| (32..=126).contains(&byte))
        });
    let hex_code = identifier.strip_prefix("0x").is_some_and(|value| {
        value.len() == 8 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    if !quoted_code && !hex_code {
        return None;
    }
    let rate = if parts[1] == "unknown sample rate" {
        "采样率未知"
    } else if parts[1].strip_suffix(" Hz").is_some_and(decimal_number) {
        parts[1]
    } else {
        return None;
    };
    let channels = match parts[2] {
        "unknown channels" => "声道数未知".into(),
        "mono (1 channel)" => "单声道（1 声道）".into(),
        "stereo (2 channels)" => "立体声（2 声道）".into(),
        value => {
            let channels = value
                .strip_suffix(" channels")
                .filter(|value| number(value))?;
            format!("{channels} 声道")
        }
    };
    let mut result = format!("{codec} ({identifier}) · {rate} · {channels}");
    if let Some(bits) = parts.get(3) {
        let (bits, representation) = bits.split_once("-bit")?;
        if !number(bits) {
            return None;
        }
        let representation = match representation {
            "" => "",
            " floating point" => "浮点数",
            " signed integer" => "有符号整数",
            " unsigned integer" => "无符号整数",
            _ => return None,
        };
        result.push_str(&format!(" · {bits} 位{representation}"));
    }
    Some(result)
}

fn split_unquoted<'a>(text: &'a str, separator: &str) -> Option<(&'a str, &'a str)> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
        } else if ch == '"' {
            quoted = true;
        } else if text[index..].starts_with(separator) {
            return Some((&text[..index], &text[index + separator.len()..]));
        }
    }
    None
}

/// Translate generated token separators and the empty-field label. Quoted tokens may contain English phrases or escapes.
fn missing_tokens(text: &str) -> String {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut skip_until = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if index < skip_until {
            continue;
        }
        if quoted {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
        } else if ch == '"' {
            quoted = true;
        } else if text[index..].starts_with(" or ") {
            parts.push(&text[start..index]);
            start = index + 4;
            skip_until = start;
        }
    }
    parts.push(&text[start..]);
    parts
        .into_iter()
        .map(|part| {
            if part == "empty field" {
                "空字段"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" 或 ")
}

pub(super) fn dynamic_text(text: &str) -> Option<String> {
    if let Some(row) = text
        .strip_prefix("Original row number ")
        .filter(|n| number(n))
    {
        return Some(format!("原始行号 {row}"));
    }
    for (prefix, translated) in [
        ("Duplicate grouping field: ", "重复的分类字段"),
        (
            "Grouping field is no longer available: ",
            "分类字段已不可用",
        ),
        ("Grouping field is not loaded: ", "分类字段尚未载入"),
        ("Aggregation field is not loaded: ", "汇总字段尚未载入"),
        ("Unsupported aggregation for ", "该字段不支持此汇总方式"),
    ] {
        if let Some(field) = text.strip_prefix(prefix) {
            return Some(format!("{translated}：{field}"));
        }
    }
    if let Some((operation, kind)) = text.split_once(" is not supported for this Data type: ") {
        return Some(format!("{operation} 不支持此 Data type：{kind}"));
    }
    if let Some(summary) = text.strip_suffix(" · Open table") {
        if let Some((kind, value)) = summary.split_once(" · ") {
            if let Some((count, unit)) = value.split_once(' ') {
                if number(count) {
                    let kind = match kind {
                        "Map" => Some("Map"),
                        "Union" => Some("Union"),
                        _ => None,
                    };
                    let unit = match unit {
                        "item" => Some("个元素"),
                        "entries" => Some("个条目"),
                        _ => None,
                    };
                    if let (Some(kind), Some(unit)) = (kind, unit) {
                        return Some(format!("{kind} · {count} {unit} · 打开表格"));
                    }
                }
            }
        }
    }
    if let Some(summary) = nested_summary(text)
        .or_else(|| audio_position(text))
        .or_else(|| audio_description(text))
    {
        return Some(summary);
    }
    if let Some((data_type, suffix)) = text.split_once(" supports at most ")
        && matches!(data_type, "TIME" | "TIMETZ" | "TIMESTAMPTZ")
        && let Some(digits) = suffix
            .strip_suffix(" fractional digits.")
            .filter(|digits| number(digits))
    {
        return Some(format!("{data_type} 最多支持 {digits} 位小数秒。"));
    }
    if let Some(value) = text
        .strip_prefix("Apply ")
        .and_then(|value| value.strip_suffix(" Fields"))
        .filter(|value| number(value))
    {
        return Some(format!("应用 {value} 个字段"));
    }
    if let Some(value) = text
        .strip_prefix("Showing ")
        .and_then(|value| value.strip_suffix(" Results"))
        .filter(|value| number(value))
    {
        return Some(format!("显示 {value} 条结果"));
    }
    if let Some(value) = text.strip_prefix("Loaded ") {
        if let Some((start, rest)) = value.split_once(" to ") {
            if let Some((end, total)) = rest.split_once("   Out of ") {
                if number(start) && number(end) {
                    if number(total) {
                        return Some(format!("已载入 {start}–{end}   共 {total} 行"));
                    }
                    if total == "unknown (lazy text source)" {
                        return Some(format!("已载入 {start}–{end}   总行数未知（按需读取）"));
                    }
                }
            }
        }
    }
    if let Some(value) = text
        .strip_prefix("Export ")
        .and_then(|value| value.strip_suffix(" summary rows."))
        .filter(|value| number(value))
    {
        return Some(format!("导出 {value} 行汇总结果。"));
    }
    if let Some(value) = text
        .strip_prefix("Export ")
        .and_then(|value| {
            value.strip_suffix(
                " visible rows, including the current field selection, filter and sort.",
            )
        })
        .filter(|value| number(value))
    {
        return Some(format!(
            "导出当前显示的 {value} 行，保留当前字段选择、筛选和排序。"
        ));
    }
    if let Some(value) = text.strip_prefix("Always load all records: ") {
        return match value {
            "true" => Some("始终载入全部记录：开启".into()),
            "false" => Some("始终载入全部记录：关闭".into()),
            _ => None,
        };
    }
    if let Some(value) = text.strip_prefix("Row ") {
        if let Some((row, rest)) = value.split_once(" · ") {
            if number(row) {
                return Some(format!("第 {row} 行 · {rest}"));
            }
        }
    }
    if let Some(path) = text.strip_prefix("Saved ") {
        return Some(format!("已保存：{path}"));
    }
    if let Some(value) = text.strip_suffix(" ms").filter(|value| number(value)) {
        return Some(format!("{value} 毫秒"));
    }
    if let Some(value) = text.strip_prefix("Field ").filter(|value| number(value)) {
        return Some(format!("字段 {value}"));
    }
    if let Some(value) = text.strip_suffix(" NULLs").filter(|value| number(value)) {
        return Some(format!("{value} 个 NULL"));
    }
    if let Some(value) = text
        .strip_suffix(
            " fields — inferred from the sample; select a field to override its name/type",
        )
        .filter(|value| number(value))
    {
        return Some(format!(
            "共 {value} 个字段，由样本推断；选择字段可修改名称和类型"
        ));
    }
    if let Some(value) = text
        .strip_prefix("unknown (sample estimate: ")
        .and_then(|value| value.strip_suffix(')'))
        .filter(|value| number(value))
    {
        return Some(format!("未知（样本估计：{value}）"));
    }
    if let Some((prefix, sample)) = text.rsplit_once(" · ") {
        for (suffix, description) in [
            (" records sampled (first 1000 only)", "仅前 1000 条"),
            (" records sampled (end of file reached)", "已到文件末尾"),
        ] {
            if let Some(count) = sample.strip_suffix(suffix).filter(|count| number(count)) {
                return Some(format!("{prefix} · 已采样 {count} 条记录（{description}）"));
            }
        }
    }
    if let Some(value) = text.strip_prefix("Selected field NULL rule: ") {
        let (value, case) = if let Some(value) = value.strip_suffix("; ignore case") {
            (value, "忽略大小写")
        } else if let Some(value) = value.strip_suffix("; case sensitive") {
            (value, "区分大小写")
        } else {
            return None;
        };
        let (tokens, whitespace) = if let Some(tokens) = value.strip_suffix("; trim whitespace") {
            (tokens, "匹配时去除首尾空白")
        } else if let Some(tokens) = value.strip_suffix("; exact whitespace") {
            (tokens, "精确匹配空白")
        } else {
            return None;
        };
        return Some(format!(
            "所选字段 NULL 规则：{}；{whitespace}；{case}",
            missing_tokens(tokens)
        ));
    }
    if let Some((format, bytes)) = text.split_once(" image · ") {
        if let Some(bytes) = bytes.strip_suffix(" bytes").filter(|value| number(value)) {
            if matches!(
                format,
                "PNG" | "JPG" | "JPEG" | "GIF" | "WEBP" | "BMP" | "TIFF" | "TIF" | "ICO" | "AVIF"
            ) {
                return Some(format!("{format} 图片 · {bytes} 字节"));
            }
        }
    }
    if let Some(value) = text.strip_prefix("Duplicate header ") {
        if let Some(value) = value.strip_suffix('.') {
            if let Some((original, renamed)) = split_unquoted(value, " was renamed to ") {
                return Some(format!("重复表头 {original} 已重命名为 {renamed}。"));
            }
        }
    }
    if let Some(value) = text.strip_prefix("Column ") {
        if let Some((column, detail)) = split_unquoted(value, ": ") {
            if detail
                == "common missing tokens were inferred from otherwise typed sample values; review its missing-value rule."
            {
                return Some(format!(
                    "字段 {column}：根据样本中的其他类型值推断了常见缺失标记，请检查该字段的缺失值规则。"
                ));
            }
            if let Some((count, data_type)) = detail.split_once(" sampled value(s) do not match ") {
                if let Some(data_type) = data_type.strip_suffix(
                    "; choose VARCHAR or correct the type/missing rule before loading.",
                ) {
                    if number(count) {
                        return Some(format!(
                            "字段 {column}：{count} 个样本值不符合 {data_type}；请在载入前选择 VARCHAR，或修正类型和缺失值规则。"
                        ));
                    }
                }
            }
        }
    }
    if let Some(value) = text.strip_prefix("Logical record ") {
        if let Some((record, rest)) = value.split_once(" (line ") {
            if let Some((line, detail)) = rest.split_once("): ") {
                if number(record) && number(line) {
                    if let Some((found, expected)) = detail
                        .strip_prefix("found ")
                        .and_then(|detail| detail.split_once(" fields, expected "))
                    {
                        if let Some(expected) = expected
                            .strip_suffix('.')
                            .filter(|expected| number(expected))
                        {
                            if number(found) {
                                return Some(format!(
                                    "逻辑记录 {record}（第 {line} 行）：发现 {found} 个字段，应为 {expected} 个。"
                                ));
                            }
                        }
                    }
                    let detail = match detail {
                        "Unclosed quoted field at end of file." => "文件末尾的引号字段未闭合。",
                        "Unexpected text after a closing quote." => "闭合引号后存在多余文本。",
                        "Unexpected quote inside an unquoted field." => {
                            "未加引号的字段中出现了多余引号。"
                        }
                        _ => return None,
                    };
                    return Some(format!("逻辑记录 {record}（第 {line} 行）：{detail}"));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_and_audio_summaries_require_generated_kinds_and_preserve_native_codes() {
        assert_eq!(
            dynamic_text("LIST of records · 12553 rows × 3 columns").as_deref(),
            Some("LIST of records · 12553 行 × 3 列")
        );
        assert_eq!(
            dynamic_text("NULL value · 1 rows × 1 columns").as_deref(),
            Some("NULL 值 · 1 行 × 1 列")
        );
        assert_eq!(dynamic_text("1.1 / 5.0 s").as_deref(), Some("1.1 / 5.0 秒"));
        assert_eq!(
            dynamic_text("PCM ('lpcm') · 8000 Hz · mono (1 channel) · 16-bit signed integer")
                .as_deref(),
            Some("PCM ('lpcm') · 8000 Hz · 单声道（1 声道） · 16 位有符号整数")
        );
        assert_eq!(
            dynamic_text("MP3 ('.mp3') · 44100 Hz · stereo (2 channels)").as_deref(),
            Some("MP3 ('.mp3') · 44100 Hz · 立体声（2 声道）")
        );
        assert_eq!(
            dynamic_text("Apple Lossless ('alac') · 44100 Hz · stereo (2 channels)").as_deref(),
            Some("Apple Lossless ('alac') · 44100 Hz · 立体声（2 声道）")
        );
        assert_eq!(
            dynamic_text("PCM ('lpcm') · 48000 Hz · stereo (2 channels) · 8-bit unsigned integer")
                .as_deref(),
            Some("PCM ('lpcm') · 48000 Hz · 立体声（2 声道） · 8 位无符号整数")
        );
        assert_eq!(
            dynamic_text(
                "Audio (0x00000001) · unknown sample rate · 6 channels · 32-bit floating point"
            )
            .as_deref(),
            Some("音频 (0x00000001) · 采样率未知 · 6 声道 · 32 位浮点数")
        );
        for source_value in [
            "customer_name · 10 rows × 2 columns",
            "STRUCT · name rows × 2 columns",
            "1.1.2 / 5.0 s",
            "NaN / 5.0 s",
            "PCM (file_name) · 8000 Hz · mono (1 channel)",
        ] {
            assert!(dynamic_text(source_value).is_none());
        }
    }

    #[test]
    fn nested_type_names_stay_english_while_counts_and_actions_are_localized() {
        for (kind, unit, translated_unit) in
            [("Map", "entries", "个条目"), ("Union", "item", "个元素")]
        {
            assert_eq!(
                dynamic_text(&format!("{kind} · 4 {unit} · Open table")),
                Some(format!("{kind} · 4 {translated_unit} · 打开表格"))
            );
        }
        for kind in [
            "STRUCT",
            "JSON object",
            "LIST of records",
            "JSON array of objects",
            "LIST / ARRAY",
            "MAP",
        ] {
            assert_eq!(
                dynamic_text(&format!("{kind} · 17 rows × 3 columns")),
                Some(format!("{kind} · 17 行 × 3 列"))
            );
        }
        assert_eq!(
            dynamic_text("Row 42 · Struct List Map Union · STRUCT(flag BOOLEAN, at TIMESTAMP)"),
            Some("第 42 行 · Struct List Map Union · STRUCT(flag BOOLEAN, at TIMESTAMP)".into())
        );
        assert!(dynamic_text("[\"Struct\",\"List\",\"Map\",\"Union\"]").is_none());
    }

    #[test]
    fn temporal_precision_errors_keep_exact_type_tags_and_require_numeric_counts() {
        for data_type in ["TIME", "TIMETZ", "TIMESTAMPTZ"] {
            for digits in [0, 3, 6, 9] {
                assert_eq!(
                    dynamic_text(&format!(
                        "{data_type} supports at most {digits} fractional digits."
                    )),
                    Some(format!("{data_type} 最多支持 {digits} 位小数秒。"))
                );
            }
        }
        for source_value in [
            "TIME supports at most  fractional digits.",
            "TIME supports at most three fractional digits.",
            "TIME supports at most +3 fractional digits.",
            "TIME supports at most 3.0 fractional digits.",
            "TIME supports at most ３ fractional digits.",
            "TIME supports at most 3 fractional digits. suffix",
            "VARCHAR supports at most 3 fractional digits.",
            "prefix TIME supports at most 3 fractional digits.",
            "\"TIME supports at most 3 fractional digits.\"",
        ] {
            assert!(dynamic_text(source_value).is_none(), "{source_value:?}");
        }
    }

    #[test]
    fn status_ranges_require_numbers_and_preserve_interpolated_data() {
        assert_eq!(
            dynamic_text("Showing 1000 Results").as_deref(),
            Some("显示 1000 条结果")
        );
        assert_eq!(
            dynamic_text("Loaded 4096 to 5096   Out of unknown (lazy text source)").as_deref(),
            Some("已载入 4096–5096   总行数未知（按需读取）")
        );
        assert_eq!(
            dynamic_text("Saved /tmp/Saved 中文😀.csv").as_deref(),
            Some("已保存：/tmp/Saved 中文😀.csv")
        );
        assert_eq!(
            dynamic_text("Row 42 · employee_name · VARCHAR").as_deref(),
            Some("第 42 行 · employee_name · VARCHAR")
        );
        assert!(dynamic_text("Showing employee Results").is_none());
        assert!(dynamic_text("Loaded x to 5   Out of 8").is_none());
        assert!(dynamic_text("/tmp/Showing 1000 Results.parquet").is_none());
        assert!(dynamic_text("[1,null,3]").is_none());
    }

    #[test]
    fn import_rules_preserve_quoted_user_tokens_and_filename_separators() {
        assert_eq!(dynamic_text("Selected field NULL rule: empty field or \"NA or none\" or \"empty field\"; exact whitespace; case sensitive").as_deref(), Some("所选字段 NULL 规则：空字段 或 \"NA or none\" 或 \"empty field\"；精确匹配空白；区分大小写"));
        assert_eq!(
            dynamic_text("file · 中文.records · 12.0 MiB · 1000 records sampled (first 1000 only)")
                .as_deref(),
            Some("file · 中文.records · 12.0 MiB · 已采样 1000 条记录（仅前 1000 条）")
        );
        assert_eq!(
            dynamic_text("PNG image · 16516 bytes").as_deref(),
            Some("PNG 图片 · 16516 字节")
        );
        assert_eq!(
            dynamic_text(
                "Duplicate header \"a was renamed to b\" was renamed to \"a was renamed to b_2\"."
            )
            .as_deref(),
            Some("重复表头 \"a was renamed to b\" 已重命名为 \"a was renamed to b_2\"。")
        );
        assert_eq!(
            dynamic_text("Column \"name: with detail\": 2 sampled value(s) do not match BIGINT; choose VARCHAR or correct the type/missing rule before loading.").as_deref(),
            Some("字段 \"name: with detail\"：2 个样本值不符合 BIGINT；请在载入前选择 VARCHAR，或修正类型和缺失值规则。")
        );
    }
}
