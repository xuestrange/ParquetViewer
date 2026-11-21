//! A snapshot-bound profile sidebar.
use super::*;
use crate::profile_stats::{
    Density, ProfileDetails, ProfileMode, ProfileStats, calculate_profile, profile_has_text_mode,
    profile_modes,
};
use gpui::{PathBuilder, ScrollStrategy, uniform_list};
use std::rc::Rc;

const FIELD_OPTION_HEIGHT: f32 = 28.;
const VISIBLE_FIELD_ROWS: usize = 8;
const PROFILE_SCROLLBAR_ID: usize = 4;

pub(super) struct ColumnProfile {
    column: usize,
    info: ColumnInfo,
    mode: ProfileMode,
    cancel: Option<Arc<AtomicBool>>,
    outcome: Option<anyhow::Result<ProfilePresentation>>,
}

impl ColumnProfile {
    fn cancel_pending(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl Viewer {
    pub(super) fn column_profile_running(&self) -> bool {
        self.statistics_tab == Some(StatisticsTab::Descriptive)
            && self
                .column_profile
                .as_ref()
                .is_some_and(|profile| profile.cancel.is_some())
    }

    pub(super) fn cancel_column_profile(&mut self, cx: &mut Context<Self>) {
        self.close_column_profile();
        self.notice = "Cancelled".into();
        cx.notify();
    }

    pub(super) fn close_column_profile(&mut self) {
        if let Some(mut profile) = self.column_profile.take() {
            profile.cancel_pending();
        }
    }

    pub(super) fn pause_column_profile(&mut self) {
        if let Some(profile) = &mut self.column_profile {
            profile.cancel_pending();
        }
    }

    pub(super) fn resume_column_profile(&mut self, cx: &mut Context<Self>) {
        if let Some(profile) = &self.column_profile
            && profile.cancel.is_none()
            && profile.outcome.is_none()
        {
            self.start_column_profile(profile.column, profile.mode, cx);
        }
    }

    pub(super) fn refresh_column_profile(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = &self.column_profile else {
            return;
        };
        let Some(result) = self
            .result
            .as_ref()
            .filter(|result| !result.columns.is_empty())
        else {
            self.close_column_profile();
            return;
        };
        let Some(column) = result
            .columns
            .iter()
            .position(|column| column.name == profile.info.name)
        else {
            self.close_column_profile();
            return;
        };
        let mode = match profile.mode {
            ProfileMode::Category if profile_modes(result, column) => ProfileMode::Category,
            ProfileMode::Text if profile_has_text_mode(result, column) => ProfileMode::Text,
            _ => ProfileMode::Auto,
        };
        if self.statistics_tab != Some(StatisticsTab::Descriptive) {
            let info = result.columns[column].clone();
            if let Some(profile) = &mut self.column_profile {
                profile.cancel_pending();
                profile.column = column;
                profile.info = info;
                profile.mode = mode;
                profile.outcome = None;
            }
        } else {
            self.start_column_profile(column, mode, cx);
        }
        self.profile_field_scroll
            .scroll_to_item(column, ScrollStrategy::Center);
    }

    fn start_column_profile(&mut self, column: usize, mode: ProfileMode, cx: &mut Context<Self>) {
        let Some(result) = &self.result else {
            return;
        };
        let Some(info) = result.columns.get(column) else {
            return;
        };
        if let Some(profile) = &mut self.column_profile {
            profile.cancel_pending();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.column_profile = Some(ColumnProfile {
            cancel: Some(cancel.clone()),
            column,
            info: info.clone(),
            mode,
            outcome: None,
        });
        let result = result.clone();
        let chart_limit = self.preferences.profile_chart_max_rows;
        let category_bars = self.preferences.profile_chart_category_bars;
        let operation = cancel.clone();
        let task = cx.background_executor().spawn(async move {
            calculate_profile(
                &result,
                column,
                mode,
                chart_limit,
                category_bars,
                &operation,
            )
        });
        cx.spawn(async move |weak, cx| {
            let outcome = task.await;
            let _ = weak.update(cx, |this, cx| {
                if let Some(profile) = &mut this.column_profile
                    && profile
                        .cancel
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &cancel))
                {
                    profile.cancel = None;
                    profile.outcome = Some(outcome.map(|stats| {
                        ProfilePresentation::new(
                            stats,
                            &profile.info.name,
                            chart_limit,
                            category_bars,
                        )
                    }));
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn select_profile_column(&mut self, column: usize, cx: &mut Context<Self>) {
        if self.statistics_tab == Some(StatisticsTab::Descriptive)
            && self.busy.is_none()
            && self.panel.is_none()
            && self
                .column_profile
                .as_ref()
                .is_none_or(|profile| column != profile.column)
        {
            self.statistics_scroll.set_offset(point(px(0.), px(0.)));
            self.start_column_profile(column, ProfileMode::Auto, cx);
        }
    }

    pub(super) fn render_profile_fields(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(result) = &self.result else {
            return div().into_any_element();
        };
        let count = result.columns.len();
        if count == 0 {
            return div().into_any_element();
        }
        let c = Colors::new(self.preferences.dark);
        let visible_rows = count.min(VISIBLE_FIELD_ROWS);
        div()
            .w_full()
            .h(px(visible_rows as f32 * FIELD_OPTION_HEIGHT + 2.))
            .min_w_0()
            .flex_shrink_0()
            .flex()
            .border_1()
            .border_color(rgb(c.border))
            .rounded(px(5.))
            .bg(rgb(c.surface))
            .overflow_hidden()
            .child(
                uniform_list(
                    "profile-field-options",
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        let Some(result) = &this.result else {
                            return Vec::new();
                        };
                        range
                            .filter_map(|column| {
                                let info = result.columns.get(column)?;
                                let selected = this
                                    .column_profile
                                    .as_ref()
                                    .is_some_and(|profile| column == profile.column);
                                Some(
                                    controls::menu_item(
                                        ("profile-field-option", column),
                                        c,
                                        selected,
                                        true,
                                    )
                                    .w_full()
                                    .h(px(FIELD_OPTION_HEIGHT))
                                    .tooltip(icons::tooltip(
                                        format!("{} · {}", info.name, info.data_type),
                                        this.preferences.dark,
                                    ))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .when(selected, |el| {
                                                el.font_weight(gpui::FontWeight::SEMIBOLD)
                                            })
                                            .child(info.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .w(gpui::relative(0.42))
                                            .flex_shrink_0()
                                            .truncate()
                                            .text_right()
                                            .text_size(px(11.))
                                            .text_color(rgb(c.muted))
                                            .child(info.data_type.clone()),
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.select_profile_column(column, cx);
                                        },
                                    )),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .h_full()
                .min_w_0()
                .flex_1()
                .track_scroll(self.profile_field_scroll.clone()),
            )
            .when(count > visible_rows, |el| {
                el.child(
                    div()
                        .w(px(12.))
                        .h_full()
                        .flex_shrink_0()
                        .child(self.render_scrollbar(
                            false,
                            count as f32 * FIELD_OPTION_HEIGHT,
                            TableScrollbar::Pixels(
                                self.profile_field_scroll.0.borrow().base_handle.clone(),
                            ),
                            PROFILE_SCROLLBAR_ID,
                            cx,
                        )),
                )
            })
            .into_any_element()
    }

    pub(super) fn render_column_profile(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let c = Colors::new(self.preferences.dark);
        let Some(profile) = &self.column_profile else {
            return profile_note(c, t("Choose at least one field.")).into_any_element();
        };
        let mut body = div()
            .flex_shrink_0()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .pb_2()
            .child(profile_note(
                c,
                t("Statistics for all rows in the current result"),
            ));
        if let Some(result) = &self.result
            && profile_modes(result, profile.column)
        {
            let text_mode = profile_has_text_mode(result, profile.column);
            body = body.child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(profile_note(c, t("Interpret as")))
                    .child(
                        controls::segment_group(c).children(
                            [
                                ("profile-mode-auto", "Auto", ProfileMode::Auto),
                                ("profile-mode-category", "Category", ProfileMode::Category),
                                ("profile-mode-text", "Text", ProfileMode::Text),
                            ]
                            .into_iter()
                            .filter(|(_, _, mode)| *mode != ProfileMode::Text || text_mode)
                            .map(|(id, label, mode)| {
                                let selected = profile.mode == mode;
                                controls::segment(id, c, selected).child(t(label)).on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        if let Some(profile) = &this.column_profile
                                            && profile.mode != mode
                                        {
                                            this.start_column_profile(profile.column, mode, cx);
                                        }
                                    }),
                                )
                            }),
                        ),
                    ),
            );
        }

        body = match &profile.outcome {
            None => body,
            Some(Err(error)) => body.child(
                div()
                    .text_color(rgb(c.danger))
                    .whitespace_normal()
                    .child(t(error.to_string())),
            ),
            Some(Ok(presentation)) => body.child(presentation.render(c, self.preferences.dark)),
        };
        body.into_any_element()
    }
}

fn count_label(value: usize) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + (digits.len() - 1) / 3);
    for (index, digit) in digits.bytes().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(char::from(digit));
    }
    grouped
}

fn ratio_label(count: usize, total: usize) -> String {
    if total == 0 {
        return "—".into();
    }
    let percent = count as f64 / total as f64 * 100.;
    if count > 0 && percent < 0.01 {
        "<0.01%".into()
    } else if count < total && percent > 99.99 {
        ">99.99%".into()
    } else {
        format!("{percent:.2}%")
    }
}

fn profile_note(c: Colors, label: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(px(11.))
        .text_color(rgb(c.muted))
        .whitespace_normal()
        .child(label.into())
}

fn metric(c: Colors, label: &str, value: SharedString) -> gpui::Div {
    div()
        .min_w_0()
        .flex()
        .justify_between()
        .items_start()
        .gap_3()
        .px_2()
        .py_1()
        .border_b_1()
        .border_color(rgb(c.grid))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(rgb(c.muted))
                .child(t(label)),
        )
        .child(
            div()
                .w(gpui::relative(0.58))
                .flex_shrink_0()
                .text_right()
                .whitespace_normal()
                .child(value),
        )
}

// Formatting belongs to the committed profile, while theme and translated
// labels still come from the current window at render time.
struct ProfilePresentation {
    metrics: Vec<(&'static str, SharedString)>,
    chart: ProfileChart,
}

enum ProfileChart {
    Hidden,
    Message(&'static str),
    Limited(SharedString),
    Counts(Rc<CategoryChart>),
    Density(Rc<DensityChart>),
    Boolean(Rc<BooleanChart>),
}

impl ProfilePresentation {
    fn new(stats: ProfileStats, field: &str, chart_limit: usize, category_bars: usize) -> Self {
        let mut metrics = Vec::with_capacity(11);
        let mut metric = |label: &'static str, value: String| metrics.push((label, value.into()));
        metric("Rows in current result", count_label(stats.rows));
        if !matches!(
            &stats.details,
            ProfileDetails::Boolean { .. } | ProfileDetails::Temporal(_)
        ) {
            metric("Missing count", count_label(stats.nulls));
        }
        if !matches!(&stats.details, ProfileDetails::NullsOnly) {
            metric("Missing ratio", ratio_label(stats.nulls, stats.rows));
        }
        let no_values = ProfileChart::Message("No non-missing values to plot.");
        let chart = match stats.details {
            ProfileDetails::Category { distinct, common } => {
                metric("nunique", count_label(distinct));
                if category_bars == 0 {
                    ProfileChart::Message("Bar charts are disabled in Settings.")
                } else if common.is_empty() {
                    no_values
                } else {
                    ProfileChart::Counts(Rc::new(CategoryChart::new(common, category_bars)))
                }
            }
            ProfileDetails::Number {
                range,
                quantiles,
                non_finite,
                density,
                ..
            } => {
                let (min, max) = range.unwrap_or_else(|| ("—".into(), "—".into()));
                metric("Minimum", min);
                metric("Maximum", max);
                for (label, value) in [
                    "25th percentile",
                    "Median",
                    "75th percentile",
                    "90th percentile",
                    "95th percentile",
                ]
                .into_iter()
                .zip(quantiles.unwrap_or_else(|| std::array::from_fn(|_| "—".into())))
                {
                    metric(label, value);
                }
                if non_finite > 0 {
                    metric("Non-finite values", count_label(non_finite));
                }
                density.map_or(
                    ProfileChart::Message("No finite values to plot."),
                    |density| ProfileChart::Density(Rc::new(DensityChart::new(density, field))),
                )
            }
            ProfileDetails::Temporal(range) => {
                let (min, max) = range.unwrap_or_else(|| ("—".into(), "—".into()));
                metric("Minimum", min);
                metric("Maximum", max);
                ProfileChart::Hidden
            }
            ProfileDetails::Boolean { trues, falses } => {
                metric("true", count_label(trues));
                metric("false", count_label(falses));
                if trues + falses > 0 && !stats.chart_limited {
                    ProfileChart::Boolean(Rc::new(BooleanChart::new(trues, falses)))
                } else {
                    no_values
                }
            }
            ProfileDetails::NullsOnly => ProfileChart::Hidden,
        };
        let chart = if stats.chart_limited {
            if chart_limit == 0 {
                ProfileChart::Message("Charts are disabled in Settings.")
            } else {
                ProfileChart::Limited(count_label(chart_limit).into())
            }
        } else {
            chart
        };
        Self { metrics, chart }
    }

