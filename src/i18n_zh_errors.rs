//! Localize app diagnostics while preserving paths, field names, values and external errors.

fn fixed(text: &str) -> Option<&'static str> {
    Some(match text {
        "Could not create the text record index" => "无法创建文本记录索引",
        "Incomplete text record checkpoint" => "文本记录的索引位置不完整",
        "Destination needs a parent folder" | "Export destination has no parent folder" => {
            "目标文件需要上级文件夹"
        }
        "Destination needs a file name" | "Export destination needs a file name" => {
            "请指定目标文件名"
        }
        "Export destination folder does not exist" => "导出目标文件夹不存在",
        "Could not save the exported file" => "无法保存导出文件",
        "Arrow export schema differs from the snapshot" => "导出字段结构与快照不一致",
        "Excel supports at most 16,384 columns." => "Excel 最多支持 16,384 列。",
        "Excel supports at most 1,048,575 data rows plus the header." => {
            "除表头外，Excel 最多支持 1,048,575 行数据。"
        }
        "Excel text cells support at most 32,767 UTF-16 units. Choose a format without Excel's cell-length limit."
        | "Legacy XLS text cells support at most 32,767 UTF-16 units. Choose a format without Excel's cell-length limit." => {
            "Excel 文本单元格最多容纳 32,767 个 UTF-16 代码单元。请选择没有此单元格长度限制的格式。"
        }
        "Legacy XLS supports at most 256 columns. Choose XLSX instead." => {
            "旧版 XLS 最多支持 256 列。请改用 XLSX。"
        }
        "Legacy XLS supports at most 65,535 data rows plus the header. Choose XLSX instead." => {
            "除表头外，旧版 XLS 最多支持 65,535 行数据。请改用 XLSX。"
        }
        "Legacy XLS workbook offsets exceed the format's 32-bit range. Choose XLSX instead." => {
            "工作簿的偏移量超过旧版 XLS 的 32 位范围。请改用 XLSX。"
        }
        "Legacy XLS record is too large. Choose XLSX instead." => {
            "记录超过旧版 XLS 的大小限制。请改用 XLSX。"
        }
        "Legacy XLS shared-string reference count exceeds its signed 32-bit format limit." => {
            "共享字符串的引用数量超出旧版 XLS 的 32 位有符号整数范围。"
        }
        "Legacy XLS has too many unique strings." => "不同字符串的数量超过旧版 XLS 的限制。",
        "Legacy XLS exceeds the CFB v3 2 GiB file limit. Choose XLSX instead." => {
            "旧版 XLS 超过 CFB v3 的 2 GiB 文件上限。请改用 XLSX。"
        }
        "Dataset row count exceeds addressable memory" => "数据集行数超过可寻址范围",
        "No Parquet schema was found" => "未找到 Parquet 字段结构",
        "Parquet row group range overflow" => "Parquet 行组范围溢出",
        "Expected i64 time storage" => "时间值需要使用 i64 存储",
        "Time value cannot be represented exactly at nanosecond precision" => {
            "无法以纳秒精度准确表示这个时间值"
        }
        "Invalid text file timestamp buffers" => "文本文件的时间戳缓冲区无效",
        "A source filter must return Boolean values" => "源数据筛选表达式必须返回布尔值",
        "An open-ended row range requires a delimited text file." => {
            "只有分隔文本文件支持载入到末尾的行范围。"
        }
        "Record count and offset must not exceed 9,223,372,036,854,775,807." => {
            "记录数和偏移量不能超过 9,223,372,036,854,775,807。"
        }
        "Advanced SQL is no longer supported. Use the visual filters." => {
            "请使用可视化筛选，高级 SQL 已停用。"
        }
        "Conflicting sort rules" => "排序规则相互冲突",
        "Add a filter condition before scanning the entire file." => {
            "扫描整个文件前，请先添加一个筛选条件。"
        }
        "NUL is not allowed in filter expressions." => "筛选表达式不能包含 NUL 字符。",
        "Comments are not allowed in filter expressions." => "筛选表达式不能包含注释。",
        "Only scalar filter expressions are allowed." => "筛选只支持标量表达式。",
        "The requested result snapshot is no longer available. Reload the file." => {
            "请求的结果快照已失效，请重新载入文件。"
        }
        "Could not start DataFusion CPU workers" => "无法启动 DataFusion 工作线程",
        "DataFusion returned an invalid row-index schema" => "DataFusion 返回的行索引结构无效",
        "Cancellation monitor failed" => "取消操作监控失败",
        "Invalid aggregation result schema" => "汇总结果的字段结构无效",
        "Invalid aggregation row count" => "汇总结果的行数无效",
        "Aggregation row count overflow" => "汇总结果的行数溢出",
        "Aggregation input differs from the displayed snapshot" => "汇总输入与当前显示的快照不一致",
        "This audio cell is empty." => "此音频单元格为空。",
        "macOS Foundation is unavailable." => "macOS Foundation 框架不可用。",
        "macOS audio playback is unavailable." => "macOS 音频播放功能不可用。",
        "Could not allocate memory for the audio cell." => "无法为此音频单元格分配内存。",
        "Could not copy the audio cell into native memory." => "无法将音频单元格复制到原生内存。",
        "Could not allocate a native audio player." => "无法创建系统音频播放器。",
        "macOS could not start audio playback. Check the audio format and output device." => {
            "macOS 无法开始播放，请检查音频格式和输出设备。"
        }
        "the data is corrupt or its codec is unsupported by macOS" => {
            "数据已损坏，或 macOS 不支持其音频编码"
        }
        "macOS could not decode the audio data" => "macOS 无法解码音频数据",
        _ => return None,
    })
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|ch| ch.is_ascii_digit())
}

