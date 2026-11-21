//! Search and choose the Arrow columns retained by the loaded table.
use super::*;
use gpui::{UniformListScrollHandle, uniform_list};

#[derive(Clone, Copy)]
enum FieldOption {
    Available(usize),
    Excluded(usize),
}

impl Viewer {
    pub(super) fn toggle_fields_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panel.is_some() || self.busy.is_some() || self.info.is_none() {
            return;
        }
        if self.fields_sidebar_open {
            self.dismiss_fields_sidebar(window, cx);
            return;
        }
        self.open_fields_sidebar(window, cx);
    }

    pub(super) fn open_fields_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_statistics_sidebar(cx);
        self.close_preview(cx);
        self.filter_sidebar_open = false;
        self.axis_menu = None;
        self.selection_menu = None;
        self.settings_menu = None;
        self.error = None;
        self.pending_fields = if self.fields_explicitly_chosen {
            self.chosen_fields.iter().cloned().collect()
        } else {
            HashSet::new()
        };
        self.field_scroll = UniformListScrollHandle::new();
        self.fields_sidebar_open = true;
        let search = self.field_search.focus_handle(cx);
        // Wait for the newly mounted search control, including when the range
        // dialog has just been dismissed and focus is being reassigned.
        window.defer(cx, move |window, _| search.focus(window));
        cx.notify();
    }

    pub(super) fn close_fields_sidebar(&mut self) {
        self.fields_sidebar_open = false;
        self.pending_fields = HashSet::new();
    }

    pub(super) fn dismiss_fields_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_fields_sidebar();
        if std::mem::take(&mut self.selecting_initial_fields) {
            self.panel = Some(Panel::Range);
        }
        self.focus.focus(window);
        cx.notify();
    }

    fn select_all_fields(&mut self, cx: &mut Context<Self>) {
        if let Some(info) = &self.info {
            self.pending_fields = info
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect();
            cx.notify();
        }
    }

    fn apply_fields(&mut self, cx: &mut Context<Self>) {
        let Some(info) = &self.info else {
            return;
        };
        if self.busy.is_some() || self.panel.is_some() || self.pending_fields.is_empty() {
            return;
        }
        if self
            .visual_filters
            .conditions
            .iter()
            .any(|condition| !self.pending_fields.contains(&condition.field))
        {
            self.error = Some(
                "A field used by a filter cannot be hidden. Remove its condition first.".into(),
            );
            cx.notify();
            return;
        }
        let fields = info
            .columns
            .iter()
            .filter(|column| self.pending_fields.contains(&column.name))
            .map(|column| column.name.clone())
            .collect::<Vec<_>>();
        if std::mem::take(&mut self.selecting_initial_fields) {
            self.request.retain_sort_fields(&fields);
            self.chosen_fields = fields;
            self.fields_explicitly_chosen = true;
            self.close_fields_sidebar();
            self.panel = Some(Panel::Range);
            cx.notify();
        } else if let Some(mut request) = self.current_request(cx) {
            request.retain_sort_fields(&fields);
            request.selected_columns = fields;
            if request.selected_columns == self.chosen_fields {
                self.fields_explicitly_chosen = true;
            }
            self.close_fields_sidebar();
            self.execute_request(request, self.visual_filters.clone(), None, cx);
        }
    }

    fn field_options(&self, info: &DatasetInfo, cx: &App) -> Vec<FieldOption> {
        let query = self.field_search.read(cx).text().to_lowercase();
        let query = query.trim();
        let terms = query.contains([',', '，']).then(|| {
            query
                .split([',', '，'])
                .map(str::trim)
                .collect::<HashSet<_>>()
        });
        let matches = |name: &str| {
            if query.is_empty() {
                return true;
            }
            let name = name.to_lowercase();
            match &terms {
                Some(terms) => terms.contains(name.as_str()),
                None => name.contains(query),
            }
        };
        info.columns
            .iter()
            .enumerate()
            .filter_map(|(index, column)| {
                matches(&column.name).then_some(FieldOption::Available(index))
            })
            .chain(
                info.excluded_fields
                    .iter()
                    .enumerate()
                    .filter_map(|(index, name)| {
                        matches(name).then_some(FieldOption::Excluded(index))
                    }),
            )
            .collect()
    }

    pub(super) fn render_fields_sidebar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(info) = &self.info else {
            return div().into_any_element();
        };
        let colors = Colors::new(self.preferences.dark);
        let options = self.field_options(info, cx);
        let option_count = options.len();
        let interactive = self.busy.is_none() && self.panel.is_none();
        div()
            .id("fields-sidebar")
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(colors.border))
            .bg(rgb(colors.bar))
            .text_color(rgb(colors.text))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_3()
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(rgb(colors.border))
                    .child(icons::glyph(Icon::Fields, 16., self.preferences.dark))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(t("Choose Fields")),
                    )
                    .child(
                        controls::icon_button("fields-sidebar-close", colors, interactive)
                            .child(icons::glyph(Icon::Close, 14., self.preferences.dark))
                            .tooltip(icons::tooltip(t("Close"), self.preferences.dark))
                            .when(interactive, |el| {
                                el.on_click(cx.listener(|this, _, window, cx| {
                                    this.dismiss_fields_sidebar(window, cx)
                                }))
                            }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .when(self.selecting_initial_fields, |el| {
                        el.child(
                            div()
                                .text_color(rgb(colors.muted))
                                .child(t("Choose fields before loading records.")),
                        )
                    })
                    .child(
                        self.input_shell(&self.field_search, None)
                            .w_full()
                            .flex_shrink_0()
                            .flex_none(),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_1()
                            .child(self.button(
                                "select-fields",
                                "Select All",
                                interactive,
                                |this, _, cx| this.select_all_fields(cx),
                                cx,
                            ))
                            .child(self.button(
                                "clear-fields",
                                "Select None",
                                interactive,
                                |this, _, cx| {
                                    this.pending_fields.clear();
                                    cx.notify();
                                },
                                cx,
                            ))
                            .when(self.selecting_initial_fields, |el| {
                                el.child(self.button(
                                    "initial-saved-views",
                                    "Saved Views…",
                                    interactive,
                                    |this, _, cx| this.open_saved_views(cx),
                                    cx,
                                ))
                            }),
                    )
                    .when(option_count == 0, |el| {
                        el.child(
                            div()
                                .flex_1()
                                .text_color(rgb(colors.muted))
                                .child(t("No matching fields")),
                        )
                    })
                    .when(option_count > 0, |el| {
                        el.child(
                            uniform_list(
                                "field-list",
                                option_count,
                                cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                                    range
                                        .filter_map(|index| {
                                            this.render_field_option(
                                                *options.get(index)?,
                                                colors,
                                                cx,
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .border_1()
                            .border_color(rgb(colors.border))
                            .rounded(px(5.))
                            .bg(rgb(colors.surface))
                            .track_scroll(self.field_scroll.clone()),
                        )
                    }),
            )
            .child(
                div()
                    .p_3()
                    .border_t_1()
                    .border_color(rgb(colors.border))
                    .flex()
                    .justify_end()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .when(self.pending_fields.is_empty(), |el| {
                        el.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_normal()
                                .text_color(rgb(colors.muted))
                                .child(t("Choose at least one field.")),
                        )
                    })
                    .child(self.button(
                        "apply-fields",
                        format!("Apply {} Fields", self.pending_fields.len()),
                        interactive && !self.pending_fields.is_empty(),
                        |this, _, cx| this.apply_fields(cx),
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn render_field_option(
        &self,
        option: FieldOption,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let info = self.info.as_ref()?;
        let (id, name, data_type, enabled) = match option {
            FieldOption::Available(index) => {
                let column = info.columns.get(index)?;
                (
                    ("field-option", index),
                    &column.name,
                    column.data_type.clone(),
                    true,
                )
            }
            FieldOption::Excluded(index) => (
                ("excluded-field", index),
                info.excluded_fields.get(index)?,
                t("Duplicate name — excluded"),
                false,
            ),
        };
        let selected = enabled && self.pending_fields.contains(name);
        let name = name.clone();
        Some(
            controls::menu_item(id, colors, selected, enabled)
                .w_full()
                .h(px(31.))
                .tooltip(icons::tooltip(
                    format!("{name} · {data_type}"),
                    self.preferences.dark,
                ))
                .child(icons::checkbox(selected, self.preferences.dark))
                .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                .child(
                    div()
                        .w(gpui::relative(0.42))
                        .flex_shrink_0()
                        .truncate()
                        .text_right()
                        .text_size(px(11.))
                        .text_color(rgb(colors.muted))
                        .child(data_type),
                )
                .when(enabled, |row| {
                    row.on_click(cx.listener(move |this, _, _, cx| {
                        if !this.pending_fields.remove(&name) {
                            this.pending_fields.insert(name.clone());
                        }
                        cx.notify();
                    }))
                }),
        )
    }
}
