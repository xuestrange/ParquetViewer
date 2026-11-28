//! The window footer describes the table snapshot currently on screen.
use super::*;
use crate::engine::csv_source::CsvIndexProgress;

#[path = "status_progress.rs"]
mod progress;
#[path = "status_rows.rs"]
mod rows;

fn block(id: &'static str, foreground: u32, background: u32) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(20.))
        .min_w_0()
        .px(px(7.))
        .flex()
        .items_center()
        .gap(px(5.))
        .rounded(px(3.))
        .whitespace_nowrap()
        .bg(rgb(background))
        .text_color(rgb(foreground))
        .font_weight(gpui::FontWeight::MEDIUM)
}

fn query_time(ms: u128) -> String {
    let duration = match ms {
        0 => "<1 ms".to_owned(),
        1..1000 => format!("{ms} ms"),
        _ => format!("{:.1} s", ms as f64 / 1000.),
    };
    t("Query {time}").replace("{time}", &duration)
}

fn browse_scope(available: usize, index: &CsvIndexProgress) -> (String, String) {
    // The worker may publish new offsets just before the UI grows the list.
    let label = match (index.complete && index.indexed_rows == available, available) {
        (true, 1) => t("1 row"),
        (true, n) => t("{count} rows").replace("{count}", &count_label(n)),
        (false, 1) => t("1 row available"),
        (false, n) => t("{count} rows available").replace("{count}", &count_label(n)),
    };
    (label, t("Scroll to read indexed rows from the text file."))
}

fn index_indicator(index: &CsvIndexProgress, c: Colors, dark: bool) -> gpui::Stateful<gpui::Div> {
    let detail = t("Scanned {bytes} of {total}")
        .replace(
            "{bytes}",
            &crate::process_memory::format_bytes(Some(index.indexed_bytes)),
        )
        .replace(
            "{total}",
            &crate::process_memory::format_bytes(Some(index.total_bytes)),
        );
    let rows = t("Indexed rows: {rows}").replace("{rows}", &count_label(index.indexed_rows));
    let tooltip = format!("{rows}\n{detail}");
    if let Some(error) = &index.error {
        return block("csv-row-index", c.danger, c.danger_bg)
            .flex_shrink_0()
            .child(t("Indexing stopped"))
            .tooltip(icons::tooltip(format!("{}\n{tooltip}", t(error)), dark));
    }

    // The last byte can be consumed before the reader verifies EOF.
    let percent = (u128::from(index.indexed_bytes) * 100)
        .checked_div(u128::from(index.total_bytes))
        .unwrap_or(0)
        .min(99);
    let label = t("Indexing {percent}%").replace("{percent}", &percent.to_string());
    let fill = div().w(gpui::relative(percent as f32 / 100.));
    block("csv-row-index", c.accent, c.accent_soft)
        .flex_shrink_0()
        .child(progress::meter(c, fill).child(label))
        .tooltip(icons::tooltip(tooltip, dark))
}

