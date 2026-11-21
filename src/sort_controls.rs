//! Edit sort priority and the optional row cap without touching the import range.
use super::*;
use crate::engine::SortKey;

const MAX_FIELD_CHOICES: usize = 48;

pub(super) struct SortControls {
    keys: Vec<SortKey>,
    top_n: Entity<TextInput>,
    field_search: Entity<TextInput>,
    choosing_field: bool,
    _inputs: [Subscription; 2],
}

impl Viewer {
    pub(super) fn open_sort_controls(&mut self, fields: Vec<String>, cx: &mut Context<Self>) {
        let request = &self.displayed_request;
        let mut keys = request
            .sort_rules()
            .map(|(field, descending, nulls_first)| SortKey {
                field: field.to_owned(),
                descending,
                nulls_first,
            })
            .collect::<Vec<_>>();
        if keys.is_empty() {
            keys.extend(fields.into_iter().map(|field| SortKey {
                field,
                descending: false,
                nulls_first: false,
            }));
        }
        let dark = self.preferences.dark;
        let cap = request.top_n.map(|rows| rows.to_string());
        let top_n = cx.new(|cx| {
            let mut input = TextInput::new(cx)
                .with_dark(dark)
                .with_placeholder(&t("All matching rows"));
            if let Some(cap) = cap {
                input.set_text(&cap, cx);
            }
            input
        });
        let field_search = cx.new(|cx| {
            TextInput::new(cx)
                .with_dark(dark)
                .with_placeholder(&t("Search fields"))
        });
        let top_n_subscription = cx.subscribe(&top_n, |this, _, event, cx| match event {
            InputEvent::Changed => {
                this.error = None;
                cx.notify();
            }
            InputEvent::Submit => this.apply_sort_controls(cx),
        });
        let field_subscription = cx.subscribe(&field_search, |_: &mut Self, _, _, cx| cx.notify());
        self.sort_controls = Some(SortControls {
            keys,
            top_n,
            field_search,
            choosing_field: false,
            _inputs: [top_n_subscription, field_subscription],
        });
        self.error = None;
        self.panel = Some(Panel::Sort);
        cx.notify();
    }

