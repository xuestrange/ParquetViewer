use super::*;
use crate::controls;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum StatisticsTab {
    Descriptive,
    Aggregation,
}

impl StatisticsTab {
    fn label(self) -> &'static str {
        match self {
            Self::Descriptive => "Descriptive Statistics",
            Self::Aggregation => "Aggregation",
        }
    }
}

pub(crate) fn section_header(
    id: &'static str,
    label: &'static str,
    open: bool,
    colors: Colors,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .min_w_0()
        .min_h(px(30.))
        .px_2()
        .flex()
        .items_center()
        .gap_2()
        .rounded(px(4.))
        .border_1()
        .border_color(rgb(colors.border))
        .bg(rgb(colors.surface))
        .hover(|el| el.bg(rgb(colors.hover)))
        .cursor_pointer()
        .child(
            div()
                .text_color(rgb(colors.muted))
                .child(if open { "▾" } else { "▸" }),
        )
        .child(
            div()
                .truncate()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(t(label)),
        )
}

impl Viewer {
    pub(super) fn toggle_statistics_sidebar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.statistics_tab.is_some() {
            self.close_statistics_sidebar(cx);
            self.focus.focus(window);
        } else if self.busy.is_none()
            && self.panel.is_none()
            && self
                .result
                .as_ref()
                .is_some_and(|result| !result.columns.is_empty())
        {
            self.close_fields_sidebar();
            self.close_preview(cx);
            self.filter_sidebar_open = false;
            self.settings_menu = None;
            self.selection_menu = None;
            self.axis_menu = None;
            self.select_statistics_tab(StatisticsTab::Descriptive, window, cx);
        }
        cx.notify();
    }

    pub(super) fn close_statistics_sidebar(&mut self, cx: &mut Context<Self>) {
        if self.statistics_tab.take().is_none() {
            return;
        }
        // Closing the panel hides the current analysis. Source and snapshot
        // changes handle result invalidation separately.
        self.pause_column_profile();
        if self.aggregation_inflight {
            self.cancel(cx);
        }
        self.dismiss_aggregation_picker(cx);
        cx.notify();
    }

    fn select_statistics_tab(
        &mut self,
        tab: StatisticsTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.statistics_tab == Some(tab) || self.busy.is_some() || self.panel.is_some() {
            return;
        }
        self.statistics_tab = Some(tab);
        match tab {
            StatisticsTab::Descriptive => {
                self.dismiss_aggregation_picker(cx);
                self.restore_aggregation_details(cx);
                self.resume_column_profile(cx);
            }
            StatisticsTab::Aggregation => {
                self.pause_column_profile();
                self.prepare_aggregation_editor(cx);
            }
        }
        self.focus.focus(window);
        cx.notify();
    }

    fn render_statistics_tabs(
        &self,
        active: StatisticsTab,
        c: Colors,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let can_switch = self.busy.is_none() && self.panel.is_none();
        let tabs = [
            ("statistics-descriptive", StatisticsTab::Descriptive),
            ("statistics-aggregation", StatisticsTab::Aggregation),
        ];
        controls::segment_group(c)
            .flex_1()
            .min_w_0()
            .children(tabs.into_iter().map(|(id, tab)| {
                let selected = active == tab;
                controls::segment(id, c, selected)
                    .flex_grow()
                    .min_w_0()
                    .child(div().truncate().child(t(tab.label())))
                    .when(!can_switch && !selected, |el| {
                        el.opacity(0.55).cursor_default()
                    })
                    .when(can_switch && !selected, |el| {
                        el.on_click(cx.listener(move |this, _, window, cx| {
                            this.select_statistics_tab(tab, window, cx);
                        }))
                    })
            }))
    }

    pub(super) fn render_statistics_sidebar(
        &self,
        width: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let c = Colors::new(self.preferences.dark);
        let Some(tab) = self.statistics_tab else {
            return div().into_any_element();
        };
        let descriptive = tab == StatisticsTab::Descriptive;
        div()
            .id("statistics-sidebar")
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(c.border))
            .bg(rgb(c.bar))
            .text_color(rgb(c.text))
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(c.border))
                    .child(self.render_statistics_tabs(tab, c, cx))
                    .child(
                        controls::icon_button("statistics-close", c, true)
                            .child(icons::glyph(Icon::Close, 14., self.preferences.dark))
                            .tooltip(icons::tooltip(t("Close"), self.preferences.dark))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_statistics_sidebar(cx);
                                this.focus.focus(window);
                            })),
                    ),
            )
            .when(descriptive, |el| {
                el.child(
                    div()
                        .min_w_0()
                        .flex_shrink_0()
                        .px_3()
                        .pt_3()
                        .child(self.render_profile_fields(cx)),
                )
            })
            .child(
                div()
                    .id("statistics-scroll")
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(if descriptive {
                        &self.statistics_scroll
                    } else {
                        &self.aggregation_scroll
                    })
                    .p_3()
                    .flex()
                    .flex_col()
                    .child(div().min_w_0().flex_shrink_0().child(if descriptive {
                        self.render_column_profile(cx)
                    } else {
                        self.render_aggregation_section(width - 24., cx)
                    })),
            )
            .into_any_element()
    }
}

pub(super) fn sidebar_width(viewport: f32, requested: f32) -> f32 {
    let maximum = maximum_sidebar_width(viewport);
    requested.clamp(270_f32.min(maximum), maximum)
}

fn maximum_sidebar_width(viewport: f32) -> f32 {
    (viewport - 180.).max(0.).min(viewport * 0.72)
}

#[cfg(test)]
mod tests {
    use super::sidebar_width;

    #[test]
    fn sidebar_resize_reserves_space_for_the_table() {
        assert_eq!(sidebar_width(1200., 460.), 460.);
        assert_eq!(sidebar_width(1200., 100.), 270.);
        assert!((sidebar_width(1200., 2000.) - 864.).abs() < 0.001);
        assert_eq!(sidebar_width(500., 2000.), 320.);
        assert_eq!(sidebar_width(350., 460.), 170.);
    }
}