    fn render(&self, c: Colors, dark: bool) -> gpui::Div {
        let chart = match &self.chart {
            ProfileChart::Hidden => None,
            ProfileChart::Message(message) => Some(profile_note(c, t(*message))),
            ProfileChart::Limited(limit) => Some(profile_note(
                c,
                t("Chart skipped. The current result exceeds the chart row limit of {limit}.")
                    .replace("{limit}", limit.as_str()),
            )),
            ProfileChart::Counts(chart) => Some(render_counts_chart(chart, c, dark)),
            ProfileChart::Density(chart) => Some(render_density_chart(chart, c)),
            ProfileChart::Boolean(chart) => Some(render_boolean_chart(chart, c)),
        };
        div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .min_w_0()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .border_1()
                    .border_color(rgb(c.border))
                    .bg(rgb(c.surface))
                    .rounded(px(5.))
                    .overflow_hidden()
                    .children(
                        self.metrics
                            .iter()
                            .map(|(label, value)| metric(c, label, value.clone())),
                    ),
            )
            .when_some(chart, |el, chart| el.child(chart))
    }
}

fn chart_card(c: Colors, title: &str) -> gpui::Div {
    div()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap_2()
        .min_w_0()
        .p_2()
        .border_1()
        .border_color(rgb(c.border))
        .rounded(px(5.))
        .bg(rgb(c.surface))
        .child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(t(title)),
        )
}

struct CategoryBar {
    label: SharedString,
    count: SharedString,
    fraction: f32,
}

struct CategoryChart {
    counts: Vec<(String, usize)>,
    max_count: usize,
    maximum: SharedString,
    count_width: Pixels,
    limit: Option<SharedString>,
}

impl CategoryChart {
    fn new(common: Vec<(String, usize)>, category_bars: usize) -> Self {
        // The statistics worker returns categories in descending frequency order.
        let max_count = common.first().map_or(1, |(_, count)| (*count).max(1));
        let maximum = SharedString::from(count_label(max_count));
        let count_width = px((maximum.as_str().len() as f32 * 7. + 6.).max(48.));
        Self {
            counts: common,
            max_count,
            maximum,
            count_width,
            limit: (category_bars != usize::MAX).then(|| count_label(category_bars).into()),
        }
    }

