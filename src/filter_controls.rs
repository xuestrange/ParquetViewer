//! Persistent, draft-preserving filter sidebar over the loaded row window.
use super::*;

impl Viewer {
    fn filter_columns(&self) -> Vec<ColumnInfo> {
        self.info
            .as_ref()
            .map(|info| {
                info.columns
                    .iter()
                    .filter(|column| self.chosen_fields.contains(&column.name))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn effective_filter(&self) -> anyhow::Result<String> {
        self.visual_filters.to_filter(&self.filter_columns())
    }

    pub(super) fn reset_filter_controls(&mut self, cx: &mut Context<Self>) {
        self.visual_filters = FilterGroup::default();
        self.displayed_filters = FilterGroup::default();
        self.filter_editor_subscription = None;
        self.filter_editor_observer = None;
        self.filter_editor = None;
        self.filter_sidebar_open = false;
        cx.notify();
    }

    /// Explicitly replace the editor after a schema/group change. Toggling or
    /// rendering the sidebar never calls this for an already-existing draft.
    pub(super) fn sync_filter_editor(&mut self, cx: &mut Context<Self>) {
        self.install_filter_editor(self.visual_filters.clone(), cx);
    }

    pub(super) fn install_filter_editor(&mut self, group: FilterGroup, cx: &mut Context<Self>) {
        let columns = self.filter_columns();
        let dark = self.preferences.dark;
        let host_focus = self.focus.clone();
        let whole_source = self.request.whole_source;
        let loaded_result = self.result.as_ref().map(Arc::downgrade).unwrap_or_default();
        let editor = cx.new(|cx| {
            let mut editor = FilterEditor::new(columns, group, dark, cx);
            editor.set_host_focus(host_focus);
            editor.set_whole_source(whole_source, cx);
            editor.set_loaded_result(loaded_result);
            editor
        });
        self.filter_editor_subscription =
            Some(cx.subscribe(&editor, |this, _, event, cx| match event {
                FilterEditorEvent::Apply(group, whole_source) => {
                    match group.to_filter(&this.filter_columns()) {
                        Ok(_) => {
                            this.visual_filters = group.clone();
                            this.request.whole_source = *whole_source;
                            // Keep the editor, its values and focus while applying.
                            this.execute(cx);
                        }
                        Err(error) => this.fail(error, cx),
                    }
                }
                FilterEditorEvent::Cancel => {
                    this.filter_sidebar_open = false;
                    cx.notify();
                }
            }));
        self.filter_editor_observer = Some(cx.observe(&editor, |_, _, cx| cx.notify()));
        self.filter_editor = Some(editor);
        cx.notify();
    }

    /// Preserve an existing draft across ordinary reloads. Rebuild only when
    /// the selected schema changed; unused hidden editors remain unallocated.
    pub(super) fn reconcile_filter_editor(&mut self, cx: &mut Context<Self>) {
        let columns = self.filter_columns();
        if self
            .filter_editor
            .as_ref()
            .is_some_and(|editor| !editor.read(cx).matches_columns(&columns))
        {
            self.sync_filter_editor(cx);
        }
    }

    /// Clear applied rules and draft; the main action chooses when to execute.
    pub(super) fn clear_filter_draft(&mut self, cx: &mut Context<Self>) {
        self.request.whole_source = false;
        self.visual_filters = FilterGroup::default();
        self.sync_filter_editor(cx);
    }

    pub(super) fn use_loaded_filter_scope(&mut self, cx: &mut Context<Self>) {
        self.request.whole_source = false;
        if let Some(editor) = &self.filter_editor {
            editor.update(cx, |editor, cx| editor.set_whole_source(false, cx));
        }
    }

    pub(super) fn toggle_filter_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.csv_browse.is_some() {
            self.choose_browse_range(BrowseCommand::Filter, cx);
            return;
        }
        if self.selecting_initial_fields {
            return;
        }
        self.filter_sidebar_open = !self.filter_sidebar_open;
        if self.filter_sidebar_open {
            self.close_statistics_sidebar(cx);
            self.close_fields_sidebar();
            self.close_preview(cx);
            if self.filter_editor.is_none() {
                self.sync_filter_editor(cx);
            }
            if let Some(editor) = &self.filter_editor {
                editor.read(cx).focus_first(window, cx);
            }
        } else {
            self.focus.focus(window);
        }
        cx.notify();
    }

    pub(super) fn render_filter_sidebar(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if !self.filter_sidebar_open {
            return div().into_any_element();
        }
        let c = Colors::new(self.preferences.dark);
        let draft_pending = self.filter_editor.as_ref().is_some_and(|editor| {
            editor
                .read(cx)
                .has_pending_changes(&self.visual_filters, self.request.whole_source, cx)
        });
        let query_pending = self.visual_filters != self.displayed_filters
            || self.request.whole_source != self.displayed_request.whole_source;
        div()
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
                    .items_center()
                    .gap_2()
                    .p_3()
                    .border_b_1()
                    .border_color(rgb(c.border))
                    .child(icons::glyph(Icon::Filter, 16., self.preferences.dark))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(t("Filters")),
                    )
                    .when(draft_pending || query_pending, |header| {
                        header.child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(10.))
                                .text_color(rgb(c.warning))
                                .child(t("Not applied")),
                        )
                    })
                    .child(
                        controls::icon_button("filter-sidebar-close", c, true)
                            .child(icons::glyph(Icon::Close, 14., self.preferences.dark))
                            .tooltip(icons::tooltip(t("Close"), self.preferences.dark))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.filter_sidebar_open = false;
                                this.focus.focus(window);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .p_3()
                    .when_some(self.filter_editor.clone(), |body, editor| {
                        body.child(editor)
                    }),
            )
            .into_any_element()
    }
}
