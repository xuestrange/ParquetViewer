//! The window footer describes the table snapshot currently on screen.
use super::*;

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

impl Viewer {
    pub(super) fn render_status_bar(&self, window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        let dark = self.preferences.dark;
        let c = Colors::new(dark);
        let compact = window.viewport_size().width < px(1100.);
        let activity = self.status_activity();
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
                .filter(|_| parent_rows.is_none())
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
                let (scope, detail) =
                    rows::row_summary(result, &self.displayed_request, parent_rows);
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
            .child(div().min_w_0().flex_1().when(
                activity.is_none() && !self.notice.is_empty(),
                |space| {
                    let notice = t(&self.notice);
                    space.child(
                        div()
                            .id("status-notice")
                            .pl(px(4.))
                            .truncate()
                            .tooltip(icons::tooltip(notice.clone(), dark))
                            .child(notice),
                    )
                },
            ))
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