    fn bar(&self, index: usize) -> CategoryBar {
        let (label, count) = &self.counts[index];
        CategoryBar {
            label: if label.is_empty() {
                "\"\"".into()
            } else {
                label.clone().into()
            },
            count: if *count == self.max_count {
                self.maximum.clone()
            } else {
                count_label(*count).into()
            },
            fraction: *count as f32 / self.max_count as f32,
        }
    }
}

fn render_counts_chart(chart: &Rc<CategoryChart>, c: Colors, dark: bool) -> gpui::Div {
    let count_width = chart.count_width;
    let label_width = gpui::relative(0.40);
    let data = chart.clone();
    let caption = match &chart.limit {
        Some(limit) => t("Most frequent categories (up to {limit})").replace("{limit}", limit),
        None => t("All categories, sorted by frequency"),
    };
    chart_card(c, "value_counts")
        .child(profile_note(c, caption))
        .child(controls::chart_rows(
            "profile-category-bars",
            data.counts.len(),
            move |index| {
                let bar = data.bar(index);
                div()
                    .id(("profile-category-bar", index))
                    .flex()
                    .items_center()
                    .min_w_0()
                    .text_size(px(11.))
                    .tooltip(icons::tooltip(bar.label.clone(), dark))
                    .child(
                        div()
                            .w(label_width)
                            .pr_2()
                            .flex_shrink_0()
                            .truncate()
                            .child(bar.label),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .border_l_1()
                            .border_color(rgb(c.border))
                            .child(
                                div()
                                    .h(px(10.))
                                    .w(gpui::relative(bar.fraction))
                                    .bg(rgb(c.primary))
                                    .rounded_r(px(2.)),
                            ),
                    )
                    .child(
                        div()
                            .w(count_width)
                            .flex_shrink_0()
                            .text_right()
                            .child(bar.count),
                    )
            },
        ))
        .child(
            div()
                .flex()
                .items_start()
                .text_size(px(10.))
                .text_color(rgb(c.muted))
                .child(div().w(label_width).flex_shrink_0())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .justify_between()
                        .border_t_1()
                        .border_color(rgb(c.border))
                        .pt_1()
                        .child("0")
                        .child(chart.maximum.clone()),
                )
                .child(div().w(count_width).flex_shrink_0()),
        )
        .child(
            div()
                .flex()
                .child(div().w(label_width).flex_shrink_0())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_center()
                        .child(profile_note(c, t("Count"))),
                )
                .child(div().w(count_width).flex_shrink_0()),
        )
}