fn precision_scale(text: &str) -> bool {
    text.split_once(',')
        .is_some_and(|(p, s)| digits(p) && digits(s))
}

fn csv_utf8(text: &str) -> Option<String> {
    let pair = text
        .strip_prefix("Text file data must be valid UTF-8 (data record ")?
        .strip_suffix(')')?;
    let (record, field) = pair.split_once(", field ")?;
    (digits(record) && digits(field)).then(|| {
        format!(
            "文本文件数据必须使用有效的 UTF-8 编码（第 {record} 条数据记录，第 {field} 个字段）"
        )
    })
}

fn csv_record(text: &str) -> Option<String> {
    let record = text.strip_prefix("Text data record ")?;
    if let Some((record, sizes)) = record.split_once(" has ")
        && let Some((actual, expected)) = sizes.split_once(" fields, expected ")
        && [record, actual, expected].into_iter().all(digits)
    {
        return Some(format!(
            "第 {record} 条数据记录有 {actual} 个字段，应有 {expected} 个"
        ));
    }
    let (record, field) = record.split_once(", field ")?;
    let (field, description) = field.split_once(" (")?;
    if !digits(record) || !digits(field) {
        return None;
    }
    let location = format!("第 {record} 条数据记录的第 {field} 个字段");
    if let Some(name) = description.strip_suffix(
        ") has sub-microsecond Timestamp precision. Increase the inference sample or choose Text.",
    ) {
        return Some(format!(
            "{location}（{name}）的时间戳精度高于微秒。请扩大类型推断样本或选择文本类型。"
        ));
    }
    let details =
        description.strip_suffix(") exactly. Increase the inference sample or choose Text.")?;
    let (name, precision) = details.rsplit_once(") cannot fit Decimal(")?;
    precision_scale(precision).then(|| format!("{location}（{name}）的值无法用 Decimal({precision}) 精确表示。请扩大类型推断样本或选择文本类型。"))
}

