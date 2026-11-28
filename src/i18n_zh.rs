//! Simplified Chinese labels for application-owned text. Unknown text is preserved.
//! Field names, paths, query text and data values are never translation keys.

pub(super) fn static_text(text: &str) -> Option<&'static str> {
    Some(match text {
        "Run" => "运行",
        "Add a condition to choose which source rows to import." => {
            "添加条件，选择要载入的源数据行。"
        }
        "Filter scope" => "筛选范围",
        "Loaded range" => "载入范围",
        "Entire file" => "整个文件",
        "Scan the entire source and import only matching rows." => {
            "扫描整个数据源，仅载入命中的行。"
        }
        "Add a filter condition before scanning the entire file." => {
            "请先添加筛选条件，再扫描整个文件。"
        }
        "Not enough query memory to calculate this summary. Reduce the number of rows or groups." => {
            "计算此汇总所需的内存不足。请减少载入的行数或分组数。"
        }
        "Not enough memory to calculate exact Category counts. Reduce the loaded range." => {
            "可用内存不足以精确统计各类别的数量。请缩小载入范围。"
        }
        "Elapsed {time}" => "已用 {time}",
        "Elapsed time for the current operation." => "当前操作已用的时间。",
        "Query {time}" => "查询 {time}",
        "Elapsed time for the displayed query." => "生成当前查询结果所用的时间。",
        "{count} files" => "{count} 个文件",
        "1 field" => "1 个字段",
        "{count} fields" => "{count} 个字段",
        "1 row" => "1 行",
        "{count} rows" => "{count} 行",
        "1 row shown" => "显示 1 行",
        "{count} rows shown" => "显示 {count} 行",
        "1 group row" => "组内 1 行",
        "{count} group rows" => "组内 {count} 行",
        "Rows in parent result" => "分组前的结果行数",
        "Matches in entire file" => "整个文件中匹配的行数",
        "Matches in loaded range" => "载入范围内匹配的行数",
        "Filtered within loaded range" => "已在载入范围内筛选",
        "Loaded rows" => "已载入行数",
        "1 row available" => "可浏览 1 行",
        "{count} rows available" => "可浏览 {count} 行",
        "Scroll to read indexed rows from the text file." => {
            "滚动即可浏览文本文件中已建立索引的行。"
        }
        "Scanned {bytes} of {total}" => "已扫描 {bytes}，共 {total}",
        "Indexed rows: {rows}" => "已建立索引的行数：{rows}",
        "Indexing {percent}%" => "正在建立索引 {percent}%",
        "Indexing stopped" => "索引已停止",
        "Browse Text File" => "浏览文本文件",
        "Go to Row…" => "跳转到行…",
        "Source row number" => "源文件行号",
        "Go" => "跳转",
        "Cancel pending jump" => "取消等待跳转",
        "Waiting for row {row}" => "正在等待第 {row} 行",
        "Indexing stopped before the requested row." => "索引已停止，尚未到达所请求的行。",
        "The requested row is beyond the end of the file." => "所请求的行超出了文件末尾。",
        "A row beyond the indexed range opens when indexing reaches it." => {
            "如果所请求的行尚未建立索引，读到该行时会自动跳转。"
        }
        "Jump to the first source row" => "跳到源文件的第一行",
        "Jump to the last indexed row" => "跳到已建立索引的最后一行",
        "Expected a delimited text source" => "此操作需要分隔文本文件",
        "Choose which source rows to use for this operation." => {
            "请选择本次操作要使用的源文件行范围。"
        }
        "Total source row count is unknown." => "源文件的总行数未知。",
        "The selection is too large for the clipboard. Select fewer cells or export a range." => {
            "所选内容超出了剪贴板的大小限制。请减少所选单元格，或导出所需范围。"
        }
        "Rows processed: {rows}" => "已处理行数：{rows}",
        "Rows processed: {rows} / {total}" => "已处理行数：{rows} / {total}",
        "No rows loaded" => "未载入记录",
        "Rows {first}–{last} of {total}" => "第 {first}–{last} 行（共 {total} 行）",
        "Rows {first}–{last}" => "第 {first}–{last} 行",
        "Time grouping" => "时间分组",
        "Exact value" => "按原值",
        "Selected keys" => "已选键字段",
        "Day" => "按天",
        "Week" => "按周",
        "Month" => "按月",
        "Quarter" => "按季度",
        "Year" => "按年",
        "Day of month" => "日（1–31）",
        "Week of year" => "周（1–53）",
        "Month of year" => "月（1–12）",
        "Quarter of year" => "季度（1–4）",
        "Calendar year" => "年",
        "Day part" => "日",
        "Week part" => "周",
        "Month part" => "月",
        "Quarter part" => "季度",
        "Year part" => "年",
        "Weeks start on Monday. Timestamp grouping uses the column time zone." => {
            "每周从星期一开始。Timestamp 按字段时区分组。"
        }
        "StdDev and percentiles use Float64. Exact percentiles use DataFusion interpolation. Missing ratio measures NULL as a fraction." => {
            "标准差和分位数使用 Float64。精确分位数按 DataFusion 规则插值。缺失比例为 NULL 行数占比。"
        }
        "Missing ratio" => "缺失比例",
        "StdDev (sample)" => "样本标准差",
        "StdDev (population)" => "总体标准差",
        "Check Duplicate Keys" => "检查重复键",
        "Check Duplicate Keys…" => "检查重复键…",
        "Compare Parquet Versions" => "比较 Parquet 版本",
        "Compare Parquet Versions…" => "比较 Parquet 版本…",
        "Compare File Versions" => "比较文件版本",
        "Compare File Versions…" => "比较文件版本…",
        "Choose a file version to compare" => "选择要比较的文件",
        "Open a single file before comparing versions." => "请先打开单个文件再比较版本。",
        "Open a single Parquet file before comparing versions." => {
            "请先打开单个 Parquet 文件，再比较版本。"
        }
        "Choose a Parquet version to compare" => "选择要比较的 Parquet 文件",
        "Checks the current result. Matching NULL key values are grouped together." => {
            "检查当前结果。键中同一位置的 NULL 视为相同。"
        }
        "Compares both complete files. Choose a unique, non-NULL key; common fields of the same type are compared." => {
            "比较两个完整文件。请选择唯一且无 NULL 的键，程序会比较同名同类型的字段。"
        }
        "Duplicate groups" => "重复键组数",
        "Rows with duplicate keys" => "重复键涉及行数",
        "No Schema differences." => "Schema 一致。",
        "Schema differences" => "Schema 差异",
        "Row changes" => "行差异",
        "Added" => "新增",
        "Removed" => "删除",
        "Modified" => "修改",
        "Original file" => "原文件",
        "Comparison file" => "对比文件",
        "Key fields" => "键字段",
        "No matching scalar key fields." => "没有可用的标量键字段。",
        "Checking data…" => "正在检查数据…",
        "Comparing fields" => "正在比较字段",
        "Key occurrences" => "键出现次数",
        "Comparison row number" => "对比文件行号",
        "Change" => "变化",
        "Field" => "字段",
        "Choose matching scalar key fields." => "请选择两边类型一致的标量键字段。",
        "Choose at least one key field." => "请至少选择一个键字段。",
        "Original file contains NULL or duplicate keys." => "原文件的键含有 NULL 或重复值。",
        "Comparison file contains NULL or duplicate keys." => "对比文件的键含有 NULL 或重复值。",
        "Median (exact)" => "中位数（精确）",
        "Median (approx.)" => "中位数（近似）",
        "P25 (exact)" => "P25（精确）",
        "P25 (approx.)" => "P25（近似）",
        "P75 (exact)" => "P75（精确）",
        "P75 (approx.)" => "P75（近似）",
        "P90 (exact)" => "P90（精确）",
        "P90 (approx.)" => "P90（近似）",
        "P95 (exact)" => "P95（精确）",
        "P95 (approx.)" => "P95（近似）",
        "Parquet reader returned an incomplete row range." => {
            "Parquet reader 返回的行数少于请求范围。"
        }
        "Choose at least one aggregation." => "请至少选择一种汇总。",
        "Count rows does not take a field." => "Count rows 不接受字段。",
        "Choose an aggregation field." => "请选择一个汇总字段。",
        "Decimal aggregation cannot safely preserve this precision for the current number of rows. Reduce the imported row range or use a lower-precision field." => {
            "无法在当前行数下安全保留 Decimal 精度。请缩小载入范围或选择精度较低的字段。"
        }
        "Saved view contains an invalid Arrow Schema." => "已保存视图中的 Arrow Schema 无效。",
        "Original row number" => "原始行号",
        "Go to Row" => "跳到该行",
        "Saved Views" => "已保存视图",
        "Saved Views…" => "已保存视图…",
        "Saved view has an invalid loading range." => "已保存视图的载入范围无效。",
        "Saved view has no selected fields." => "已保存视图中没有所选字段。",
        "Saved view contains too many frozen rows." => "已保存视图中的冻结行过多。",
        "Saved view does not match the current Schema." => "已保存视图与当前 Schema 不匹配。",
        "Saved view contains invalid column settings." => "已保存视图包含无效的列设置。",
        "Could not read saved views." => "无法读取已保存视图。",
        "Saved views file is too large." => "视图配置文件过大。",
        "Saved views file is invalid." => "视图配置文件无效。",
        "Saved views file uses an unsupported version." => "视图配置文件的版本不受支持。",
        "Saved views need a parent directory." => "视图配置文件需要一个上级目录。",
        "Could not save views." => "无法保存视图。",
        "Could not identify the source for saved views." => "无法识别已保存视图对应的来源。",
        "Open a source before using saved views." => "请先打开数据来源，再使用已保存视图。",
        "Operation cancelled" => "操作已取消",
        "Source files changed. Saved frozen rows were skipped." => {
            "源文件已变化，已跳过保存的冻结行。"
        }
        "Some saved frozen rows are absent from this result." => {
            "当前结果中缺少部分已保存的冻结行。"
        }
        "View name" => "视图名称",
        "Enter a view name from 1 to 80 characters." => "请输入 1 到 80 个字符的视图名称。",
        "A saved view already has that name." => "已有同名的保存视图。",
        "View saved. Frozen rows could not be saved safely." => {
            "视图已保存。冻结行的原始位置无法安全确认，已跳过。"
        }
        "View saved." => "视图已保存。",
        "Saved view removed." => "已移除保存的视图。",
        "Reload the source before applying a saved view." => "请重新载入来源，再恢复已保存的视图。",
        "Saved frozen columns do not fit this window." => "当前窗口空间不足，已跳过保存的冻结列。",
        "Saved view applied." => "已恢复保存的视图。",
        "Saved views store the fields, range, filters, sorting, widths and safely addressable frozen positions for this source." => {
            "视图会保存当前来源的字段、范围、筛选、排序、列宽和可安全定位的冻结位置。"
        }
        "Save Current View" => "保存当前视图",
        "Return to the full result before saving a view." => "请返回完整结果后再保存视图。",
        "Reading or saving views…" => "正在读取或保存视图…",
        "Applying saved view…" => "正在恢复已保存视图…",
        "No saved views for this source." => "此来源还没有已保存视图。",
        "Restore" => "恢复",
        "Descriptive Statistics" => "描述性统计",
        "Drag to reorder" => "拖动排序",
        "Statistics" => "统计",
        "Show or hide descriptive statistics" => "显示或隐藏描述性统计",
        "Statistics for all rows in the current result" => "统计当前结果中的全部行",
        "Calculating descriptive statistics…" => "正在计算描述性统计…",
        "Rows in current result" => "当前结果行数",
        "Missing count" => "缺失值数量",
        "Interpret as" => "按类型统计",
        "Auto" => "自动",
        "Category" => "Category",
        "nunique" => "nunique",
        "value_counts" => "value_counts",
        "Density plot" => "Density plot",
        "Relative density" => "相对密度",
        "25th percentile" => "第 25 百分位数",
        "Median" => "中位数",
        "75th percentile" => "第 75 百分位数",
        "90th percentile" => "第 90 百分位数",
        "95th percentile" => "第 95 百分位数",
        "Non-finite values" => "非有限值",
        "Boolean distribution" => "Boolean 分布",
        "No non-missing values to plot." => "没有可绘制的非缺失值。",
        "No finite values to plot." => "没有可绘制的有限数值。",
        "No groups to plot." => "没有可绘制的分组。",
        "Showing the first {shown} of {total} groups." => "共 {total} 个分组，绘制前 {shown} 个。",
        "Charts are disabled in Settings." => "图表已在设置中关闭。",
        "Bar charts are disabled in Settings." => "已在设置中关闭柱形图。",
        "Chart skipped. The current result exceeds the chart row limit of {limit}." => {
            "当前结果超过 {limit} 行，已跳过绘图。"
        }
        "Chart Data Limits" => "绘图数据上限",
        "Statistical Analysis" => "统计分析",
        "Summary results" => "汇总结果",
        "Summary charts" => "汇总图表",
        "Maximum bars per chart" => "每张图的最大柱数",
        "… ({count} bytes total; Save Value writes the original bytes)" => {
            "…（共 {count} 字节，「保存值」会写出原始字节）"
        }
        "Enter a whole number from 1 to {limit}." => "请输入 1 到 {limit} 之间的整数。",
        "Use Defaults" => "恢复默认值",
        "Most frequent categories (up to {limit})" => "出现次数最多的类别（最多 {limit} 个）",
        "All categories, sorted by frequency" => "所有类别，按频数排序",
        "Maximum result rows for statistics charts" => "允许绘制图表的最大结果行数",
        "Above the limit, statistics still run. Choose 0 to turn off charts or INF for all rows." => {
            "超出上限时仍计算统计量。选 0 关闭图表，选 INF 取消行数上限。"
        }
        "Choose 0 to hide bar charts or INF to show all bars." => {
            "选 0 隐藏柱形图，选 INF 显示所有柱。"
        }
        "Enter a nonnegative whole number or INF." => "请输入非负整数或 INF。",
        "100K" => "10万",
        "1M" => "100万",
        "10M" => "1000万",
        "Export Summary Results" => "导出汇总结果",
        "Choose a format. The macOS save dialog will open." => {
            "请选择格式，随后会打开 macOS 保存对话框。"
        }
        "The displayed result changed before export. Try again." => {
            "导出前显示结果已变化，请重试。"
        }
        "Exporting summary results…" => "正在导出汇总结果…",
        "Data engine lock was poisoned" => "数据引擎状态异常",
        "Finding group details…" => "正在查找分组明细…",
        "Row count must not specify a field" => "Count rows 不接受字段",
        "Summary field is no longer available" => "汇总字段已不可用",
        "Click a group to show its detail rows." => "点击分组查看对应明细。",
        "Back to all details" => "返回全部明细",
        "Export summary…" => "导出汇总…",
        "The displayed result changed. Run aggregation again." => "当前结果已改变，请重新汇总。",
        "Summary grouping fields are unavailable" => "汇总的分组字段不可用",
        "Summary row is unavailable" => "汇总行不可用",
        "The summary group has no matching detail rows" => "该汇总分组没有匹配的明细行",
        "Aggregation" => "分类汇总",
        "Show or hide aggregation    ⌥⌘G" => "显示或隐藏分类汇总    ⌥⌘G",
        "Group by" => "分类字段",
        "Measures" => "汇总项",
        "Summary field" => "汇总字段",
        "Summary method" => "汇总方式",
        "Add group field" => "添加分类字段",
        "Add measure" => "添加汇总项",
        "No grouping (whole result)" => "不分类，汇总整个结果",
        "All records" => "全部记录",
        "Count rows" => "Count rows（行数）",
        "Count" => "Count（非 NULL 计数）",
        "NUnique" => "nunique（去重计数）",
        "Sum" => "Sum（求和）",
        "Mean" => "Mean（平均值）",
        "Minimum" => "Min（最小值）",
        "Maximum" => "Max（最大值）",
        "Run aggregation" => "开始汇总",
        "Aggregating…" => "正在汇总…",
        "Finishing summary…" => "正在整理汇总结果…",
        "Preparing charts…" => "正在生成图表…",
        "Choose a file instead of a folder." => "请选择文件。",
        "The fields or filter changed. Importing will reset the table view." => {
            "字段结构已变化，或原筛选条件已不适用。确认导入后将重置表格视图。"
        }
        "Compares both complete files. Choose a unique, non-NULL key. Matching fields with compatible types are compared." => {
            "比较两个完整文件。请选择唯一且非 NULL 的键。比较名称相同且类型兼容的字段。"
        }
        "Reopen the text file with its saved separator and header settings." => {
            "请使用保存时的分隔符和表头设置重新打开文本文件。"
        }
        "Choose group fields" => "选择分类字段",
        "Choose summary field" => "选择汇总字段",
        "Choose a summary field or select All records." => "请选择汇总字段，或选择「全部记录」。",
        "Choose aggregation" => "选择汇总方式",
        "Done" => "完成",
        "Aggregation uses the current result rows." => "汇总当前结果中的行。",
        "Struct and List are not supported." => "暂不支持 Struct 和 List。",
        "Count and nunique exclude NULL." => "Count 和 nunique 均忽略 NULL。",
        "No aggregation results" => "尚无汇总结果",
        "Choose grouping fields and measures, then run." => "选择分类字段和汇总项，然后开始汇总。",
        "Aggregation request changed. Run again." => "汇总条件已更改，请重新汇总。",
        "Current result changed. Run aggregation again." => "明细结果已改变，请重新汇总。",
        "Add at least one summary measure." => "请至少添加一个汇总项。",
        "No supported fields" => "没有支持的字段",
        "groups" => "组",
        "input rows" => "条输入记录",
        "Preview" => "预览",
        "Preparing preview…" => "正在准备预览…",
        "Close preview" => "关闭预览",
        "List" => "List",
        "Struct" => "Struct",
        "Contains value" => "包含值",
        "Does not contain value" => "不包含值",
        "Empty List" => "空 List",
        "Nonempty List" => "非空 List",
        "Choose an available Struct field." => "请选择可用的 Struct 字段。",
        "The Struct field name is ambiguous." => "Struct 字段名称存在歧义。",
        "Struct fields are listed by path; List conditions match individual values." => {
            "Struct 字段按路径列出；List 条件匹配其中的元素。"
        }
        "Match a whole List element, using its original Data type." => {
            "按原始 Data type 匹配完整的 List 元素。"
        }
        "Match a whole List element; Text matching is case-sensitive." => {
            "匹配完整的 List 元素；Text 匹配区分大小写。"
        }
        "Check whether the List is empty or missing." => "检查 List 是否为空或为 NULL。",
        "Select a Struct field to compare its value." => "选择 Struct 内部字段，以比较其值。",
        "Enter a whole number using digits only." => "请输入仅由数字组成的整数。",
        "This field supports missing-value checks." => "此字段支持 NULL 检查。",
        "Enter a time in the format shown." => "请按提示的格式输入时间。",
        "Choose a date and enter the time; include a timezone when shown." => {
            "选择日期并输入时间；显示时区格式时，请填写时区。"
        }
        "Choose a date or enter YYYY-MM-DD." => "选择日期，或输入 YYYY-MM-DD。",
        "Choose true or false." => "选择 True 或 False。",
        "Text matching ignores case." => "文本匹配不区分大小写。",
        "Enter a number; decimals and large integers stay exact." => {
            "请输入数值；Decimal 和大整数会保持原始精度。"
        }
        "Upper bound" => "上限值",
        "Range" => "区间",
        "Type" => "Type",
        "Data type" => "Data type",
        "Number" => "Number",
        "Text" => "Text",
        "Boolean" => "Boolean",
        "Date" => "Date",
        "Date & time" => "Date & time",
        "Time" => "Time",
        "NULL checks only" => "仅检查 NULL",
        "On date" => "等于日期",
        "Not on date" => "不等于日期",
        "On or after" => "不早于",
        "On or before" => "不晚于",
        "Between dates (inclusive)" => "日期区间（含边界）",
        "At date/time" => "等于时间点",
        "Not at date/time" => "不等于时间点",
        "Between date/times (inclusive)" => "日期时间区间（含边界）",
        "At time" => "等于时间",
        "Not at time" => "不等于时间",
        "Between times (inclusive)" => "时间区间（含边界）",
        "At or after" => "不早于",
        "At or before" => "不晚于",
        "After" => "晚于",
        "Before" => "早于",
        "true / false" => "true / false",
        "Today" => "今天",
        "Mon" => "一",
        "Tue" => "二",
        "Wed" => "三",
        "Thu" => "四",
        "Fri" => "五",
        "Sat" => "六",
        "Sun" => "日",
        "Previous month" => "上个月",
        "Next month" => "下个月",
        "Change year" => "更改年份",
        "Go to year" => "跳到该年份",
        "Choose date" => "选择日期",
        "Choose a date" => "选择日期",
        "Pick date" => "选择日期",
        "Close calendar" => "关闭日历",
        "Choose operator" => "选择运算",
        "Choose time grouping" => "选择时间分组",
        "Choose at least one time grouping for {field}." => "请为「{field}」至少选择一种时间分组。",
        "Numeric value" => "输入数值",
        "Upper numeric value" => "输入数值上限",
        "Enter text" => "输入文本",
        "Lower value" => "下限值",
        "Row" => "行",
        "Total source rows" => "源数据总行数",
        "Rows remaining from offset" => "从当前偏移起剩余行数",
        "JSON preview: MAP keys are JSON strings; original key types and duplicate keys cannot be reconstructed." => {
            "JSON 预览中的 MAP 键为字符串，无法还原原始键类型及重复键。"
        }
        "JSON view follows the field schema; scalar formatting and original physical types may differ." => {
            "JSON 视图按字段结构显示，标量格式可能与原始物理类型不同。"
        }
        "\n… JSON preview truncated. Copy Node or Save Value for the full JSON." => {
            "\n… JSON 预览已截断。使用“复制节点”或“保存值”获取完整 JSON。"
        }
        "File" => "文件",
        "Edit" => "编辑",
        "Tools" => "工具",
        "Settings" => "设置",
        "Summary" => "汇总",
        "All" => "全部",
        "Range…" => "范围",
        "Export" => "导出",
        "Display" => "显示",
        "Loading" => "载入",
        "Help" => "帮助",
        "Language" => "语言",
        "English" => "英语",
        "Simplified Chinese" => "简体中文",
        "简体中文" => "简体中文",
        "New" => "新建",
        "New Window" => "新建窗口",
        "Clear Current File" => "清空当前文件",
        "Open" => "打开",
        "Open File…" => "打开文件…",
        "Import Text File…" => "导入文本文件…",
        "Open Data File" => "打开数据文件",
        "Open Parquet File" => "打开 Parquet 文件",
        "Browse, filter, and explore Parquet data." => "浏览、筛选并分析 Parquet 数据。",
        "Drop Parquet or delimited text files here" => "将 Parquet 文件或分隔文本文件拖到这里",
        "Or double-click to choose a file." => "也可以双击此处选择文件。",
        "Recent files will appear here." => "最近打开的文件会显示在这里。",
        "Close File" => "关闭文件",
        "Close Window" => "关闭窗口",
        "Minimize" => "最小化",
        "Enter Full Screen" => "进入全屏",
        "Exit Full Screen" => "退出全屏",
        "Window" => "窗口",
        "Main Window" => "主窗口",
        "Next Window" => "下一个窗口",
        "Previous Window" => "上一个窗口",
        "Quit Parquet Viewer" => "退出 Parquet Viewer",
        "Reload" => "重新载入",
        "Reimport…" => "重新导入…",
        "Jump to first loaded row (current result only)" => "跳到最前（仅当前已载入结果）",
        "Jump to last loaded row (current result only)" => "跳到最后（仅当前已载入结果）",
        "First" => "最前",
        "Last" => "最后",
        "First loaded row" => "已载入结果的首行",
        "Last loaded row" => "已载入结果的末行",
        "Export Results…" => "导出结果…",
        "Export…" => "导出…",
        "Export Current Results" => "导出当前结果",
        "Choose a format; the macOS save dialog will open." => {
            "选择导出格式，随后打开 macOS 保存对话框。"
        }
        "CSV (.csv)" => "CSV (.csv)",
        "JSON (.json)" => "JSON (.json)",
        "Excel (.xlsx)" => "Excel (.xlsx)",
        "Excel 97–2003 (.xls)" => "Excel 97–2003 (.xls)",
        "Parquet (.parquet)" => "Parquet (.parquet)",
        "Undo" => "撤销",
        "Redo" => "重做",
        "Cut" => "剪切",
        "Copy" => "复制",
        "Paste" => "粘贴",
        "Copy with Headers" => "连同表头复制",
        "Copy as WHERE" => "复制为 WHERE 条件",
        "Copy Value" => "复制值",
        "Copy Node" => "复制节点",
        "Copy Cell" => "复制单元格",
        "Copy Table" => "复制表格",
        "Table copied as TSV" => "已将表格复制为 TSV",
        "Copy Image" => "复制图片",
        "Copy SQL to Clipboard" => "复制 SQL 到剪贴板",
        "Copied to clipboard" => "已复制到剪贴板",
        "Select All" => "全选",
        "Select None" => "取消全选",
        "Fields" => "字段",
        "Fields…" => "选择字段…",
        "Choose Fields" => "选择字段",
        "Choose Fields…" => "选择字段…",
        "Choose fields before loading records." => "载入记录前，请先选择字段。",
        "Choose at least one field." => "请至少选择一个字段。",
        "Search fields, or name1, name2" => "搜索字段，或输入名称1, 名称2",
        "Load Range…" => "载入范围…",
        "Choose a row range or load all records." => "请选择载入的行范围，或载入全部记录。",
        "This dataset is at least {threshold}. Choose a row range or load all records." => {
            "数据集大小已达到 {threshold}。请选择载入的行范围，或载入全部记录。"
        }
        "Load All Records" => "载入全部记录",
        "Load All »" => "载入全部 »",
        "Toggle Dark Mode" => "切换深色模式",
        "Theme" => "主题",
        "Follow System" => "跟随系统",
        "Theme: Light" => "主题：浅色",
        "Theme: Dark" => "主题：深色",
        "Theme: Follow System" => "主题：跟随系统",
        "Memory" => "内存",
        "Current process memory footprint; refreshed every 2 seconds." => {
            "当前进程的实际内存占用，每 2 秒刷新。"
        }
        "Return to Cell" => "返回单元格",
        "Quick Peek" => "快速查看",
        "Duplicate name — excluded" => "名称重复，已排除",
        "Dark Mode" => "深色模式",
        "Dark" => "深色",
        "Light" => "浅色",
        "Filter Query ⓘ" => "筛选条件 ⓘ",
        "Filter Query (?)" => "筛选条件 (?)",
        "Filter" => "筛选",
        "Table & Filter Help" => "表格与筛选帮助",
        "Close" => "关闭",
        "More" => "更多",
        "More…" => "更多…",
        "More commands" => "更多操作",
        "Loading range" => "载入范围",
        "Loading Range" => "载入范围",
        "Loading range…" => "载入范围…",
        "Loading Range…" => "载入范围…",
        "Choose the source row window to load before filtering." => {
            "选择要载入的源数据行范围，再应用筛选。"
        }
        "Export…    ⌘S" => "导出…    ⌘S",
        "Choose Fields…    ⌘F" => "选择字段…    ⌘F",
        "Show or hide filters    ⌥⌘F" => "显示或隐藏筛选栏    ⌥⌘F",
        "Load window" => "载入此范围",
        "Clear Filters" => "清除筛选",
        "All rows are frozen." => "所有行均已冻结。",
        "Auto fit selected columns" => "自动调整所选列宽",
        "Sort ascending" => "升序排序",
        "Sort descending" => "降序排序",
        "Top N" => "前 N 行",
        "Sort & Top N" => "排序与前 N 行",
        "Sort & Top N…" => "排序与前 N 行…",
        "Ascending" => "升序",
        "Descending" => "降序",
        "NULL first" => "NULL 在前",
        "NULL last" => "NULL 在后",
        "Move up" => "上移",
        "Move down" => "下移",
        "Add sort field" => "添加排序字段",
        "All matching rows" => "所有匹配行",
        "Sort fields in priority order. Ties keep original row order." => {
            "按字段优先级排序。排序键完全相同时，保留原始行顺序。"
        }
        "Top N keeps the first matching rows after sorting. Leave it blank to show all matches." => {
            "保留排序后的前 N 条匹配行。留空可显示所有匹配行。"
        }
        "Top N needs a positive row count and at least one sort field." => {
            "请输入正整数行数，并选择至少一个排序字段。"
        }
        "Search for more fields." => "搜索以查找更多字段。",
        "Clear Sort & Top N" => "清除排序和行数限制",
        "Apply Sort" => "应用排序",
        "Saved view contains invalid sorting settings." => "命名视图包含无效的排序设置。",
        "Freeze selected rows" => "冻结所选行",
        "Freeze selected columns" => "冻结所选列",
        "Unfreeze selected rows" => "取消冻结所选行",
        "Unfreeze selected columns" => "取消冻结所选列",
        "Unfreeze all rows" => "取消冻结全部行",
        "Unfreeze all columns" => "取消冻结全部列",
        "Clear color marks" => "清除颜色标记",
        "Clear marking" => "清除标记",
        "Blue" => "蓝色",
        "Teal" => "青色",
        "Amber" => "琥珀色",
        "Filters" => "筛选",
        "Filter Conditions" => "筛选条件",
        "Advanced" => "高级",
        "Visual" => "可视化",
        "Match all" => "满足全部条件",
        "Match" => "匹配方式",
        "Match any" => "满足任一条件",
        "Not applied" => "尚未应用",
        "Add conditions…" => "添加条件…",
        "Add condition" => "添加条件",
        "Remove" => "移除",
        "Back" => "返回",
        "True" => "True",
        "False" => "False",
        "Search fields" => "搜索字段",
        "Select field" => "选择字段",
        "Choose a field for each condition before applying filters." => {
            "请为每个筛选条件选择字段后再应用。"
        }
        "Select operator" => "选择运算符",
        "No matching fields" => "没有匹配的字段",
        "No conditions yet" => "尚无筛选条件",
        "Add a condition to filter the loaded rows." => "添加条件，筛选已载入的记录。",
        "Filters apply to the loaded row window and selected fields." => {
            "筛选仅作用于已载入的行范围和所选字段。"
        }
        "Upper value" => "上限值",
        "Value" => "值",
        "and" => "且",
        "Equals" => "等于",
        "Does not equal" => "不等于",
        "Contains" => "包含",
        "Does not contain" => "不包含",
        "Starts with" => "开头为",
        "Ends with" => "结尾为",
        "Greater than" => "大于",
        "Greater than or equal" => "大于或等于",
        "Less than" => "小于",
        "Less than or equal" => "小于或等于",
        "Between (inclusive)" => "介于（含边界）",
        "Is NULL" => "为 NULL",
        "Is not NULL" => "不为 NULL",
        "Is empty" => "为空字符串",
        "Is not empty" => "不为空字符串",
        "Applying these conditions replaces the advanced filter. Cancel keeps it unchanged." => {
            "应用这些条件将替换高级筛选；取消则保留原筛选。"
        }
        "A field used by a filter cannot be hidden. Remove its condition first." => {
            "筛选条件正在使用此字段。请先移除对应条件，再隐藏字段。"
        }
        "Offset" => "起始偏移",
        "Rows" => "行数",
        "columns" => "列",
        "Recent Files" => "最近打开",
        "Remove from Recent Files" => "从最近打开列表中移除",
        "Read-only source" => "源文件只读",
        "Execute" => "执行",
        "Clear" => "清除",
        "Record Offset" => "记录偏移",
        "Record Count" => "记录数",
        "Offset must be a nonnegative integer; count must be a positive integer." => {
            "起始偏移必须为非负整数，记录数必须为正整数。"
        }
        "Offset must be a nonnegative integer" => "起始偏移必须为非负整数",
        "Row count must be a positive integer" => "行数必须为正整数",
        "Row count must be greater than zero" => "行数必须大于零",
        "Record count must be greater than zero." => "记录数必须大于零。",
        "No records match the current filter." => "没有记录符合当前筛选条件。",
        "Reading Parquet schema…" => "正在读取 Parquet Schema…",
        "Reading source schema…" => "正在读取源数据 Schema…",
        "Only Parquet files are supported." => "仅支持 Parquet 文件。",
        "Reading Parquet data" => "正在读取 Parquet 数据",
        "Reading data" => "正在读取数据",
        "Preparing table" => "正在准备表格",
        "Loading data into memory" => "正在载入内存",
        "Filtering data" => "正在筛选数据",
        "Sorting data" => "正在排序数据",
        "Processing the loaded records…" => "正在处理已载入的记录…",
        "Source changed after loading. Reload the file." => {
            "源文件在载入后发生了变化，请重新载入。"
        }
        "Source changed while reading metadata. Reload the file." => {
            "读取 Metadata 时源文件发生了变化，请重新载入。"
        }
        "Excel text cells support at most 32,767 UTF-16 units. Choose a format without Excel's cell-length limit." => {
            "Excel 文本单元格最多支持 32,767 个 UTF-16 单元，请选择不受此单元格长度限制的格式。"
        }
        "Finishing" => "即将完成",
        "Progress will appear when the data engine reports it." => "正在准备加载，进度稍后显示。",
        "Reloading file…" => "正在重新载入文件…",
        "Loading records…" => "正在载入记录…",
        "Exporting current results…" => "正在导出当前结果…",
        "Cancel  esc" => "取消  esc",
        "Cancelled" => "已取消",
        "Close  esc" => "关闭  esc",
        "Dismiss" => "关闭提示",
        "Metadata" => "Metadata",
        "Current Query" => "当前查询",
        "Files:" => "文件数：",
        "Rows:" => "行数：",
        "Size:" => "大小：",
        "Schema" => "Schema",
        "SQL Schema" => "SQL Schema",
        "SQL CREATE TABLE" => "SQL 建表语句",
        "Get SQL CREATE TABLE Script" => "生成 SQL 建表语句",
        "SQL Server schema for the selected fields" => "所选字段的 SQL Server Schema",
        "SQL schema copied" => "已复制 SQL Schema",
        "Cell Inspector" => "查看单元格",
        "Save Value…" => "保存值…",
        "Cell saved" => "已保存单元格",
        "Open Media…" => "打开媒体…",
        "Open ›" => "打开 ›",
        "‹ Back" => "‹ 返回",
        "Play" => "播放",
        "Pause" => "暂停",
        "Stop" => "停止",
        "Pause audio" => "暂停音频",
        "Play audio" => "播放音频",
        "Stop audio" => "停止音频",
        "+5 s" => "+5 秒",
        "−5 s" => "−5 秒",
        "-5 s" => "−5 秒",
        "Freeze Through This Column" => "冻结至此列",
        "Unfreeze All" => "取消全部冻结",
        "Move Column Left" => "向左移动列",
        "Move Column Right" => "向右移动列",
        "Word Wrap" => "自动换行",
        "Binary Display" => "Binary 显示格式",
        "Binary Display…" => "Binary 显示格式…",
        "Floating Point Display" => "Floating Point 显示格式",
        "Floating Point Display…" => "Floating Point 显示格式…",
        "Standard" => "标准",
        "Scientific" => "科学计数法",
        "Decimal" => "Decimal",
        "Hex" => "Hex",
        "Base64" => "Base64",
        "ASCII" => "ASCII",
        "Size" => "大小",
        "GUID" => "GUID",
        "IPv4" => "IPv4",
        "IPv6" => "IPv6",
        "Short (16-bit)" => "Short (16-bit)",
        "Integer (32-bit)" => "Integer (32-bit)",
        "Long (64-bit)" => "Long (64-bit)",
        "Float (32-bit)" => "Float (32-bit)",
        "Double (64-bit)" => "Double (64-bit)",
        "Source files changed. Reload to see the latest data (⌘R)." => {
            "源文件已更改，请重新载入以查看最新数据（⌘R）。"
        }
        "Choose a different path to avoid overwriting an open source file." => {
            "请选择其他路径，以免覆盖已打开的源文件。"
        }
        "Choose a different path to avoid overwriting the open source file." => {
            "请选择其他路径，以免覆盖已打开的源文件。"
        }
        "About Parquet Viewer" => "关于 Parquet Viewer",
        "Parquet Viewer" => "Parquet Viewer",
        "Parquet Viewer for macOS" => "Parquet Viewer for macOS",
        "Version {version} · Rust + GPUI + Arrow + DataFusion" => {
            "版本 {version} · Rust + GPUI + Arrow + DataFusion"
        }
        "GPL-3.0-or-later. GPUI components: Apache-2.0." => {
            "GPL-3.0-or-later；GPUI 组件采用 Apache-2.0 许可。"
        }
        "Query & Keyboard Help" => "查询与键盘帮助",
        "Query Syntax & Shortcuts" => "查询语法与快捷键",
        "Apply" => "应用",
        "Import Delimited Text" => "导入分隔文本",
        "Import Delimited Text…" => "导入分隔文本…",
        "Import Text…" => "导入文本…",
        "Import Settings…" => "导入设置…",
        "The row count is unknown. Choose a loading range instead." => {
            "总行数尚未知，请先指定载入范围。"
        }
        "The full text-file row count is unknown. Set Record Count to the number of rows you want to load; the application will read that window without first counting the entire file." => {
            "文本文件的总行数尚未知。请指定要载入的记录数；应用会读取对应范围，不会先扫描整个文件计数。"
        }
        "Enter one delimiter character, or \\\\t for a tab." => {
            "请输入一个分隔符；制表符可写为 \\t。"
        }
        "Enter exactly one delimiter character, or \\\\t for a tab." => {
            "只能输入一个分隔符；制表符可写为 \\t。"
        }
        "Analyzing the first 1000 text records…" => "正在分析前 1000 条文本记录…",
        "Checking import settings against the first 1000 records…" => {
            "正在用前 1000 条记录检查导入设置…"
        }
        "Source data stays read-only. Analyze settings, then load only the requested row window." => {
            "源文件保持只读。请检查导入设置，再载入指定的行范围。"
        }
        "Delimiter" => "分隔符",
        "Custom separator" => "自定义分隔符",
        "Import settings" => "导入设置",
        "{separator} delimiter" => "{separator}分隔",
        "Space" => "空格",
        "Colon" => "冒号",
        "Caret" => "脱字符",
        "Inference rows" => "类型推断行数",
        "Header row" => "首行为表头",
        "Automatic types" => "自动推断类型",
        "Custom field types" => "自选字段类型",
        "Sample rows" => "推断行数",
        "Enter one printable ASCII character other than a double quote, or \\t for tab." => {
            "请输入一个可打印的 ASCII 字符，双引号除外。制表符可输入 \\t。"
        }
        "Text File Import" => "文本文件导入",
        "Import Text" => "导入文本",
        "Cancel" => "取消",
        "Scanning text records" => "正在扫描文本记录",
        "Total rows will be available after the text row index reaches the end of the file." => {
            "文本行索引扫描到文件末尾后会显示总行数。"
        }
        "Reopen the text file with the saved import settings before applying this view." => {
            "请使用保存视图时的导入设置重新打开文本文件，再应用该视图。"
        }
        "Text files must be opened individually." => "每次只能打开一个文本文件。",
        "Choose a Parquet or delimited text file." => "请选择 Parquet 文件或分隔文本文件。",
        "Choose a single text file." => "请选择单个文本文件。",
        "An open-ended row range requires a delimited text file." => {
            "只可对分隔文本文件指定无上限的载入范围。"
        }
        "Unsupported field separator" => "此分隔符不受支持",
        "Text file headers must contain valid UTF-8" => "文本文件表头必须是有效的 UTF-8",
        "Text file field names cannot contain NUL" => "文本文件的字段名不能包含 NUL",
        "The inference sample must be between 1 and 100,000 records." => {
            "用于推断的记录数必须介于 1 和 100,000 之间。"
        }
        "Could not read the first text record" => "无法读取第一条文本记录",
        "The text file contains no fields." => "文本文件中没有字段。",
        "A type override refers to a missing field." => "手动指定 Data type 的字段不存在。",
        "Could not read the text file sample" => "无法读取文本文件样本",
        "Could not index the text file records" => "无法为文本文件的记录建立索引",
        "A text record exceeds the 64 MiB limit." => "一条文本记录超过 64 MiB 上限。",
        "Text file row count exceeds addressable memory" => "文本文件行数超过可寻址范围",
        "Missing text file block" => "缺少文本文件数据块",
        "Incomplete text file block" => "文本文件数据块不完整",
        "Invalid text file block offsets" => "文本文件数据块的偏移量无效",
        "Text record block is outside the scanned buffer" => "文本记录块超出已扫描的缓冲区",
        "The text decoder returned an incomplete row range." => {
            "文本解码器返回的行数少于请求范围。"
        }
        "Invalid text file decimal buffers" => "文本文件中的 Decimal 缓冲区无效",
        "Comma" => "逗号",
        "Semicolon" => "分号",
        "Tab" => "制表符",
        "Pipe" => "竖线",
        "First record is a header" => "首条记录为表头",
        "All text" => "全部按文本读取",
        "Set as Default" => "设为默认值",
        "Data preview" => "数据预览",
        "Sample size · {count}" => "已取样 {count} 条",
        "Click a type below a column name to change it." => "点击字段名下方的类型即可修改。",
        "Preparing text preview…" => "正在准备文本预览…",
        "Records used for inference: {count}" => "用于推断的记录数 {count}",
        "Change column type" => "更改字段类型",
        "Choose a type for {column}" => "请选择「{column}」的字段类型",
        "Apply changes to update the preview." => "应用更改以刷新预览。",
        "No header" => "无表头",
        "Refresh Preview" => "刷新预览",
        "Missing tokens (comma separated)" => "缺失值标记（以逗号分隔）",
        "Empty fields are NULL" => "空字段视为 NULL",
        "Keep empty strings" => "保留空字符串",
        "Trim for NULL matching" => "匹配 NULL 时忽略首尾空白",
        "Exact whitespace" => "精确匹配空白",
        "Ignore NULL token case" => "匹配 NULL 标记时忽略大小写",
        "Case-sensitive NULL" => "匹配 NULL 标记时区分大小写",
        "Apply Field" => "应用字段设置",
        "Preview — first 5 records (NULL differs from an empty string)" => {
            "预览前 5 条记录（NULL 与空字符串不同）"
        }
        "Record offset" => "记录偏移",
        "Rows to load" => "载入行数",
        "Import Window" => "载入此范围",
        "Later values may differ from the sample. Conversion errors keep the old view; choose VARCHAR to preserve mixed values. No full-file scan is used to count rows." => {
            "后续数据可能与样本不同。类型转换失败时会保留原视图；混合类型可选用 VARCHAR。不会为计数而扫描整个文件。"
        }
        "Empty collection — 0 rows" => "空集合，共 0 行",
        "JSON" => "JSON",
        "Choose an available field." => "请选择可用字段。",
        "The field name is ambiguous." => "字段名称存在歧义。",
        "A field name cannot contain NUL." => "字段名称不能包含 NUL。",
        "Invalid upper bound." => "上限值无效。",
        "The value cannot contain NUL." => "值不能包含 NUL。",
        "Enter true or false for a Boolean value." => "Boolean 值请输入 true 或 false。",
        "Only NULL checks are supported for this field type." => "此字段类型仅支持 NULL 检查。",
        "Enter a whole number in range." => "请输入有效范围内的整数。",
        "Enter a nonnegative whole number in range." => "请输入有效范围内的非负整数。",
        "Enter a finite number within the FLOAT range." => "请输入 FLOAT 范围内的有限数值。",
        "Enter a finite number within the DOUBLE range." => "请输入 DOUBLE 范围内的有限数值。",
        "Enter a decimal number using digits and a dot (no exponent)." => {
            "请输入由数字和小数点组成的十进制数，不使用指数形式。"
        }
        "Enter a valid ISO date, such as 2025-01-31." => "请输入有效的 ISO 日期，例如 2025-01-31。",
        "Enter a valid time, such as 14:30:00." => "请输入有效时间，例如 14:30:00。",
        "TIME supports at most 6 fractional digits." => "TIME 最多支持 6 位小数秒。",
        "Enter an ISO date/time without a time zone, such as 2025-01-31 14:30:00." => {
            "请输入不含时区的 ISO 日期时间，例如 2025-01-31 14:30:00。"
        }
        "Enter an ISO date/time with an explicit UTC offset, such as 2025-01-31T14:30:00+00:00." => {
            "请输入包含 UTC 偏移的 ISO 日期时间，例如 2025-01-31T14:30:00+00:00。"
        }
        "TIMESTAMPTZ supports at most 6 fractional digits." => "TIMESTAMPTZ 最多支持 6 位小数秒。",
        "Enter a time with an explicit UTC offset, such as 14:30:00+00:00." => {
            "请输入包含 UTC 偏移的时间，例如 14:30:00+00:00。"
        }
        "TIMETZ supports at most 6 fractional digits." => "TIMETZ 最多支持 6 位小数秒。",
        "Invalid time" => "时间无效",
        "Invalid date/time" => "日期时间无效",
        "Invalid date/time or UTC offset" => "日期时间或 UTC 偏移无效",
        "The source window is unavailable." => "源数据窗口不可用。",
        "The source file is no longer open." => "源文件已关闭。",
        "The source file or query has changed; this preview belongs to an older snapshot." => {
            "源文件或查询已更改，此预览对应较早的数据快照。"
        }
        "The source column is no longer displayed." => "源列已不在当前显示结果中。",
        "The source row is no longer displayed." => "源行已不在当前显示结果中。",
        "The source window has closed." => "源数据窗口已关闭。",
        "The text source changed. Reload the file before paging." => {
            "文本源文件已更改，请重新载入后再翻页。"
        }
        "Operation cancelled." => "操作已取消。",
        "Could not load data or apply the filter" => "无法载入数据或应用筛选",
        "Could not initialize data engine" => "无法初始化数据引擎",
        "Could not bind the delimited source with the selected settings" => {
            "无法按当前设置读取分隔文本"
        }
        "Could not create the private text paging index" => "无法创建文本分页索引",
        "Load a file before exporting." => "请先载入文件，再导出。",
        "Result snapshot is unavailable." => "结果快照不可用。",
        "Source data is read-only. Choose a different destination; source files and their aliases cannot be overwritten." => {
            "源数据只读。请选择其他保存位置，不能覆盖源文件或其别名。"
        }
        "Source data is read-only. Choose a different destination; source files and their aliases cannot be overwritten" => {
            "源数据只读。请选择其他保存位置，不能覆盖源文件或其别名"
        }
        "The requested window exceeds the 128 MiB display budget. Reduce the record count or choose fewer fields." => {
            "请求的数据范围超过 128 MiB 显示预算，请减少记录数或选择更少字段。"
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::static_text;

    #[test]
    fn loaded_navigation_and_filter_controls_have_explicit_chinese_semantics() {
        assert_eq!(
            static_text("Jump to first loaded row (current result only)"),
            Some("跳到最前（仅当前已载入结果）")
        );
        assert_eq!(
            static_text("Jump to last loaded row (current result only)"),
            Some("跳到最后（仅当前已载入结果）")
        );
        assert_eq!(static_text("Is NULL"), Some("为 NULL"));
        assert_eq!(static_text("Is empty"), Some("为空字符串"));
        assert_eq!(static_text("Theme: Follow System"), Some("主题：跟随系统"));
    }

    #[test]
    fn parquet_loading_progress_and_format_errors_have_chinese_labels() {
        for (label, chinese) in [
            ("Only Parquet files are supported.", "仅支持 Parquet 文件。"),
            ("Reading Parquet data", "正在读取 Parquet 数据"),
            ("Preparing table", "正在准备表格"),
            ("Loading data into memory", "正在载入内存"),
            ("Finishing", "即将完成"),
            (
                "Progress will appear when the data engine reports it.",
                "正在准备加载，进度稍后显示。",
            ),
        ] {
            assert_eq!(static_text(label), Some(chinese));
        }
    }

    #[test]
    fn delimited_index_labels_keep_their_substitution_fields() {
        for (label, chinese) in [
            ("1 row available", "可浏览 1 行"),
            ("{count} rows available", "可浏览 {count} 行"),
            ("Indexing {percent}%", "正在建立索引 {percent}%"),
            ("Indexed rows: {rows}", "已建立索引的行数：{rows}"),
            ("Scanned {bytes} of {total}", "已扫描 {bytes}，共 {total}"),
            ("Indexing stopped", "索引已停止"),
        ] {
            assert_eq!(static_text(label), Some(chinese));
        }
    }

    #[test]
    fn professional_type_names_remain_english_in_chinese_chrome() {
        for term in [
            "Number",
            "Text",
            "Category",
            "Boolean",
            "Date",
            "Date & time",
            "Time",
            "True",
            "False",
            "Data type",
            "Schema",
            "Metadata",
            "Decimal",
            "Float (32-bit)",
            "Double (64-bit)",
            "nunique",
            "value_counts",
            "Density plot",
        ] {
            assert_eq!(static_text(term), Some(term), "{term}");
        }
        assert_eq!(static_text("Filters"), Some("筛选"));
        assert_eq!(static_text("Is NULL"), Some("为 NULL"));
        assert_eq!(static_text("Descriptive Statistics"), Some("描述性统计"));
    }

    #[test]
    fn unknown_source_text_is_not_translated_as_a_label() {
        for text in [
            "customer_id",
            "/Users/example/Fields.parquet",
            "SELECT * FROM data",
            "Shanghai",
            "NULL",
            "2026-10-31",
        ] {
            assert_eq!(static_text(text), None);
        }
    }
}