type PlotPaths = [Option<gpui::Path<Pixels>>; 2];

#[derive(Default)]
struct PlotGeometry(Option<(Bounds<Pixels>, PlotPaths)>);

impl PlotGeometry {
    fn paths(&mut self, bounds: Bounds<Pixels>, build: impl FnOnce() -> PlotPaths) -> &PlotPaths {
        if self
            .0
            .as_ref()
            .is_none_or(|(previous, _)| *previous != bounds)
        {
            self.0 = Some((bounds, build()));
        }
        &self.0.as_ref().expect("plot geometry is populated").1
    }
}

fn plot_visible(bounds: Bounds<Pixels>, padding: Pixels, window: &Window) -> bool {
    let outline = Bounds::from_corners(
        point(bounds.left() - padding, bounds.top() - padding),
        point(bounds.right() + padding, bounds.bottom() + padding),
    );
    outline.intersects(&window.content_mask().bounds)
}

fn paint_plot_paths(paths: &PlotPaths, colors: [u32; 2], window: &mut Window) {
    for (path, color) in paths.iter().zip(colors) {
        if let Some(path) = path {
            // GPUI consumes and scales the owned path on each paint. Reuse
            // the tessellation while leaving scale, clipping and theme live.
            window.paint_path(path.clone(), rgb(color));
        }
    }
}