fn filter_error(text: &str) -> Option<String> {
    if let Some((bits, kind)) = text
        .strip_prefix("The whole number is outside the ")
        .and_then(|value| value.strip_suffix(" range."))
        .and_then(|value| value.split_once("-bit "))
        && matches!(bits, "8" | "16" | "32" | "64" | "128")
    {
        let kind = match kind {
            "signed" => "有符号",
            "unsigned" => "无符号",
            _ => return None,
        };
        return Some(format!("这个整数超出 {bits} 位{kind}整数的范围。"));
    }
    for (start, end, description) in [
        (
            "The value has too many integer digits for DECIMAL(",
            ").",
            "整数位数超出范围",
        ),
        (
            "The value has too many fractional digits for DECIMAL(",
            "); rounding is not applied.",
            "小数位数超出范围，无法按原精度保存",
        ),
    ] {
        if let Some(precision) = text
            .strip_prefix(start)
            .and_then(|value| value.strip_suffix(end))
            .filter(|value| precision_scale(value))
        {
            return Some(format!("该数值的{description}（DECIMAL({precision})）。"));
        }
    }
    if let Some((op, kind)) = text
        .strip_suffix(" fields.")
        .and_then(|msg| msg.split_once(" is not supported for "))
        && matches!(
            op,
            "Equals"
                | "Does not equal"
                | "Contains"
                | "Does not contain"
                | "Starts with"
                | "Ends with"
                | "Greater than"
                | "Greater than or equal"
                | "Less than"
                | "Less than or equal"
                | "Between (inclusive)"
                | "Is NULL"
                | "Is not NULL"
                | "Is empty"
                | "Is not empty"
        )
        && let Some(label) = super::zh::static_text(op)
    {
        return Some(format!("{kind} 类型的字段不支持“{label}”。"));
    }
    let count = text
        .strip_prefix("This timestamp type supports at most ")?
        .strip_suffix(" fractional digits.")?;
    digits(count).then(|| format!("此时间戳类型最多支持 {count} 位小数秒。"))
}

/// Match a Rust debug string. Quoted field names may contain colons, escapes or diagnostics.
fn quoted(text: &str) -> Option<(&str, &str)> {
    let content = text.strip_prefix('"')?;
    let mut escaped = false;
    for (i, ch) in content.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(text.split_at(i + 2));
        }
    }
    None
}

fn frozen_width(text: &str) -> Option<String> {
    let sizes = text.strip_prefix("Cannot freeze these columns: the frozen area needs ")?.strip_suffix(" px is available while keeping 180 px for the table. Unfreeze columns, reduce their widths, or close the sidebar.")?;
    let (required, available) = sizes.split_once(" px, but only ")?;
    (digits(required) && digits(available)).then(|| format!("冻结这些列需要 {required} 像素宽。为表格保留 180 像素后，只剩 {available} 像素。请取消冻结部分列、缩小列宽或关闭侧栏。"))
}

/// Recognize known error contexts so colons in paths, values and external errors remain literal.
fn context_label(text: &str) -> Option<&'static str> {
    match text {
        "Could not read saved views."
        | "Saved views file is invalid."
        | "Could not save views."
        | "Could not identify the source for saved views."
        | "Saved view contains an invalid Arrow Schema."
        | "Could not read the first text record"
        | "Could not read the text file sample"
        | "Could not index the text file records"
        | "Could not load data or apply the filter"
        | "Could not initialize data engine"
        | "Could not bind the delimited source with the selected settings"
        | "Text file headers must contain valid UTF-8"
        | "Invalid upper bound."
        | "Choose an available field."
        | "Choose an available Struct field."
        | "The field name is ambiguous."
        | "The Struct field name is ambiguous."
        | "A field name cannot contain NUL."
        | "The value cannot contain NUL."
        | "Only NULL checks are supported for this field type."
        | "Source data is read-only. Choose a different destination; source files and their aliases cannot be overwritten."
        | "Invalid time"
        | "Invalid date/time"
        | "Invalid date/time or UTC offset"
        | "Not enough query memory to calculate this summary. Reduce the number of rows or groups."
        | "Source changed after loading. Reload the file." => super::zh::static_text(text),
        prompt if prompt.starts_with("Enter ") && prompt.ends_with('.') => {
            super::zh::static_text(prompt)
        }
        _ => fixed(text),
    }
}