    pub(super) fn sort_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.panel == Some(Panel::Sort)
            && self.sort_controls.as_ref().is_some_and(|sort| {
                [&sort.top_n, &sort.field_search]
                    .iter()
                    .any(|input| input.focus_handle(cx).is_focused(window))
            })
    }

    pub(super) fn sync_sort_inputs(&self, cx: &mut Context<Self>) {
        let Some(sort) = &self.sort_controls else {
            return;
        };
        for (input, placeholder) in [
            (&sort.top_n, "All matching rows"),
            (&sort.field_search, "Search fields"),
        ] {
            input.update(cx, |input, cx| {
                input.set_dark(self.preferences.dark, cx);
                input.set_placeholder(&t(placeholder), cx);
            });
        }
    }

    pub(super) fn execute_sort(
        &mut self,
        keys: Vec<SortKey>,
        top_n: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let Some(mut request) = self.current_request(cx) else {
            return;
        };
        request.set_sort(keys, top_n);
        self.sort_controls = None;
        if self.panel == Some(Panel::Sort) {
            self.panel = None;
        }
        self.execute_request(request, self.visual_filters.clone(), None, cx);
    }

    fn apply_sort_controls(&mut self, cx: &mut Context<Self>) {
        let Some(sort) = &self.sort_controls else {
            return;
        };
        let input = sort.top_n.read(cx).text();
        let input = input.trim();
        let top_n = if input.is_empty() {
            None
        } else if let Some(rows) = input.parse::<usize>().ok().filter(|rows| *rows > 0)
            && !sort.keys.is_empty()
        {
            Some(rows)
        } else {
            self.error =
                Some("Top N needs a positive row count and at least one sort field.".into());
            cx.notify();
            return;
        };
        self.execute_sort(sort.keys.clone(), top_n, cx);
    }

    fn edit_sort_keys(&mut self, change: impl FnOnce(&mut Vec<SortKey>), cx: &mut Context<Self>) {
        if let Some(sort) = &mut self.sort_controls {
            change(&mut sort.keys);
            self.error = None;
            cx.notify();
        }
    }

    pub(super) fn render_sort_controls(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(sort) = &self.sort_controls else {
            return div().into_any_element();
        };
        let c = Colors::new(self.preferences.dark);
        let key_count = sort.keys.len();
        let fields = self.result.iter().flat_map(|result| &result.columns);
        let can_add = fields.clone().count() > key_count;
        let mut body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_color(rgb(c.muted)).child(t(
                "Sort fields in priority order. Ties keep original row order.",
            )))
            .children(sort.keys.iter().enumerate().map(|(index, key)| {
                div()
                    .id(("sort-key", index))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_1()
                    .border_color(rgb(c.border))
                    .rounded(px(3.))
                    .child(
                        div()
                            .w(px(24.))
                            .text_color(rgb(c.muted))
                            .child((index + 1).to_string()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(90.))
                            .truncate()
                            .child(key.field.clone()),
                    )
                    .child(self.button(
                        "sort-direction",
                        if key.descending {
                            "Descending"
                        } else {
                            "Ascending"
                        },
                        true,
                        move |this, _, cx| {
                            this.edit_sort_keys(
                                |keys| keys[index].descending = !keys[index].descending,
                                cx,
                            );
                        },
                        cx,
                    ))
                    .child(self.button(
                        "sort-nulls",
                        if key.nulls_first {
                            "NULL first"
                        } else {
                            "NULL last"
                        },
                        true,
                        move |this, _, cx| {
                            this.edit_sort_keys(
                                |keys| keys[index].nulls_first = !keys[index].nulls_first,
                                cx,
                            );
                        },
                        cx,
                    ))
                    .child(self.button(
                        "sort-up",
                        "Move up",
                        index > 0,
                        move |this, _, cx| {
                            this.edit_sort_keys(|keys| keys.swap(index, index - 1), cx);
                        },
                        cx,
                    ))
                    .child(self.button(
                        "sort-down",
                        "Move down",
                        index + 1 < key_count,
                        move |this, _, cx| {
                            this.edit_sort_keys(|keys| keys.swap(index, index + 1), cx);
                        },
                        cx,
                    ))
                    .child(self.button(
                        "sort-remove",
                        "Remove",
                        true,
                        move |this, _, cx| {
                            this.edit_sort_keys(
                                |keys| {
                                    keys.remove(index);
                                },
                                cx,
                            );
                        },
                        cx,
                    ))
            }))
            .child(
                controls::selector("sort-add", c, sort.choosing_field, can_add)
                    .justify_between()
                    .child(t("Add sort field"))
                    .child(controls::chevron(c, sort.choosing_field))
                    .when(can_add, |el| {
                        el.on_click(cx.listener(|this, _, window, cx| {
                            if let Some(sort) = &mut this.sort_controls {
                                sort.choosing_field = !sort.choosing_field;
                                if sort.choosing_field {
                                    sort.field_search
                                        .update(cx, |input, cx| input.set_text("", cx));
                                    let field = sort.field_search.focus_handle(cx);
                                    window.defer(cx, move |window, _| field.focus(window));
                                } else {
                                    this.focus.focus(window);
                                }
                                cx.notify();
                            }
                        }))
                    }),
            );
        if sort.choosing_field && can_add {
            let search = sort.field_search.read(cx).text().trim().to_lowercase();
            let matches = fields
                .filter(|field| !sort.keys.iter().any(|key| key.field == field.name))
                .filter(|field| search.is_empty() || field.name.to_lowercase().contains(&search))
                .take(MAX_FIELD_CHOICES + 1)
                .collect::<Vec<_>>();
            body = body
                .child(self.input_shell(&sort.field_search, None))
                .child(
                    div()
                        .id("sort-field-options")
                        .max_h(px(144.))
                        .overflow_y_scroll()
                        .border_1()
                        .border_color(rgb(c.border))
                        .rounded(px(3.))
                        .when(matches.is_empty(), |el| {
                            el.child(div().p_2().child(t("No matching fields")))
                        })
                        .children(matches.iter().take(MAX_FIELD_CHOICES).enumerate().map(
                            |(index, field)| {
                                let name = field.name.clone();
                                controls::menu_item(("sort-field-option", index), c, false, true)
                                    .h(px(26.))
                                    .child(div().min_w_0().truncate().child(name.clone()))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if let Some(sort) = &mut this.sort_controls {
                                            sort.keys.push(SortKey {
                                                field: name.clone(),
                                                descending: false,
                                                nulls_first: false,
                                            });
                                            sort.choosing_field = false;
                                            this.error = None;
                                            this.focus.focus(window);
                                            cx.notify();
                                        }
                                    }))
                            },
                        )),
                )
                .when(matches.len() > MAX_FIELD_CHOICES, |el| {
                    el.child(t("Search for more fields."))
                });
        }
        body.child(div().h(px(1.)).bg(rgb(c.border)))
            .child(div().flex().items_center().gap_2()
                .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(t("Top N")))
                .child(self.input_shell(&sort.top_n, Some(176.))))
            .child(div().text_color(rgb(c.muted)).child(t("Top N keeps the first matching rows after sorting. Leave it blank to show all matches.")))
            .child(div().flex().justify_end().gap_2()
                .child(self.button("sort-clear", "Clear Sort & Top N", true, |this, _, cx| this.execute_sort(Vec::new(), None, cx), cx))
                .child(self.button("execute", "Apply Sort", true, |this, _, cx| this.apply_sort_controls(cx), cx)))
            .into_any_element()
    }
}