struct DensityChart {
    samples: Vec<(f32, f32)>,
    labels: [SharedString; 3],
    field: SharedString,
    geometry: RefCell<PlotGeometry>,
}

impl DensityChart {
    fn new(density: Density, field: &str) -> Self {
        let denominator = density.points.len().saturating_sub(1).max(1) as f32;
        let samples = density
            .points
            .into_iter()
            .enumerate()
            .map(|(index, value)| (index as f32 / denominator, (value as f32).clamp(0., 1.)))
            .collect();
        Self {
            samples,
            labels: density.labels.map(Into::into),
            field: SharedString::new(field),
            geometry: RefCell::default(),
        }
    }

    fn build_paths(&self, bounds: Bounds<Pixels>) -> PlotPaths {
        let width = bounds.size.width;
        let bottom = bounds.bottom() - px(1.);
        let height = bounds.size.height - px(5.);
        let mut outline = PathBuilder::stroke(px(1.5));
        let mut area = PathBuilder::fill();
        area.move_to(point(bounds.left(), bottom));
        for (index, &(x, y)) in self.samples.iter().enumerate() {
            let next = point(bounds.left() + width * x, bottom - height * y);
            if index == 0 {
                outline.move_to(next);
            } else {
                outline.line_to(next);
            }
            area.line_to(next);
        }
        area.line_to(point(bounds.right(), bottom));
        area.close();
        [area.build().ok(), outline.build().ok()]
    }
}

