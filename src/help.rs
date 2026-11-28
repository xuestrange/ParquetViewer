//! The in-app guide follows the order of a typical viewing session.

use gpui::{Div, FontWeight, Stateful, div, prelude::*, px, rgb};

use crate::{
    i18n::Language,
    icons::{self, Icon},
    theme::Colors,
};

type Localized = (&'static str, &'static str);

struct Topic {
    icon: Icon,
    title: Localized,
    body: Localized,
}

const TOPICS: &[Topic] = &[
    Topic {
        icon: Icon::Open,
        title: ("Load and navigate", "载入与定位"),
        body: (
            "Use File in the top toolbar to open or reload data, load a range or all records, or create a window. Use Import Text File for text with any extension. Set the separator, header, inference sample size, and column types in the preview. Choose Reimport (⌘R) to revisit those settings. First and Last jump within the loaded result.",
            "从顶栏的「文件」菜单打开或重新载入数据、载入范围或全部记录，也可以新建窗口。「导入文本文件」支持任意扩展名的分隔文本。预览中可设置分隔符、表头、类型推断行数和字段类型，选择「重新导入」（⌘R）可再次调整。「最前」和「最后」可跳到已载入结果的首尾。",
        ),
    },
    Topic {
        icon: Icon::Filter,
        title: ("Filter records", "筛选记录"),
        body: (
            "Open Filters (⌥⌘F) and choose Loaded range or Entire file. Click Add condition and select a text field. Choose Contains, enter the text, and click Apply. The table shows matching rows with their original row numbers.",
            "打开「筛选」（⌥⌘F），选择「载入范围」或「整个文件」。点击「添加条件」，选择文本字段和「包含」，输入文本后点击「应用」。表格会显示匹配的记录，并保留原始行号。",
        ),
    },
    Topic {
        icon: Icon::Table,
        title: ("Work with the table", "浏览表格"),
        body: (
            "Use Fields to choose which columns to show. Drag row numbers or column headers to select a range. Use the context menus to mark, freeze, sort, or wrap data. Double-click nested values or embedded images to preview them in the sidebar.",
            "在“字段”中选择要显示的列。拖动行号或列标题选择范围。右键菜单提供标记、冻结、排序和自动换行。双击嵌套值或内嵌图片，可在侧栏预览。",
        ),
    },
    Topic {
        icon: Icon::Statistics,
        title: ("Analyze data", "统计分析"),
        body: (
            "Statistical Analysis opens on Descriptive Statistics. Select Aggregation in the sidebar header to group and summarize data. Drag the sidebar edge to adjust its width. In Aggregation, drag fields or measures to reorder them, and choose several calculations or date parts on one row.",
            "打开「统计分析」会先显示「描述性统计」。点击侧栏顶部的「分类汇总」可分组汇总数据。拖动侧栏边缘可调整宽度。分类字段和汇总项可拖动排序，同一行可选多个计算方式或日期维度。",
        ),
    },
    Topic {
        icon: Icon::Export,
        title: ("Save and export", "保存与导出"),
        body: (
            "Press ⇧⌘V to save or restore a source's loading range, fields, and filters. Press ⌘S to export the displayed rows, or export a summary from Aggregation. Source files are read-only.",
            "按 ⇧⌘V 保存或恢复同一来源的载入范围、字段和筛选条件。按 ⌘S 导出当前显示行，也可从「分类汇总」导出汇总结果。源文件始终只读。",
        ),
    },
];

const SHORTCUTS: &[(&str, Localized)] = &[
    ("⌘O", ("Open file", "打开文件")),
    ("⌘N", ("New window", "新建窗口")),
    ("⌘R", ("Reload", "重新载入")),
    ("⌘F", ("Choose fields", "选择字段")),
    ("⌥⌘F", ("Open filters", "打开筛选")),
    ("⌥⌘G", ("Open statistical analysis", "打开统计分析")),
    ("⌘C", ("Copy", "复制")),
    ("⇧⌘C", ("Copy with headers", "连同表头复制")),
    ("⇧⌘V", ("Saved views", "已保存视图")),
    ("⌘S", ("Export", "导出")),
    ("Enter", ("Inspect cell", "查看单元格")),
    ("Esc", ("Close panel", "关闭面板")),
];

fn local(text: Localized, language: Language) -> &'static str {
    match language {
        Language::English => text.0,
        Language::SimplifiedChinese => text.1,
    }
}

fn card(colors: Colors) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .gap_2()
        .p_3()
        .rounded(px(6.))
        .border_1()
        .border_color(rgb(colors.border))
        .bg(rgb(colors.surface))
}

pub(crate) fn render(language: Language, dark: bool) -> Stateful<Div> {
    let colors = Colors::new(dark);
    let guide = local(("Quick guide", "快速上手"), language);
    let guide_detail = local(
        (
            "Open a text file to browse its first rows while the footer tracks indexing. File → Go to Row… jumps to a source row as soon as it is indexed. When a Parquet dataset’s file size reaches this Mac’s installed RAM minus 4 GiB, you can choose a row range or load all rows.",
            "打开文本文件即可浏览开头的记录，底部状态栏会显示索引进度。「文件 → 跳转到行…」可在目标行建立索引后跳转到该行。Parquet 数据集的文件大小达到本机物理内存总量减去 4 GiB 时，可选择载入的行范围或全部记录。",
        ),
        language,
    );
    let shortcuts = local(("Keyboard shortcuts", "键盘快捷键"), language);

    div()
        .id("help-content")
        .flex_1()
        .min_h_0()
        .min_w_0()
        .overflow_y_scroll()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .pr_1()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_3()
                        .rounded(px(6.))
                        .bg(rgb(colors.accent_soft))
                        .child(
                            div()
                                .text_color(rgb(colors.accent))
                                .text_size(px(16.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(guide),
                        )
                        .child(
                            div()
                                .whitespace_normal()
                                .line_height(px(19.))
                                .child(guide_detail),
                        ),
                )
                .children(TOPICS.iter().map(|topic| {
                    card(colors)
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(icons::glyph(topic.icon, 15., dark))
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(local(topic.title, language)),
                                ),
                        )
                        .child(
                            div()
                                .whitespace_normal()
                                .line_height(px(19.))
                                .text_color(rgb(colors.muted))
                                .child(local(topic.body, language)),
                        )
                }))
                .child(
                    card(colors)
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(shortcuts),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_2()
                                .children(SHORTCUTS.iter().map(|(key, label)| {
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .child(
                                            div()
                                                .px_1()
                                                .rounded(px(3.))
                                                .border_1()
                                                .border_color(rgb(colors.border))
                                                .bg(rgb(colors.bar))
                                                .font_weight(FontWeight::MEDIUM)
                                                .child(*key),
                                        )
                                        .child(
                                            div()
                                                .text_color(rgb(colors.muted))
                                                .child(local(*label, language)),
                                        )
                                })),
                        ),
                ),
        )
}