fn append_cause(context: String, tail: &str, depth: usize) -> Option<String> {
    if tail.is_empty() {
        return Some(context);
    }
    let cause = tail.strip_prefix(": ")?;
    let translated = diagnostic(cause, depth - 1);
    Some(format!(
        "{}：{}",
        context.trim_end_matches('。'),
        translated.as_deref().unwrap_or(cause)
    ))
}

fn diagnostic(text: &str, depth: usize) -> Option<String> {
    if depth == 0 {
        return None;
    }
    if let Some(label) = context_label(text) {
        return Some(label.to_owned());
    }
    if let Some((head, _)) = text.split_once(": ")
        && let Some(label) = context_label(head)
    {
        return append_cause(label.to_owned(), &text[head.len()..], depth);
    }
    if let Some(info) = text.strip_prefix("Condition ")
        && let Some((number, field)) = info.split_once(" · field ")
        && digits(number)
        && let Some((name, tail)) = quoted(field)
    {
        let context = format!("条件 {number} · 字段 {name}");
        return append_cause(context, tail, depth);
    }
    if let Some(message) = csv_utf8(text)
        .or_else(|| csv_record(text))
        .or_else(|| filter_error(text))
        .or_else(|| frozen_width(text))
    {
        return Some(message);
    }
    if let Some(range) = text.strip_prefix("Could not decode text data records ")
        && let Some((start, rest)) = range.split_once(" to ")
        && let Some((end, tail)) = rest.split_once(". Check the column types.")
        && digits(start)
        && digits(end)
    {
        let label = format!("无法解码第 {start} 至 {end} 条文本数据记录。请检查列类型。");
        return append_cause(label, tail, depth);
    }
    if let Some(details) = text.strip_prefix("Could not parse Timestamp at text data record ")
        && let Some((record, rest)) = details.split_once(", field ")
        && let Some((field, name_and_cause)) = rest.split_once(" (")
        && digits(record)
        && digits(field)
        && (name_and_cause.ends_with(')') || name_and_cause.contains("): "))
    {
        return Some(format!(
            "无法解析第 {record} 条文本数据记录中第 {field} 个字段的时间戳 ({name_and_cause}"
        ));
    }
    if let Some(detail) = text.strip_prefix("Cannot open this audio cell: ") {
        return Some(format!(
            "无法打开此音频单元格：{}",
            fixed(detail).unwrap_or(detail)
        ));
    }
    if let Some(argument) = text.strip_prefix("Could not read ")
        && let Some((path, tail)) = argument.split_once(" as Parquet")
    {
        // A path and an external diagnostic can contain the same phrase. Preserve their order.
        if tail.contains(" as Parquet") {
            return Some(format!("读取失败：{argument}"));
        }
        let label = format!("无法将 {path} 读取为 Parquet");
        return match tail {
            "" => Some(label),
            tail => Some(format!("{label}：{}", tail.strip_prefix(": ")?)),
        };
    }
    for (prefix, suffix, translated) in [
        (
            "No loadable fields remain in ",
            ": all fields conflict ignoring case. Case-sensitive duplicate fields cannot be loaded.",
            "中的字段名在忽略大小写后全部冲突，没有可载入的字段。",
        ),
        (
            "Different Parquet schemas were found. ",
            " does not match the first file. Open matching partitions together.",
            "的 Parquet 字段结构与首个文件不一致。请一起打开结构匹配的分区。",
        ),
    ] {
        if let Some((path, tail)) = text
            .strip_prefix(prefix)
            .and_then(|body| body.rsplit_once(suffix))
        {
            let label = format!("{path}{translated}");
            return append_cause(label, tail, depth);
        }
    }
    // These templates end in an unquoted parameter. Preserve the full suffix verbatim.
    for (prefix, translated) in [
        ("File does not exist: ", "文件不存在："),
        ("Could not open ", "无法打开 "),
        ("Unknown column: ", "未知列："),
        ("Duplicate selected column: ", "重复选择了列："),
        ("Duplicate selected field: ", "重复选择了字段："),
        ("Selected field is not available: ", "所选字段不可用："),
        ("Sort column is not loaded: ", "排序列尚未载入："),
        ("Duplicate export column: ", "重复导出列："),
        (
            "Export column is not present in the displayed snapshot: ",
            "当前显示的快照中没有要导出的列：",
        ),
        ("DataFusion execution failed: ", "DataFusion 执行失败："),
        ("DataFusion aggregation failed: ", "DataFusion 汇总失败："),
    ] {
        if let Some(argument) = text.strip_prefix(prefix).filter(|value| !value.is_empty()) {
            return Some(format!("{translated}{argument}"));
        }
    }
    None
}