fn render_density_chart(density: &Rc<DensityChart>, c: Colors) -> gpui::Div {
    let plot = density.clone();
    chart_card(c, "Density plot")
        .child(profile_note(c, t("Relative density")))
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    if plot.samples.len() < 2
                        || bounds.size.width <= px(2.)
                        || !plot_visible(bounds, px(4.), window)
                    {
                        return;
                    }
                    let width = bounds.size.width;
                    let bottom = bounds.bottom() - px(1.);
                    let height = bounds.size.height - px(5.);
                    for fraction in [0., 0.25, 0.5, 0.75, 1.] {
                        window.paint_quad(gpui::fill(
                            Bounds::new(
                                point(bounds.left(), bottom - height * fraction),
                                size(width, px(1.)),
                            ),
                            rgb(if fraction == 0. { c.border } else { c.grid }),
                        ));
                    }
                    let mut geometry = plot.geometry.borrow_mut();
                    let paths = geometry.paths(bounds, || plot.build_paths(bounds));
                    paint_plot_paths(paths, [c.accent_soft, c.accent], window);
                },
            )
            .w_full()
            .h(px(128.)),
        )
        .child(
            div()
                .flex()
                .justify_between()
                .gap_2()
                .text_size(px(10.))
                .text_color(rgb(c.muted))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(density.labels[0].clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_center()
                        .child(density.labels[1].clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_right()
                        .child(density.labels[2].clone()),
                ),
        )
        .child(
            div()
                .text_center()
                .truncate()
                .text_size(px(11.))
                .text_color(rgb(c.muted))
                .child(density.field.clone()),
        )
}

struct BooleanChart {
    arcs: [Vec<(f32, f32)>; 2],
    labels: [SharedString; 2],
    geometry: RefCell<PlotGeometry>,
}

impl BooleanChart {
    fn new(trues: usize, falses: usize) -> Self {
        let counts = [trues, falses];
        let total = trues + falses;
        let mut start = -std::f64::consts::FRAC_PI_2;
        let arcs = counts.map(|count| {
            if count == 0 {
                return Vec::new();
            }
            let sweep = std::f64::consts::TAU * count as f64 / total as f64;
            let segments = (64. * count as f64 / total as f64).ceil() as usize;
            let arc = (0..=segments)
                .map(|index| {
                    let angle = start + sweep * index as f64 / segments as f64;
                    let (sin, cos) = angle.sin_cos();
                    (cos as f32, sin as f32)
                })
                .collect();
            start += sweep;
            arc
        });
        let labels = counts
            .map(|count| format!("{} · {}", count_label(count), ratio_label(count, total)).into());
        Self {
            arcs,
            labels,
            geometry: RefCell::default(),
        }
    }