impl Viewer {
    pub(super) fn render_status_bar(&self, window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        let dark = self.preferences.dark;
        let c = Colors::new(dark);
        let compact = window.viewport_size().width < px(1100.);
        let activity = self.status_activity();
        let index = self.csv_index.as_ref().map(|source| source.progress());
        let notice = self
            .pending_csv_jump_label()
            .or_else(|| (activity.is_none() && !self.notice.is_empty()).then(|| t(&self.notice)));
        let parent_rows = self
            .aggregation_base
            .as_ref()
            .map(|detail| detail.base.row_count());
        let timing = if let Some((_, start)) = activity {
            let elapsed = format!("{:.1} s", start.elapsed().as_secs_f64());
            Some((
                t("Elapsed {time}").replace("{time}", &elapsed),
                "Elapsed time for the current operation.",
            ))
        } else {
            self.result
                .as_ref()
                .filter(|_| parent_rows.is_none() && self.csv_browse.is_none())
                .map(|result| {
                    (
                        query_time(result.elapsed_ms),
                        "Elapsed time for the displayed query.",
                    )
                })
        };
        let memory = cx.global::<MainWindowRegistry>().process_memory_bytes;

        div()
            .h(px(28.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(5.))
            .px(px(8.))
            .bg(rgb(c.bar))
            .border_t_1()
            .border_color(rgb(c.border))
            .text_size(px(11.))
            .text_color(rgb(c.muted))
            .when_some(self.paths.first(), |bar, path| {
                let name = path
                    .file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy();
                let files = self
                    .info
                    .as_ref()
                    .map_or(self.paths.len(), |info| info.file_count);
                let label = if files > 1 {
                    t("{count} files").replace("{count}", &count_label(files))
                } else {
                    name.into_owned()
                };
                let mut detail = self
                    .paths
                    .iter()
                    .take(3)
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>();
                if self.paths.len() > detail.len() {
                    detail.push("…".to_owned());
                }
                if self.modified {
                    detail.push(t(
                        "Source files changed. Reload to see the latest data (⌘R).",
                    ));
                }
                let (fg, bg) = if self.modified {
                    (c.warning, c.warning_bg)
                } else {
                    (c.accent, c.accent_soft)
                };
                bar.child(
                    block("dataset-file-name", fg, bg)
                        .max_w(px(if compact { 160. } else { 230. }))
                        .child(icons::glyph(
                            if files > 1 { Icon::Folder } else { Icon::Table },
                            13.,
                            dark,
                        ))
                        .when(self.modified, |el| el.child("●"))
                        .child(div().min_w_0().truncate().child(label))
                        .tooltip(icons::tooltip(detail.join("\n"), dark)),
                )
            })
            .when_some(self.result.as_ref(), |bar, result| {
                let (scope, detail) = if let Some(index) =
                    index.as_ref().filter(|_| self.csv_browse.is_some())
                {
                    browse_scope(self.table_row_count(), index)
                } else {
                    rows::row_summary(result, &self.displayed_request, parent_rows, index.as_ref())
                };
                let fields = match self.view_columns.len() {
                    1 => t("1 field"),
                    n => t("{count} fields").replace("{count}", &count_label(n)),
                };
                let summary = format!("{scope} · {fields}");
                let mut tooltip = summary.clone();
                if !detail.is_empty() && detail != scope {
                    tooltip.push('\n');
                    tooltip.push_str(&detail);
                }
                bar.child(
                    block("load-scope", c.text, c.header)
                        .max_w(px(if compact { 280. } else { 380. }))
                        .child(div().min_w_0().truncate().child(summary))
                        .tooltip(icons::tooltip(tooltip, dark)),
                )
            })
            .when(self.info.is_some() && self.result.is_none(), |bar| {
                bar.child(block("unloaded-scope", c.text, c.header).child(t("No rows loaded")))
            })
            .when(parent_rows.is_some() && self.busy.is_none(), |bar| {
                bar.child(
                    self.button(
                        "back-to-details-status",
                        "Back to all details",
                        true,
                        |this, _, cx| this.restore_aggregation_details(cx),
                        cx,
                    )
                    .h(px(20.))
                    .text_size(px(11.))
                    .flex_shrink_0(),
                )
            })
            .child(div().min_w_0().flex_1().when_some(notice, |space, notice| {
                space.child(
                    div()
                        .id("status-notice")
                        .pl(px(4.))
                        .truncate()
                        .tooltip(icons::tooltip(notice.clone(), dark))
                        .child(notice),
                )
            }))
            .when_some(
                index.as_ref().filter(|i| !i.complete || i.error.is_some()),
                |bar, index| bar.child(index_indicator(index, c, dark)),
            )
            .when_some(activity, |bar, (message, _)| {
                bar.child(
                    self.render_status_progress(message, c, compact, cx)
                        .flex_shrink_0(),
                )
            })
            .when_some(timing, |bar, (label, tooltip)| {
                bar.child(
                    block("query-time", c.success, c.success_bg)
                        .flex_shrink_0()
                        .when(activity.is_some(), |el| el.min_w(px(84.)).justify_center())
                        .child(label)
                        .tooltip(icons::tooltip(t(tooltip), dark)),
                )
            })
            .child(
                block("process-memory", c.blue_text, c.memory_bg)
                    .flex_shrink_0()
                    .child(format!(
                        "{} {}",
                        t("Memory"),
                        crate::process_memory::format_bytes(memory)
                    ))
                    .tooltip(icons::tooltip(
                        t("Current process memory footprint; refreshed every 2 seconds."),
                        dark,
                    )),
            )
    }
}
