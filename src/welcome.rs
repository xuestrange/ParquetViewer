//! Start page for windows without a loaded result.
use super::*;

impl Viewer {
    pub(super) fn render_welcome(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let c = Colors::new(self.preferences.dark);
        let can_open = self.busy.is_none() && self.panel.is_none();
        let parquet_action = self.welcome_action(
            "welcome-parquet",
            "Open Parquet File…",
            OpenDialog::ParquetFiles,
            can_open,
            cx,
        );
        let text_action = self.welcome_action(
            "welcome-text",
            "Import Text File…",
            OpenDialog::TextFile,
            can_open,
            cx,
        );

        div()
            .size_full()
            .flex()
            .flex_col()
            .justify_center()
            .bg(rgb(c.bg))
            .child(
                div()
                    .id("welcome")
                    .w_full()
                    .min_h_0()
                    .max_h_full()
                    .overflow_y_scroll()
                    .child(
                        div().px(px(32.)).py(px(28.)).flex().flex_col().items_center().child(
                            div()
                                .w_full()
                                .max_w(px(1060.))
                                .flex()
                                .flex_col()
                                .gap_4()
                                .child(
                                    div()
                                        .flex()
                                        .flex_wrap()
                                        .gap_4()
                                        .child(self.welcome_source_card(
                                            Icon::Table,
                                            c.memory_bg,
                                            "Parquet",
                                            "Browse Parquet files and inspect nested columns.",
                                            parquet_action,
                                            c,
                                        ))
                                        .child(self.welcome_source_card(
                                            Icon::Code,
                                            c.accent_soft,
                                            "Delimited Text",
                                            "Adjust the separator, header, and column types before importing CSV, TSV, TXT, and other delimited files.",
                                            text_action,
                                            c,
                                        )),
                                )
                                .child(self.welcome_drop_target(c, can_open, cx))
                                .when(!self.preferences.recent.is_empty(), |body| {
                                    body.child(self.welcome_recent(c, can_open, cx))
                                }),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn welcome_drop_target(
        &self,
        c: Colors,
        can_open: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id("welcome-drop")
            .w_full()
            .min_h(px(172.))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .p(px(20.))
            .rounded(px(12.))
            .border_1()
            .border_dashed()
            .border_color(rgb(c.accent))
            .bg(rgb(c.surface))
            .text_center()
            .when(can_open, |target| {
                target
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(c.accent_soft)))
                    .drag_over::<ExternalPaths>(move |style, _, _, _| {
                        style.bg(rgb(c.accent_soft)).border_color(rgb(c.primary))
                    })
                    .on_click(cx.listener(|this, event: &gpui::ClickEvent, window, cx| {
                        if event.click_count() == 2 {
                            this.focus.focus(window);
                            this.open_dialog(OpenDialog::SingleFile, cx);
                        }
                    }))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                        if let [path] = paths.paths() {
                            this.open_single_file(path.clone(), cx);
                        } else {
                            this.open(paths.paths().to_vec(), cx);
                        }
                    }))
            })
            .child(
                div()
                    .size(px(38.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.))
                    .bg(rgb(c.accent_soft))
                    .child(icons::glyph(Icon::Import, 21., self.preferences.dark)),
            )
            .child(
                div()
                    .text_size(px(22.))
                    .line_height(px(28.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(t("Drop files here to open")),
            )
    }

    fn welcome_action(
        &self,
        id: &'static str,
        label: &'static str,
        kind: OpenDialog,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let dark = self.preferences.dark;
        let c = Colors::new(dark);
        let icon = match kind {
            OpenDialog::TextFile => Icon::Import,
            _ => Icon::Open,
        };
        controls::button(id, c, ButtonStyle::Standard, enabled)
            .when(enabled, |button| {
                button.on_click(cx.listener(move |this, _, window, cx| {
                    this.focus.focus(window);
                    this.open_dialog(kind, cx);
                }))
            })
            .child(icons::glyph(icon, 15., dark))
            .child(t(label))
    }

    fn welcome_source_card(
        &self,
        icon: Icon,
        icon_bg: u32,
        title: &'static str,
        detail: &'static str,
        action: gpui::Stateful<gpui::Div>,
        c: Colors,
    ) -> gpui::Div {
        div()
            .w(px(348.))
            .max_w_full()
            .min_w_0()
            .min_h(px(176.))
            .flex_grow()
            .flex()
            .flex_col()
            .gap_3()
            .p(px(20.))
            .rounded(px(12.))
            .border_1()
            .border_color(rgb(c.border))
            .bg(rgb(c.surface))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .size(px(40.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(7.))
                            .bg(rgb(icon_bg))
                            .child(icons::glyph(icon, 18., self.preferences.dark)),
                    )
                    .child(
                        div()
                            .text_size(px(22.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(t(title)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .whitespace_normal()
                    .text_size(px(13.))
                    .line_height(px(19.))
                    .text_color(rgb(c.muted))
                    .child(t(detail)),
            )
            .child(div().flex().child(action))
    }

    fn welcome_recent(&self, c: Colors, can_open: bool, cx: &mut Context<Self>) -> gpui::Div {
        let dark = self.preferences.dark;
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_size(px(14.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(t("Recent Files")),
            )
            .child(
                div().overflow_hidden().flex().flex_wrap().gap_2().children(
                    self.preferences
                        .recent
                        .iter()
                        .take(5)
                        .enumerate()
                        .map(|(index, path)| {
                            let target = path.clone();
                            let name = path
                                .file_name()
                                .unwrap_or(path.as_os_str())
                                .to_string_lossy()
                                .into_owned();
                            let parent = path
                                .parent()
                                .map(|parent| {
                                    parent
                                        .file_name()
                                        .unwrap_or(parent.as_os_str())
                                        .to_string_lossy()
                                        .into_owned()
                                })
                                .unwrap_or_default();
                            // Choose the glyph from the filename without probing the file.
                            let suffix = name.to_ascii_lowercase();
                            let (icon, icon_bg) = if has_delimited_text_extension(path) {
                                (Icon::Code, c.accent_soft)
                            } else if [".parquet", ".parquet.gz", ".parquet.gzip"]
                                .iter()
                                .any(|extension| suffix.ends_with(extension))
                            {
                                (Icon::Table, c.memory_bg)
                            } else {
                                (Icon::Open, c.header)
                            };
                            div()
                                .id(("recent", index))
                                .w(gpui::relative(0.49))
                                .max_w_full()
                                .h(px(41.))
                                .rounded(px(6.))
                                .bg(rgb(c.surface))
                                .border_1()
                                .border_color(rgb(c.border))
                                .flex()
                                .items_center()
                                .gap_3()
                                .px_3()
                                .when(can_open, |row| {
                                    row.cursor_pointer()
                                        .hover(|row| row.bg(rgb(c.hover)))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.focus.focus(window);
                                            this.open_single_file(target.clone(), cx);
                                        }))
                                })
                                .tooltip(icons::tooltip(path.display().to_string(), dark))
                                .child(
                                    div()
                                        .size(px(25.))
                                        .flex_shrink_0()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(5.))
                                        .bg(rgb(icon_bg))
                                        .child(icons::glyph(icon, 14., dark)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .max_w(gpui::relative(0.32))
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(11.))
                                        .text_color(rgb(c.muted))
                                        .child(parent),
                                )
                                .child(div().flex_shrink_0().text_color(rgb(c.muted)).child("›"))
                        }),
                ),
            )
    }
}