    fn build_paths(&self, bounds: Bounds<Pixels>) -> PlotPaths {
        let radius = bounds.size.width.min(bounds.size.height) / 2. - px(2.);
        let center = point(
            bounds.left() + bounds.size.width / 2.,
            bounds.top() + bounds.size.height / 2.,
        );
        self.arcs.each_ref().map(|arc| {
            if arc.is_empty() {
                return None;
            }
            let mut wedge = PathBuilder::fill();
            wedge.move_to(center);
            for &(x, y) in arc {
                wedge.line_to(point(center.x + radius * x, center.y + radius * y));
            }
            wedge.close();
            wedge.build().ok()
        })
    }
}

fn render_boolean_chart(chart: &Rc<BooleanChart>, c: Colors) -> gpui::Div {
    let plot = chart.clone();
    let colors = [c.primary, c.success];
    chart_card(c, "Boolean distribution").child(
        div()
            .flex()
            .items_center()
            .gap_3()
            .min_w_0()
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        if !plot_visible(bounds, px(0.), window) {
                            return;
                        }
                        let mut geometry = plot.geometry.borrow_mut();
                        let paths = geometry.paths(bounds, || plot.build_paths(bounds));
                        paint_plot_paths(paths, colors, window);
                    },
                )
                .size(px(94.))
                .flex_shrink_0(),
            )
            .child(
                div().flex_1().min_w_0().flex().flex_col().gap_2().children(
                    ["true", "false"]
                        .into_iter()
                        .zip(colors)
                        .zip(&chart.labels)
                        .map(|((label, color), summary)| {
                            div()
                                .flex()
                                .min_w_0()
                                .items_center()
                                .gap_2()
                                .text_size(px(11.))
                                .child(
                                    div()
                                        .size(px(9.))
                                        .flex_shrink_0()
                                        .rounded(px(2.))
                                        .bg(rgb(color)),
                                )
                                .child(div().flex_1().min_w_0().truncate().child(label))
                                .child(div().text_right().child(summary.clone()))
                        }),
                ),
            ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_percent_labels_do_not_hide_rare_values() {
        assert_eq!(ratio_label(0, 0), "—");
        assert_eq!(ratio_label(0, 100_000), "0.00%");
        assert_eq!(ratio_label(1, 100_000), "<0.01%");
        assert_eq!(ratio_label(99_999, 100_000), ">99.99%");
        assert_eq!(ratio_label(100_000, 100_000), "100.00%");
    }

    #[test]
    fn plot_geometry_reuses_paths_until_the_canvas_moves_or_resizes() {
        let mut geometry = PlotGeometry::default();
        let first = Bounds::new(point(px(20.), px(40.)), size(px(220.), px(128.)));
        let scrolled = Bounds::new(point(px(20.), px(35.)), first.size);
        let resized = Bounds::new(scrolled.origin, size(px(260.), px(128.)));
        let mut builds = 0;
        for bounds in [first, first, scrolled, scrolled, resized, resized] {
            geometry.paths(bounds, || {
                builds += 1;
                [None, None]
            });
        }
        assert_eq!(builds, 3);
    }

    #[test]
    fn category_chart_prepares_exact_labels_and_relative_bar_widths() {
        let chart = CategoryChart::new(vec![(String::new(), 12_000), ("Other".into(), 3_000)], 8);
        assert_eq!(chart.maximum, "12,000");
        let first = chart.bar(0);
        assert_eq!(first.label, "\"\"");
        assert_eq!(first.count, "12,000");
        assert_eq!(first.fraction, 1.);
        let second = chart.bar(1);
        assert_eq!(second.label, "Other");
        assert_eq!(second.count, "3,000");
        assert_eq!(second.fraction, 0.25);
    }
}