pub(super) fn error_text(text: &str) -> Option<String> {
    diagnostic(text, 12)
}

#[cfg(test)]
mod tests {
    use super::error_text;

    #[test]
    fn fixed_width_integer_overflow_reports_the_signedness_and_width() {
        for (sign, expected) in [("signed", "有符号"), ("unsigned", "无符号")] {
            for width in [8, 16, 32, 64, 128] {
                let input = format!("The whole number is outside the {width}-bit {sign} range.");
                assert_eq!(
                    error_text(&input),
                    Some(format!("这个整数超出 {width} 位{expected}整数的范围。"))
                );
            }
        }
        assert!(error_text("The whole number is outside the xyz-bit signed range.").is_none());
    }

    #[test]
    fn filenames_fields_and_external_diagnostics_remain_verbatim() {
        assert_eq!(
            error_text("Could not read /tmp/Saved: Open.csv as Parquet: backend: Open").as_deref(),
            Some("无法将 /tmp/Saved: Open.csv 读取为 Parquet：backend: Open")
        );
        assert_eq!(error_text(r#"Condition 3 · field "Open: \"Saved\"": Enter a whole number in range.: invalid digit found in string"#).as_deref(),
            Some(r#"条件 3 · 字段 "Open: \"Saved\""：请输入有效范围内的整数：invalid digit found in string"#));
        assert_eq!(
            error_text("Could not read saved views.: backend: Saved /file").as_deref(),
            Some("无法读取已保存视图：backend: Saved /file")
        );
        assert_eq!(
            error_text("Cannot open this audio cell: codec: Name").as_deref(),
            Some("无法打开此音频单元格：codec: Name")
        );
        assert_eq!(
            error_text("Could not read saved views.: Open").as_deref(),
            Some("无法读取已保存视图：Open")
        );
        assert_eq!(
            error_text("Could not read A as Parquet: backend also tried X as Parquet").as_deref(),
            Some("读取失败：A as Parquet: backend also tried X as Parquet")
        );
        for raw in [
            "/tmp/Could not open file",
            "Open: Error",
            "Condition bad · field \"name\"",
            "Text data record hi has 1 fields, expected 2",
        ] {
            assert_eq!(error_text(raw), None);
        }
    }

    #[test]
    fn csv_and_frozen_width_keep_numeric_parameters_and_field_names() {
        assert_eq!(error_text("Text data record 20, field 2 (Amount): new) cannot fit Decimal(12,2) exactly. Increase the inference sample or choose Text.").as_deref(),
            Some("第 20 条数据记录的第 2 个字段（Amount): new）的值无法用 Decimal(12,2) 精确表示。请扩大类型推断样本或选择文本类型。"));
        assert!(error_text("Cannot freeze these columns: the frozen area needs 530 px, but only 440 px is available while keeping 180 px for the table. Unfreeze columns, reduce their widths, or close the sidebar.").unwrap().contains("440 像素"));
    }
}
