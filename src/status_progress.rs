//! Progress for foreground operations and background indexing in the window footer.
use super::*;
use crate::engine::QueryProgressSnapshot;

struct OperationProgress {
    phase: String,
    fraction: Option<f64>,
    detail: String,
}

impl OperationProgress {
    fn new(message: &str, snapshot: Option<QueryProgressSnapshot>) -> Self {
        let Some(progress) = snapshot else {
            let phase = t(message);
            return Self {
                detail: phase.clone(),
                phase,
                fraction: None,
            };
        };
        // Scanning the last row can precede merging groups and producing the
        // final aggregate. That remaining work has no measurable fraction.
        let finishing_summary = progress.phase == QueryPhase::FinishingAggregation
            || (progress.phase == QueryPhase::Aggregating
                && progress.fraction.is_some_and(|fraction| fraction >= 1.));
        let phase = t(match progress.phase {
            QueryPhase::Loading
                if progress.processed_rows == 0 && progress.total_rows.is_none() =>
            {
                message
            }
            QueryPhase::Loading => "Reading data",
            QueryPhase::Indexing => "Scanning text records",
            QueryPhase::Preparing => "Loading data into memory",
            QueryPhase::Filtering => "Filtering data",
            QueryPhase::Sorting => "Sorting data",
            QueryPhase::Aggregating if finishing_summary => "Finishing summary…",
            QueryPhase::Aggregating => "Aggregating…",
            QueryPhase::FinishingAggregation => "Finishing summary…",
            QueryPhase::Charting => "Preparing charts…",
            QueryPhase::Complete => "Finishing",
        });
        let fraction = progress
            .fraction
            .filter(|value| value.is_finite() && !finishing_summary)
            .map(|value| value.clamp(0., 1.));
        let mut detail = phase.clone();
        let rows = match progress.total_rows {
            Some(total) => {
                Some(t("Rows processed: {rows} / {total}").replace("{total}", &count_label(total)))
            }
            None if progress.processed_rows > 0 => Some(t("Rows processed: {rows}")),
            None => None,
        };
        if let Some(rows) = rows {
            detail.push('\n');
            detail.push_str(&rows.replace("{rows}", &count_label(progress.processed_rows)));
        }
        Self {
            phase,
            fraction,
            detail,
        }
    }
}

pub(super) fn meter(c: Colors, fill: gpui::Div) -> gpui::Div {
    div()
        .relative()
        .h_full()
        .min_w_0()
        .flex()
        .items_center()
        .gap(px(5.))
        .pb(px(2.))
        .text_size(px(11.))
        .font_weight(gpui::FontWeight::MEDIUM)
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(px(2.))
                .h(px(2.))
                .rounded(px(1.))
                .overflow_hidden()
                .bg(rgb(c.border))
                .child(fill.h_full().bg(rgb(c.accent))),
        )
}

impl Viewer {
    pub(super) fn render_status_progress(
        &self,
        message: &str,
        c: Colors,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let state = OperationProgress::new(
            message,
            self.query_progress.as_ref().map(QueryProgress::snapshot),
        );
        let fill = match state.fraction {
            Some(value) => div().w(gpui::relative(value as f32)),
            None => div()
                .absolute()
                .left(gpui::relative(
                    f32::from(self.status_progress_tick) / 9. * 0.7,
                ))
                .w(gpui::relative(0.3)),
        };
        div()
            .h_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(if compact { 4. } else { 7. }))
            .px(px(if compact { 5. } else { 8. }))
            .bg(rgb(c.accent_soft))
            .text_color(rgb(c.accent))
            .child(
                meter(c, fill)
                    .id("status-progress")
                    .tooltip(icons::tooltip(state.detail, self.preferences.dark))
                    .child(
                        div()
                            .min_w_0()
                            .max_w(px(if compact { 92. } else { 172. }))
                            .truncate()
                            .child(state.phase),
                    )
                    .when_some(state.fraction, |el, value| {
                        el.child(
                            div()
                                .flex_shrink_0()
                                .child(format!("{:.0}%", (value * 100.).floor())),
                        )
                    }),
            )
            .child(
                self.button(
                    "cancel",
                    "Cancel",
                    true,
                    |this, _, cx| {
                        if this.busy.is_some() {
                            this.cancel(cx);
                        } else {
                            this.cancel_column_profile(cx);
                        }
                    },
                    cx,
                )
                .h(px(20.))
                .px(px(5.))
                .text_size(px(11.)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_decoded_rows_with_an_unknown_total() {
        let state = OperationProgress::new(
            "Reading data",
            Some(QueryProgressSnapshot {
                phase: QueryPhase::Loading,
                fraction: None,
                processed_rows: 12_345,
                total_rows: None,
            }),
        );
        assert_eq!(state.fraction, None);
        assert_eq!(
            state.detail,
            format!(
                "{}\n{}",
                t("Reading data"),
                t("Rows processed: {rows}").replace("{rows}", "12,345")
            )
        );
    }

    #[test]
    fn finishing_an_aggregate_after_scanning_all_rows_is_indeterminate() {
        let state = OperationProgress::new(
            "Aggregating…",
            Some(QueryProgressSnapshot {
                phase: QueryPhase::Aggregating,
                fraction: Some(1.),
                processed_rows: 20,
                total_rows: Some(20),
            }),
        );
        assert_eq!(state.phase, t("Finishing summary…"));
        assert_eq!(state.fraction, None);
        assert!(
            state.detail.contains(
                &t("Rows processed: {rows} / {total}")
                    .replace("{rows}", "20")
                    .replace("{total}", "20")
            )
        );
    }
}
